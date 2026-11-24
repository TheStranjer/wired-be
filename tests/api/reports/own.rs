//! The reporter's own reads, `GET /api/v0/pleroma/reports` and its `{id}`
//! twin (WBE-475): a user lists and shows exactly the reports they filed,
//! as the seven-key entity with nothing of the moderators' side, behind
//! `read:reports`, with the cited statuses held to the reporter's own
//! visibility.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    report::{self, ReportState},
    status::Visibility,
    user::User,
};

use super::support::{filed, fixture, post};
use crate::common;

/// The route of the reporter's own reports.
const OWN: &str = "/api/v0/pleroma/reports";
/// The scopes of a reader who also files.
const READ_AND_FILE: &str = "read:reports write:reports";
/// Every key of the own-report entity, sorted.
const OWN_KEYS: [&str; 7] = [
    "account",
    "actor",
    "content",
    "created_at",
    "id",
    "state",
    "statuses",
];

/// Files a report of `reporter` (by `token`) about `target` saying
/// `comment`, citing `cited`, and answers the new report's id.
async fn file_about(
    app: &Router,
    token: &str,
    target: &User,
    comment: &str,
    cited: &[Uuid],
) -> String {
    let body = json!({
        "account_id": target.id.to_string(),
        "comment": comment,
        "status_ids": cited.iter().map(ToString::to_string).collect::<Vec<_>>(),
    });
    filed(app, token, body).await["id"]
        .as_str()
        .expect("the filing answers its id")
        .to_owned()
}

/// `GET {path}` as `token`.
async fn read(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// The ids of a `{reports, total}` answer, in order.
fn listed_ids(answer: &Value) -> Vec<String> {
    answer["reports"]
        .as_array()
        .expect("the reports array")
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
        .collect()
}

/// Two reporters and a target, with `a` filing two reports and `b` one.
struct Ledger {
    /// The app under test.
    app: Router,
    /// The first reporter and their `read:reports write:reports` token.
    a: (User, String),
    /// The second reporter and their token.
    b: (User, String),
    /// A's two report ids, oldest first.
    a_ids: Vec<String>,
    /// B's one report id.
    b_id: String,
}

/// Seeds the [`Ledger`] on `pool`.
async fn ledger(pool: &PgPool) -> Ledger {
    let app = common::test_app(pool.clone());
    let a = fixture(pool, "own_a", READ_AND_FILE).await;
    let b = fixture(pool, "own_b", READ_AND_FILE).await;
    let target = common::create_local_user(pool, "own_target").await;
    let first = file_about(&app, &a.1, &target, "first", &[]).await;
    let second = file_about(&app, &a.1, &target, "second", &[]).await;
    let b_id = file_about(&app, &b.1, &target, "b's", &[]).await;
    Ledger {
        app,
        a,
        b,
        a_ids: vec![first, second],
        b_id,
    }
}

/// A user lists exactly the reports they filed, newest first, with the
/// total of those alone.
#[tokio::test]
async fn a_user_lists_only_their_own_reports() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = ledger(&pool).await;

    let response = read(&world.app, &world.a.1, OWN).await;

    common::assert_answer(&response, StatusCode::OK, "A's own listing");
    let body = response.json_value();
    assert_eq!(body["total"], json!(2), "A filed two");
    let mut expected = world.a_ids.clone();
    expected.sort_unstable_by(|left, right| right.cmp(left));
    assert_eq!(listed_ids(&body), expected, "A's two, newest first");
    for entry in body["reports"].as_array().expect("the reports array") {
        assert_eq!(
            entry["actor"]["id"],
            json!(world.a.0.id.to_string()),
            "every entry's actor is the reader"
        );
    }
    let theirs = read(&world.app, &world.b.1, OWN).await.json_value();
    assert_eq!(theirs["total"], json!(1), "B filed one");
    assert_eq!(listed_ids(&theirs), vec![world.b_id.clone()], "and sees it");
}

/// The window: `page` counts from 1, `page_size` and `limit` size it, the
/// total ignores it, and unusable numbers fall back to the defaults.
#[tokio::test]
async fn the_listing_pages_by_page_and_page_size() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = ledger(&pool).await;
    let all = listed_ids(&read(&world.app, &world.a.1, OWN).await.json_value());

    let first = read(&world.app, &world.a.1, &format!("{OWN}?page_size=1")).await;
    let first = first.json_value();
    assert_eq!(listed_ids(&first), all[..1], "page one of size one");
    assert_eq!(first["total"], json!(2), "the total is the whole ledger");
    let second = read(&world.app, &world.a.1, &format!("{OWN}?page=2&limit=1")).await;
    assert_eq!(
        listed_ids(&second.json_value()),
        all[1..],
        "page two, sized by the `limit` synonym"
    );
    let past = read(&world.app, &world.a.1, &format!("{OWN}?page=3&page_size=1")).await;
    assert!(
        listed_ids(&past.json_value()).is_empty(),
        "a page past the end is empty"
    );
    let junk = read(
        &world.app,
        &world.a.1,
        &format!("{OWN}?page=x&page_size=-4"),
    )
    .await;
    assert_eq!(
        listed_ids(&junk.json_value()),
        all,
        "unusable numbers read as the defaults"
    );
}

/// Another account's report is the 404 `Not found`, a report of one's own is
/// the seven-key entity with nothing of the moderators' side, and a
/// malformed id is the same miss.
#[tokio::test]
async fn show_answers_only_the_readers_own_report() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = ledger(&pool).await;
    let mine = world.a_ids[0].clone();
    let stored = report::get(&pool, mine.parse().expect("the id parses"))
        .await
        .expect("the row reads")
        .expect("the row exists");
    let _closed = report::set_state(&pool, stored.id, ReportState::Closed, None)
        .await
        .expect("the fixture closes")
        .expect("the report exists");

    let foreign = read(&world.app, &world.a.1, &format!("{OWN}/{}", world.b_id)).await;
    common::assert_answer(&foreign, StatusCode::NOT_FOUND, "B's report read by A");
    common::assert_error(&foreign, "Not found", "B's report read by A");
    let junk = read(&world.app, &world.a.1, &format!("{OWN}/not-an-id")).await;
    common::assert_answer(&junk, StatusCode::NOT_FOUND, "a malformed id");
    let unknown = read(&world.app, &world.a.1, &format!("{OWN}/{}", Uuid::now_v7())).await;
    common::assert_answer(&unknown, StatusCode::NOT_FOUND, "an unknown id");

    let own = read(&world.app, &world.a.1, &format!("{OWN}/{mine}")).await;
    common::assert_answer(&own, StatusCode::OK, "A's own report");
    let body = own.json_value();
    assert_eq!(
        common::sorted_keys(&body),
        OWN_KEYS,
        "exactly the seven keys"
    );
    assert!(body.get("notes").is_none(), "no moderator notes");
    assert_eq!(body["id"], json!(mine), "the report's id");
    assert_eq!(body["content"], json!("first"), "the reporter's words");
    assert_eq!(body["state"], json!("closed"), "where it stands");
    assert_eq!(
        body["actor"]["id"],
        json!(world.a.0.id.to_string()),
        "the reporter is the actor"
    );
    assert_ne!(body["account"]["id"], body["actor"]["id"], "the target");
    assert_eq!(body["statuses"], json!([]), "nothing cited");
}

/// A token without `read:reports` is the 403 and no token the 401, on both
/// routes.
#[tokio::test]
async fn the_routes_demand_a_token_with_read_reports() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = ledger(&pool).await;
    let (_user, write_only) = fixture(&pool, "own_write_only", "write:reports").await;
    let path = format!("{OWN}/{}", world.a_ids[0]);

    for route in [OWN, path.as_str()] {
        let refused = read(&world.app, &write_only, route).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            "a token without read:reports",
        );
        let anonymous = common::TestRequest::get(route).send(&world.app).await;
        common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token at all");
    }
}

/// A cited status that later becomes invisible to the reporter drops out of
/// `statuses` on both the list and the show, while a visible one stays.
#[tokio::test]
async fn a_cited_status_gone_invisible_is_omitted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_reporter, token) = fixture(&pool, "own_viewer", READ_AND_FILE).await;
    let target = common::create_local_user(&pool, "own_poster").await;
    let kept = post(&pool, &target, "stays visible", Visibility::Public).await;
    let hidden = post(&pool, &target, "goes private", Visibility::Public).await;
    let id = file_about(&app, &token, &target, "two cited", &[kept, hidden]).await;
    let cited = read(&app, &token, &format!("{OWN}/{id}"))
        .await
        .json_value();
    assert_eq!(
        cited["statuses"].as_array().map(Vec::len),
        Some(2),
        "both citations show while visible"
    );

    let _private = sqlx::query("UPDATE statuses SET visibility = 'private' WHERE id = $1")
        .bind(hidden)
        .execute(&pool)
        .await
        .expect("the fixture hides the status");

    let shown = read(&app, &token, &format!("{OWN}/{id}"))
        .await
        .json_value();
    let remaining: Vec<&str> = shown["statuses"]
        .as_array()
        .expect("the statuses array")
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    assert_eq!(remaining, vec![kept.to_string()], "only the visible one");
    let listed = read(&app, &token, OWN).await.json_value();
    assert_eq!(
        listed["reports"][0]["statuses"].as_array().map(Vec::len),
        Some(1),
        "the list omits it too"
    );
}
