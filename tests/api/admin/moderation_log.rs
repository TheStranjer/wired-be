//! The moderation-log listing and the visibility `stats` (WBE-194): the
//! `{items, total}` newest-first Pleroma shape, the actor and message
//! filters, the page and time windows with their caps, the validation
//! refusals, and the counters that never see a reblog or a tombstone.
//! The entries are seeded through the model helpers, and time is made
//! controllable by back-dating `inserted_at` rather than sleeping.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    moderation_log,
    status::{self, NewStatus, Visibility},
    user::User,
};

use super::support::{LOG, STATS, WINDOW_HOURS_BACK_END, WINDOW_HOURS_BACK_START, admin};
use crate::common::{self, TestRequest};

/// Moves log entry `id`'s `inserted_at` `days` into the past, so the
/// listing tests command the clock.
async fn age_entry(pool: &PgPool, id: Uuid, days: i32) {
    let _moved = sqlx::query(
        "UPDATE moderation_log SET inserted_at = now() - make_interval(days => $2) WHERE id = $1",
    )
    .bind(id)
    .bind(days)
    .execute(pool)
    .await
    .expect("the backdate runs");
}

/// The three seeded entries' ids and acting admins: `deactivate` by
/// "one" three days ago, `activate` by "two" two days ago, `delete` by
/// "one" one day ago.
#[derive(Debug)]
struct SeededLog {
    /// The admin of the two-day-old entry.
    actor_two: User,
    /// The oldest entry, three days back.
    oldest: Uuid,
    /// The middle entry, two days back.
    middle: Uuid,
    /// The newest entry, one day back.
    newest: Uuid,
}

/// Seeds the three-entry log through the model helpers, back-dated for a
/// deterministic newest-first order.
async fn seed_log(pool: &PgPool) -> SeededLog {
    let (one, _) = admin(pool, "one", "admin:read").await;
    let (two, _) = admin(pool, "two", "admin:read").await;
    let subject = common::create_local_user(pool, "subject").await;
    let oldest = moderation_log::deactivate(pool, &one, std::slice::from_ref(&subject))
        .await
        .expect("records");
    let middle = moderation_log::activate(pool, &two, std::slice::from_ref(&subject))
        .await
        .expect("records");
    let newest = moderation_log::delete(pool, &one, std::slice::from_ref(&subject))
        .await
        .expect("records");
    age_entry(pool, oldest.id, 3).await;
    age_entry(pool, middle.id, 2).await;
    age_entry(pool, newest.id, 1).await;
    SeededLog {
        actor_two: two,
        oldest: oldest.id,
        middle: middle.id,
        newest: newest.id,
    }
}

/// The listing shape: `{items, total}`, newest first, each item the
/// Pleroma `{id, data, time, message}` entry with the actor map inside
/// `data`.
#[tokio::test]
async fn moderation_log_lists_newest_first_in_pleroma_shape() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let seeded = seed_log(&pool).await;
    let (_, token) = admin(&pool, "viewer", "admin:read").await;

    let response = TestRequest::get(LOG).bearer(&token).send(&app).await;
    common::assert_answer(&response, StatusCode::OK, "the full log reads");
    let body = response.json_value();
    assert_eq!(body["total"], 3, "three entries");
    assert_eq!(
        body["items"]
            .as_array()
            .expect("items array")
            .iter()
            .map(|item| item["id"].as_str().expect("id text"))
            .collect::<Vec<&str>>(),
        vec![
            seeded.newest.to_string().as_str(),
            seeded.middle.to_string().as_str(),
            seeded.oldest.to_string().as_str()
        ],
        "newest first"
    );

    let first = &body["items"].as_array().expect("items")[0];
    assert_eq!(
        first["message"], "@one deleted users: @subject",
        "the stored sentence, top level and in data"
    );
    assert_eq!(
        first["data"]["message"], first["message"],
        "one message twice"
    );
    assert_eq!(
        first["data"]["action"], "delete",
        "the action in the data map"
    );
    assert_eq!(first["data"]["actor"]["nickname"], "one", "the actor map");
    assert_eq!(
        first["data"]["actor"]["type"], "user",
        "Pleroma stamps the actor type"
    );
    assert!(
        first["data"]["actor"]["id"].as_str().is_some(),
        "the actor id rides as a JSON string: {}",
        first["data"]["actor"]["id"]
    );
    assert!(
        first["time"].as_i64().expect("unix seconds") > 0,
        "time is unix seconds"
    );
}

/// The listing filters: `user_id` selects one actor's entries, `search`
/// matches the message case-insensitively.
#[tokio::test]
async fn moderation_log_filters_by_actor_and_message() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let seeded = seed_log(&pool).await;
    let (_, token) = admin(&pool, "filterer", "admin:read").await;

    let by_actor = TestRequest::get(&format!("{LOG}?user_id={}", seeded.actor_two.id))
        .bearer(&token)
        .send(&app)
        .await;
    let body = by_actor.json_value();
    assert_eq!(body["total"], 1, "actor two has one entry");
    assert_eq!(
        body["items"][0]["id"],
        seeded.middle.to_string(),
        "and it is his activate"
    );

    let searched = TestRequest::get(&format!("{LOG}?search=DEACTIV"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        searched.json_value()["items"][0]["id"],
        seeded.oldest.to_string(),
        "the search is a case-insensitive substring of the message"
    );

    let no_match = TestRequest::get(&format!("{LOG}?search=relay"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        no_match.json_value()["total"],
        0,
        "a word nothing says matches nothing"
    );
    assert_eq!(
        no_match.json_value()["items"],
        Value::Array(Vec::new()),
        "and the empty page answers an empty items array"
    );
}

/// The page window and the time window: `page`/`page_size` slice the
/// newest-first order with the total ignoring the window, `start_date` and
/// `end_date` bound it inclusively, and `page_size` above the bound is
/// capped rather than refused.
#[tokio::test]
async fn moderation_log_pages_and_windows_by_time() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let seeded = seed_log(&pool).await;
    let (_, token) = admin(&pool, "pager", "admin:read").await;

    let paged = TestRequest::get(&format!("{LOG}?page=2&page_size=2"))
        .bearer(&token)
        .send(&app)
        .await;
    let body = paged.json_value();
    assert_eq!(body["total"], 3, "the total ignores the window");
    assert_eq!(
        body["items"].as_array().expect("items").len(),
        1,
        "one row on page two"
    );
    assert_eq!(
        body["items"][0]["id"],
        seeded.oldest.to_string(),
        "the oldest"
    );

    let window = TestRequest::get(&format!(
        "{LOG}?start_date={}&end_date={}",
        hours_ago(WINDOW_HOURS_BACK_START),
        hours_ago(WINDOW_HOURS_BACK_END)
    ))
    .bearer(&token)
    .send(&app)
    .await;
    let w = window.json_value();
    assert_eq!(
        w["total"], 1,
        "only the two-day-old entry stands in the window"
    );
    assert_eq!(
        w["items"][0]["id"],
        seeded.middle.to_string(),
        "and it is named"
    );

    let capped = TestRequest::get(&format!("{LOG}?page_size=5000"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        capped.json_value()["items"]
            .as_array()
            .expect("items")
            .len(),
        3,
        "an oversized page comes back capped, not refused"
    );

    let junk_page = TestRequest::get(&format!("{LOG}?page=abc&page_size=0"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        junk_page.json_value()["total"],
        3,
        "unusable numbers fall to the defaults"
    );
}

/// A full ISO `Z`-suffixed timestamp `hours` before now, for the inclusive
/// window. `Z` (not `+00:00`) so the query string carries no `+` that
/// would decode to a space.
fn hours_ago(hours: i64) -> String {
    chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::hours(hours))
        .expect("the bound is representable")
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Bad filter values are refused: an unparsable `user_id` is the 400
/// `Invalid id`, a broken `start_date` is a 422 naming the parameter.
#[tokio::test]
async fn moderation_log_validation_errors() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "checker", "admin:read").await;

    let bad_id = TestRequest::get(&format!("{LOG}?user_id=not-a-uuid"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[bad_id],
        StatusCode::BAD_REQUEST,
        "Invalid id",
        "an unparsable user_id",
    );

    let bad_date = TestRequest::get(&format!("{LOG}?start_date=31/01/2026"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        bad_date.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a broken start_date is a 422: {}",
        bad_date.text()
    );
    common::assert_error(
        &bad_date,
        "start_date must be an ISO 8601 date or timestamp, not 31/01/2026",
        "a broken start_date",
    );
}

/// The `stats` counters: local live non-reblog statuses by visibility with
/// no `instance`, one domain's own with it, and an unknown domain all
/// zeros.
#[tokio::test]
async fn stats_counts_by_visibility() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;

    let _public = common::create_status(&pool, &alice, "hi", Visibility::Public).await;
    let _unlisted = common::create_status(&pool, &alice, "hi", Visibility::Unlisted).await;
    let _private = common::create_status(&pool, &alice, "hi", Visibility::Private).await;
    let _direct = common::create_status(&pool, &alice, "hi", Visibility::Direct).await;
    let _remote = common::create_status(&pool, &mallory, "hi", Visibility::Public).await;
    seed_excluded(&pool, &alice).await;

    let (_, token) = admin(&pool, "counter", "admin:read").await;

    let local = TestRequest::get(STATS).bearer(&token).send(&app).await;
    common::assert_answer(&local, StatusCode::OK, "the local stats read");
    assert_eq!(
        local.json_value()["status_visibility"],
        json!({"public": 2, "unlisted": 1, "private": 1, "direct": 1}),
        "alice's live posts, the boost and the tombstone excluded"
    );

    let by_instance = TestRequest::get(&format!("{STATS}?instance=remote.example"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        by_instance.json_value()["status_visibility"],
        json!({"public": 1, "unlisted": 0, "private": 0, "direct": 0}),
        "mallory's domain alone"
    );

    let unknown = TestRequest::get(&format!("{STATS}?instance=nowhere.example"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        unknown.json_value()["status_visibility"],
        json!({"public": 0, "unlisted": 0, "private": 0, "direct": 0}),
        "a domain no account came from is all zeros"
    );
}

/// Two statuses the count must never see: a reblog row of alice's and a
/// soft-deleted one.
async fn seed_excluded(pool: &PgPool, alice: &User) {
    let original = common::create_status(pool, alice, "to boost", Visibility::Public).await;
    let _reblog = status::insert(
        pool,
        &common::test_config().base_url,
        &NewStatus {
            reblog_of_id: Some(original.id),
            ..NewStatus::local(alice, "")
        },
    )
    .await
    .expect("a reblog inserts");
    let doomed = common::create_status(pool, alice, "doomed", Visibility::Public).await;
    let _tombstone = status::soft_delete(pool, doomed.id)
        .await
        .expect("the soft delete runs");
}
