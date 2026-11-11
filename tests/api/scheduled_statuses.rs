//! The `scheduled_at` arm of `POST /api/v1/statuses` over HTTP (WBE-99):
//! a future instant parks the whole create instead of writing a status.
//!
//! The arm runs the create's own body checks first — the empty text, the
//! unknown scope, the `expires_in` floor and the shared reply 404 are the
//! same answers they are on a plain post — and then the schedule's own
//! gates: `scheduled_at` must sit at least `MIN_SCHEDULED_SECS` (300 by
//! default, the refusal naming the configured minutes) ahead of now, the
//! account may hold at most `MAX_SCHEDULED_PER_USER` pending rows and at
//! most `MAX_SCHEDULED_PER_USER_PER_DAY` within the UTC day of the fire,
//! and every `media_ids` entry must be the poster's own upload no status
//! has claimed and no pending schedule reserves — unknown, foreign,
//! attached and already-reserved ids all one 422 `Media not found`.
//!
//! What passes becomes exactly one `scheduled_statuses` row — the
//! normalised request as its params jsonb (trimmed text, resolved
//! defaults, deduplicated id strings, and the `idempotency` and
//! `application_id` echo keys) — plus one
//! `scheduled_statuses.publish` job on the `scheduled` queue due at the
//! submitted instant keyed `sched:<id>`. Nothing else happens: no
//! `statuses` row, no expiration job, no claimed idempotency key, and no
//! `media_attachments` column moves — the upload is reserved purely by
//! living in the pending row's params. The answer is 200 with the
//! `ScheduledStatus` entity (the params echoed as `text`, the
//! `scheduled_at` and `poll` keys pinned to `null`), its
//! `media_attachments` read back in the order the body submitted them.
//! Form and query bodies schedule the same post as JSON, the schedule
//! ignores an `Idempotency-Key` beyond echoing it, and the gate is
//! `write:statuses` like the create itself (401 anonymous, 403 without
//! the scope).

#![cfg(test)]

use std::collections::BTreeSet;

use axum::{Router, http::StatusCode};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    jobs::scheduled_statuses::{KIND_PUBLISH, job_args, unique_key},
    models::{
        media_attachment::{self, NewMediaAttachment},
        scheduled_status::{self, NewScheduledStatus},
        status::Visibility,
        user::User,
    },
    util::time::{mastodon_time, parse_time},
};

use crate::common;

/// The endpoint under test.
const STATUSES: &str = "/api/v1/statuses";
/// The scope the create's gate — and this arm — asks the token for.
const WRITE: &str = "write:statuses";
/// The 401 body `AuthUser` renders when the request carries no token.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body the scope gate renders for a token without [`WRITE`].
const NO_WRITE: &str = "Insufficient permissions: write:statuses.";
/// The 422 body for text that trims to nothing over no attachments.
const EMPTY_STATUS: &str = "Cannot post an empty status without attachments";
/// The shared 404 body, for the parent probes.
const NOT_FOUND: &str = "Record not found";
/// The 422 body for an id that is no id, and for a media id the poster
/// may not reserve.
const MEDIA_NOT_FOUND: &str = "Media not found";
/// The 422 body for an account at either schedule limit.
const TOO_MANY: &str = "Too many scheduled statuses";
/// The 422 body for `scheduled_at` text that is no RFC3339 instant.
const INVALID_SCHEDULED_AT: &str = "Invalid scheduled_at";
/// The 422 wording of the stock test instance's 600-second expiration floor.
const SHORT_EXPIRY: &str = "expires_in must be at least 600 seconds";
/// The id of a status that is not stored here, for the parent probe.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

/// The Mastodon text of the instant `minutes` from now.
fn in_minutes(minutes: i64) -> String {
    mastodon_time(&(Utc::now() + TimeDelta::minutes(minutes)))
}

/// A local user named `nickname` with a `write:statuses` bearer token.
async fn writer(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, WRITE).await;
    (created, token)
}

/// `POST /api/v1/statuses` with `body` as JSON, as `token`.
async fn schedule(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The keys of a JSON object, sorted: for pinning a shape key-for-key.
fn object_keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("the value is a JSON object")
        .keys()
        .cloned()
        .collect()
}

/// The expected key set built from the names a shape must carry exactly.
fn keys(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// How many schedules the table holds (every row is pending: the fire and
/// the cancel both delete).
async fn scheduled_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM scheduled_statuses")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// How many `statuses` rows the database holds at all.
async fn post_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// How many `idempotency_keys` rows the database holds.
async fn key_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM idempotency_keys")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// The `jobs` rows of `kind`, in insertion order.
async fn jobs_of_kind(pool: &PgPool, kind: &str) -> Vec<wired_be::models::job::Job> {
    sqlx::query_as::<_, wired_be::models::job::Job>(
        "SELECT * FROM jobs WHERE kind = $1 ORDER BY id",
    )
    .bind(kind)
    .fetch_all(pool)
    .await
    .expect("the job rows read")
}

/// The schedule stored under `id`.
async fn stored_schedule(pool: &PgPool, id: Uuid) -> scheduled_status::ScheduledStatus {
    scheduled_status::get_by_id(pool, id)
        .await
        .expect("the schedule reads")
        .expect("the schedule is stored")
}

/// An unposted `image/png` upload by `owner`, as a `media_ids` entry names it.
async fn upload(pool: &PgPool, owner: &User) -> media_attachment::MediaAttachment {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(owner.id),
            url: format!("{TEST_BASE_URL}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("a fixture upload inserts")
}

/// A pending schedule for `who`, due `when`, seeded straight through the
/// model — past the endpoint's gates by construction, exactly what a
/// schedule the endpoint stored would look like to the limit counts.
async fn seed_schedule(pool: &PgPool, who: &User, when: DateTime<Utc>) {
    let row = scheduled_status::insert(
        pool,
        &NewScheduledStatus {
            user_id: who.id,
            scheduled_at: when,
            params: json!({ "status": "seeded" }),
        },
    )
    .await
    .expect("the seed schedule inserts");
    assert_eq!(row.user_id, who.id, "the seed row is its account's");
}

/// The whole answer of a scheduled create, pinned key for key: the
/// entity's own keys, the fire moment in the Mastodon format, the empty
/// attachments of an attachment-free body, and the complete params echo —
/// the request re-named the way the API spells it, with `poll` and
/// `scheduled_at` fixed to `null` and the two echo keys filled in.
fn assert_entity_answers_the_request(entity: &Value, at: &str, application_id: &str) {
    assert_eq!(
        object_keys(entity),
        keys(&["id", "scheduled_at", "params", "media_attachments"]),
        "the entity's own keys, no more"
    );
    assert_eq!(
        entity["scheduled_at"], at,
        "the fire moment in the Mastodon format"
    );
    assert_eq!(
        entity["media_attachments"],
        json!([]),
        "a post with no attachments"
    );
    assert_eq!(
        object_keys(&entity["params"]),
        keys(&[
            "text",
            "visibility",
            "sensitive",
            "spoiler_text",
            "in_reply_to_id",
            "media_ids",
            "poll",
            "idempotency",
            "scheduled_at",
            "application_id",
            "language",
            "content_type",
            "expires_in",
        ]),
        "the params' keys, pinned one for one"
    );
    assert_eq!(
        entity["params"],
        json!({
            "text": "scheduled hello",
            "visibility": "unlisted",
            "sensitive": true,
            "spoiler_text": "careful",
            "in_reply_to_id": null,
            "media_ids": [],
            "poll": null,
            "idempotency": "K-1",
            "scheduled_at": null,
            "application_id": application_id,
            "language": "eo",
            "content_type": "text/plain",
            "expires_in": 3600,
        }),
        "the whole params echo, value for value"
    );
}

/// The stored row: its account, its exact due time, and the normalised
/// request as its params jsonb — trimmed text, every resolution the create
/// would have made written back, no `preview` and no `scheduled_at`, and
/// the two echo keys the endpoint stores when the request had them.
async fn assert_row_parks_the_request(
    pool: &PgPool,
    id: Uuid,
    owner: &User,
    at: &str,
    application_id: &str,
) {
    let row = stored_schedule(pool, id).await;
    assert_eq!(row.user_id, owner.id, "scheduled for its owner");
    assert_eq!(
        row.scheduled_at,
        parse_time(at).expect("the fixture instant is a timestamp"),
        "due at the submitted instant"
    );
    assert_eq!(
        row.params,
        json!({
            "status": "scheduled hello",
            "spoiler_text": "careful",
            "sensitive": true,
            "visibility": "unlisted",
            "language": "eo",
            "content_type": "text/plain",
            "expires_in": 3600,
            "in_reply_to_id": null,
            "in_reply_to_conversation_id": null,
            "media_ids": [],
            "idempotency": "K-1",
            "application_id": application_id,
        }),
        "the normalised request keys, and no other shape"
    );
}

/// The write promise of a scheduled create, checked whole: no status row,
/// no claimed replay key, no expiration job — and exactly one
/// `scheduled_statuses.publish` job on the `scheduled` queue, due at the
/// row's instant, keyed `sched:<id>`.
async fn assert_only_the_fire_is_queued(pool: &PgPool, row: &scheduled_status::ScheduledStatus) {
    assert_eq!(post_count(pool).await, 0, "no status row");
    assert_eq!(
        key_count(pool).await,
        0,
        "the idempotency key was echoed, not claimed"
    );
    assert!(
        jobs_of_kind(pool, "statuses.expire").await.is_empty(),
        "a schedule is not a post: no expiration job"
    );
    let jobs = jobs_of_kind(pool, KIND_PUBLISH).await;
    assert_eq!(jobs.len(), 1, "exactly one fire for the schedule");
    let job = &jobs[0];
    assert_eq!(job.queue, "scheduled", "on the scheduled queue");
    assert_eq!(job.args, job_args(row.id), "the args name exactly the row");
    assert_eq!(
        job.run_at, row.scheduled_at,
        "due at scheduled_at, to the microsecond"
    );
    assert_eq!(
        job.unique_key.as_deref(),
        Some(unique_key(row.id).as_str()),
        "keyed for dedupe as the queue spells it"
    );
}

/// A future `scheduled_at` answers 200 with the whole `ScheduledStatus`
/// entity pinned key for key and stores the normalised request — and
/// writes nothing else: no status, no key claim, and one publish job due
/// at the submitted instant.
#[tokio::test]
async fn a_scheduled_post_answers_the_entity_and_posts_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let oauth_app = common::create_app(&pool, WRITE).await;
    let token = common::create_token(&pool, &alice, &oauth_app, WRITE)
        .await
        .token;
    let app = common::test_app(pool.clone());
    let application_id = oauth_app.id.to_string();

    let at = in_minutes(60);
    let response = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .header("idempotency-key", "K-1")
        .json(&json!({
            "status": "  scheduled hello  ",
            "visibility": "unlisted",
            "sensitive": true,
            "spoiler_text": "careful",
            "language": "eo",
            "content_type": "text/plain",
            "expires_in": 3600,
            "scheduled_at": at,
        }))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "a future scheduled_at");

    let entity = response.json_value();
    assert_entity_answers_the_request(&entity, &at, &application_id);
    let id = Uuid::parse_str(entity["id"].as_str().expect("the entity carries its id"))
        .expect("the id is a uuid");
    assert_row_parks_the_request(&pool, id, &alice, &at, &application_id).await;
    assert_only_the_fire_is_queued(&pool, &stored_schedule(&pool, id).await).await;
}

/// A past instant and one inside the window are the same refusal, whose
/// wording names the instance's configured minutes — the default 300
/// reads "5 minutes", a `min_scheduled_secs` of 600 reads "10 minutes" —
/// and neither stores a schedule or posts anything.
#[tokio::test]
async fn a_moment_inside_the_window_is_refused_with_the_configured_minutes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    for (when, who) in [
        (in_minutes(-60), "an hour ago"),
        (in_minutes(1), "one minute out"),
        (in_minutes(0), "right now"),
    ] {
        let refused = schedule(
            &app,
            &token,
            json!({"status": "too soon", "scheduled_at": when}),
        )
        .await;
        common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, who);
        common::assert_error(
            &refused,
            "Scheduled time must be at least 5 minutes in the future",
            &format!("{who} on the default window"),
        );
    }

    let mut config = common::test_config();
    config.min_scheduled_secs = 600;
    let strict = common::test_app_with_config(pool.clone(), config);
    let refused = schedule(
        &strict,
        &token,
        json!({"status": "still too soon", "scheduled_at": in_minutes(5)}),
    )
    .await;
    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a minute inside the widened window",
    );
    common::assert_error(
        &refused,
        "Scheduled time must be at least 10 minutes in the future",
        "the refusal tracks the setting",
    );

    assert_eq!(
        scheduled_count(&pool).await,
        0,
        "not one window refusal stored a schedule"
    );
    assert_eq!(post_count(&pool).await, 0, "and none posted");
}

/// `scheduled_at` text that is no RFC3339 instant is 422
/// `Invalid scheduled_at`, before any of the schedule's other gates run,
/// and stores nothing.
#[tokio::test]
async fn unparsable_scheduled_at_text_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    for text in ["not-a-date", "2026-02-30", "09:00:00", "1706722496"] {
        let refused = schedule(&app, &token, json!({"status": "hi", "scheduled_at": text})).await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("`{text}`"),
        );
        common::assert_error(&refused, INVALID_SCHEDULED_AT, &format!("`{text}`"));
    }

    assert_eq!(scheduled_count(&pool).await, 0);
    assert_eq!(post_count(&pool).await, 0);
}

/// At `max_scheduled_per_user` pending rows the next schedule is 422
/// `Too many scheduled statuses` — while the same request from an
/// account under the cap still stands (the count is each account's own).
#[tokio::test]
async fn the_total_limit_refuses_the_next_schedule() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let (bob, bob_token) = writer(&pool, "bob").await;

    let mut config = common::test_config();
    config.max_scheduled_per_user = 2;
    config.max_scheduled_per_user_per_day = 100;
    let app = common::test_app_with_config(pool.clone(), config);

    let due = Utc::now() + TimeDelta::hours(1);
    seed_schedule(&pool, &bob, due).await;
    seed_schedule(&pool, &bob, due + TimeDelta::minutes(5)).await;

    let refused = schedule(
        &app,
        &bob_token,
        json!({"status": "one too many", "scheduled_at": in_minutes(60)}),
    )
    .await;
    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "at the total cap",
    );
    common::assert_error(&refused, TOO_MANY, "the total limit's refusal");

    let accepted = schedule(
        &app,
        &token,
        json!({"status": "nobody else is at the cap", "scheduled_at": in_minutes(60)}),
    )
    .await;
    common::assert_answer(&accepted, StatusCode::OK, "an account under the cap");

    assert_eq!(
        scheduled_count(&pool).await,
        3,
        "the refusal stored nothing; the acceptance stored one"
    );
    assert_eq!(post_count(&pool).await, 0, "and nothing posted");
}

/// At `max_scheduled_per_user_per_day` pending rows *within the UTC day
/// of the fire moment* the schedule is 422 `Too many scheduled statuses`,
/// while the day beside it stands free.
#[tokio::test]
async fn the_per_day_limit_refuses_the_same_utc_day_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = writer(&pool, "alice").await;

    let mut config = common::test_config();
    config.max_scheduled_per_user = 100;
    config.max_scheduled_per_user_per_day = 2;
    let app = common::test_app_with_config(pool.clone(), config);

    let at = Utc::now() + TimeDelta::hours(6);
    let (day_start, day_end) = scheduled_status::utc_day_bounds(at);
    seed_schedule(&pool, &alice, day_start + TimeDelta::seconds(1)).await;
    seed_schedule(&pool, &alice, day_end - TimeDelta::seconds(1)).await;

    let refused = schedule(
        &app,
        &token,
        json!({"status": "full day", "scheduled_at": mastodon_time(&at)}),
    )
    .await;
    common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, "a full day");
    common::assert_error(&refused, TOO_MANY, "the per-day limit's refusal");

    let next_day = at + TimeDelta::days(1);
    let accepted = schedule(
        &app,
        &token,
        json!({"status": "the day beside it", "scheduled_at": mastodon_time(&next_day)}),
    )
    .await;
    common::assert_answer(&accepted, StatusCode::OK, "a free day");

    assert_eq!(
        scheduled_count(&pool).await,
        3,
        "the refusal stored nothing; the free day stored one"
    );
    assert_eq!(post_count(&pool).await, 0, "and nothing posted");
}

/// A `media_ids` entry that is unknown, somebody else's upload, an upload
/// a status already claims, or text that is no id at all, is the create's
/// own 422 `Media not found` on this arm too — and no refusal stores a
/// schedule or moves a media row.
#[tokio::test]
async fn media_ids_the_poster_may_not_reserve_are_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = writer(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let foreign = upload(&pool, &bob).await;
    let posted = common::create_status(&pool, &alice, "a live post", Visibility::Public).await;
    let attached = common::create_status_attachment(
        &pool,
        &posted,
        "image/png",
        &format!("{TEST_BASE_URL}/media/live.png"),
        0,
    )
    .await;
    let app = common::test_app(pool.clone());

    for (id, who) in [
        (Uuid::now_v7().to_string(), "an unknown id"),
        (foreign.id.to_string(), "somebody else's upload"),
        (attached.id.to_string(), "an already-attached upload"),
        ("not-an-id".to_owned(), "text that is no id"),
    ] {
        let refused = schedule(
            &app,
            &token,
            json!({"status": "pics", "media_ids": [id], "scheduled_at": in_minutes(60)}),
        )
        .await;
        common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, who);
        common::assert_error(&refused, MEDIA_NOT_FOUND, who);
    }

    assert_eq!(
        scheduled_count(&pool).await,
        0,
        "not one media refusal stored a schedule"
    );
    let untouched = media_attachment::get_by_id(&pool, attached.id)
        .await
        .expect("the attachment reads")
        .expect("the fixture attachment is stored");
    assert_eq!(
        untouched.status_id,
        Some(posted.id),
        "and the probes left its claim exactly as it was"
    );
}

/// An upload one pending schedule reserves cannot be scheduled again
/// while that row stands — every re-claim is 422 `Media not found` — and
/// the reservation never touches the attachment's row: the schedule's
/// entity answers the uploads in the order the body submitted them, the
/// stored params keep that order, and each row's `status_id` stays null
/// until the publish fires.
#[tokio::test]
async fn a_pending_schedule_reserves_its_media_without_writing_the_rows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    // Both uploads are alice's, created in this order; the body submits
    // them in the other one, and both answer and stored params must keep
    // the submitted order (the batch read answers id order).
    let older = upload(&pool, &alice).await;
    let newer = upload(&pool, &alice).await;

    let response = schedule(
        &app,
        &token,
        json!({
            "status": "pics tomorrow",
            "media_ids": [newer.id.to_string(), older.id.to_string()],
            "scheduled_at": in_minutes(60),
        }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "a schedule claiming two uploads");
    let entity = response.json_value();
    assert_eq!(
        entity["params"]["media_ids"],
        json!([newer.id.to_string(), older.id.to_string()]),
        "the params keep the submitted order"
    );
    let attachments = entity["media_attachments"]
        .as_array()
        .expect("media_attachments is an array");
    assert_eq!(attachments.len(), 2, "both uploads are entities");
    assert_eq!(
        attachments[0]["id"],
        newer.id.to_string(),
        "the body's first id answers first"
    );
    assert_eq!(
        attachments[1]["id"],
        older.id.to_string(),
        "and the second follows it, not the id order"
    );

    // Re-claiming either upload — alone or beside the other — is the same
    // 422 while the first schedule is pending.
    for ids in [
        vec![newer.id.to_string()],
        vec![older.id.to_string()],
        vec![newer.id.to_string(), older.id.to_string()],
    ] {
        let refused = schedule(
            &app,
            &token,
            json!({"status": "again", "media_ids": ids, "scheduled_at": in_minutes(120)}),
        )
        .await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a re-claimed upload",
        );
        common::assert_error(&refused, MEDIA_NOT_FOUND, "the reservation's refusal");
    }

    for id in [older.id, newer.id] {
        let row = media_attachment::get_by_id(&pool, id)
            .await
            .expect("the attachment reads")
            .expect("the fixture upload is stored");
        assert_eq!(row.status_id, None, "the schedule wrote no claim");
    }
    assert_eq!(
        scheduled_count(&pool).await,
        1,
        "one schedule stands; every re-claim stored nothing"
    );
    assert_eq!(post_count(&pool).await, 0, "and nothing posted");
}

/// The create's own refusals keep their exact answers on this arm: an
/// empty text is the 422 it always was, a bad `in_reply_to_id` is the
/// shared 404, and a short `expires_in` its floor's 422 — every one of
/// them ahead of the schedule's own gates, and none storing anything.
#[tokio::test]
async fn the_schedule_arm_keeps_the_creates_own_refusals() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let at = in_minutes(60);

    let refusals = [
        (
            json!({"status": "   ", "scheduled_at": at}),
            StatusCode::UNPROCESSABLE_ENTITY,
            EMPTY_STATUS,
        ),
        (
            json!({"status": "", "media_ids": [], "scheduled_at": at}),
            StatusCode::UNPROCESSABLE_ENTITY,
            EMPTY_STATUS,
        ),
        (
            json!({"status": "hi", "in_reply_to_id": UNKNOWN_ID, "scheduled_at": at}),
            StatusCode::NOT_FOUND,
            NOT_FOUND,
        ),
        (
            json!({"status": "hi", "in_reply_to_id": "nope", "scheduled_at": at}),
            StatusCode::NOT_FOUND,
            NOT_FOUND,
        ),
        (
            json!({"status": "hi", "expires_in": 0, "scheduled_at": at}),
            StatusCode::UNPROCESSABLE_ENTITY,
            SHORT_EXPIRY,
        ),
        (
            json!({"status": "hi", "visibility": "sideways", "scheduled_at": at}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid visibility",
        ),
    ];
    for (body, status, message) in refusals {
        let refused = schedule(&app, &token, body.clone()).await;
        common::assert_answer(&refused, status, &format!("the refused body `{body}`"));
        common::assert_error(&refused, message, &format!("the wording for `{body}`"));
    }

    assert_eq!(scheduled_count(&pool).await, 0);
    assert_eq!(post_count(&pool).await, 0);
}

/// The gate is the create's: no token is the extractor's 401, and a user
/// token without `write:statuses` is 403 — a valid future `scheduled_at`
/// changes neither answer, and neither reaches the table.
#[tokio::test]
async fn the_schedule_arm_needs_its_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_bob, read_only) = {
        let user = common::create_local_user(&pool, "bob").await;
        let token = common::create_app_and_token(&pool, &user, "read:statuses").await;
        (user, token)
    };
    let app = common::test_app(pool.clone());
    let body = json!({"status": "someday", "scheduled_at": in_minutes(60)});

    let anonymous = common::TestRequest::post(STATUSES)
        .json(&body)
        .send(&app)
        .await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, NO_TOKEN, "the anonymous refusal");

    let unread = schedule(&app, &read_only, body).await;
    common::assert_answer(
        &unread,
        StatusCode::FORBIDDEN,
        "a token without write:statuses",
    );
    common::assert_error(&unread, NO_WRITE, "the scope refusal");

    assert_eq!(scheduled_count(&pool).await, 0);
    assert_eq!(post_count(&pool).await, 0);
}

/// A form body's urlencoded `scheduled_at` and a query parameter of the
/// same name schedule the very same post a JSON body does.
#[tokio::test]
async fn form_and_query_bodies_schedule_like_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    let at = in_minutes(60);
    let from_form = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "form scheduled"), ("scheduled_at", at.as_str())])
        .send(&app)
        .await;
    common::assert_answer(&from_form, StatusCode::OK, "a form body's scheduled_at");
    assert_eq!(
        from_form.json_value()["params"]["text"],
        "form scheduled",
        "the form body parks the same request"
    );

    let later = in_minutes(120);
    let from_query = common::TestRequest::post(&format!("{STATUSES}?scheduled_at={later}"))
        .bearer(&token)
        .form(&[("status", "query scheduled")])
        .send(&app)
        .await;
    common::assert_answer(
        &from_query,
        StatusCode::OK,
        "a query parameter's scheduled_at",
    );

    let rows = sqlx::query_as::<_, scheduled_status::ScheduledStatus>(
        "SELECT * FROM scheduled_statuses ORDER BY scheduled_at",
    )
    .fetch_all(&pool)
    .await
    .expect("the schedules read");
    assert_eq!(rows.len(), 2, "both bodies parked one row each");
    assert_eq!(
        rows[0].scheduled_at,
        parse_time(&at).expect("the fixture instant is a timestamp"),
        "the form row is due at its instant"
    );
    assert_eq!(
        rows[0].params["status"], "form scheduled",
        "with the form's own normalised text"
    );
    assert_eq!(
        rows[1].scheduled_at,
        parse_time(&later).expect("the fixture instant is a timestamp"),
        "and the query row at its own"
    );
    assert_eq!(rows[1].params["status"], "query scheduled");
    assert_eq!(post_count(&pool).await, 0, "and nothing posted");
}
