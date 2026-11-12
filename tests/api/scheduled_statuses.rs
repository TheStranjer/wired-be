//! Scheduled statuses over HTTP (WBE-99, WBE-107): the parked posts, from the
//! arm that parks them to the four routes that manage them.
//!
//! The first half is the `scheduled_at` arm of `POST /api/v1/statuses`: a
//! future instant parks the whole create instead of writing a status.
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
//!
//! The second half is `/api/v1/scheduled_statuses`: the listing, one
//! schedule, the move and the cancel, over the rows the arm above parks.
//! The listing is the caller's own rows keyed and ordered on the id —
//! newest created first, the due time nowhere in it, as Pleroma's
//! `Pagination.fetch_paginated` leaves them — 20 rows by default and 40 at
//! the most, with the `Link` header walking from both ends of the page. A
//! move takes `scheduled_at` (JSON, form or query alike), owes the create's
//! window for it, leaves the parked request exactly as it was stored, and
//! carries its fire along: one waiting publish job, due at the new moment
//! and a fresh row for it. A cancel takes the row and that job with it and
//! answers `{}`. Somebody else's schedule, an id the table never held and
//! id text that is no id are one 404 `Record not found` on all three, and
//! the gate is Pleroma's pair: `read:statuses` to look, `write:statuses` to
//! move and cancel, 401 anonymous.

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
            "quote_id": null,
            "expires_in": 3600,
            "in_reply_to_id": null,
            "in_reply_to_conversation_id": null,
            "media_ids": [],
            "poll": null,
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

// The four `/api/v1/scheduled_statuses` routes (WBE-107): the listing, the
// single schedule, the move and the cancel, over the same rows the create arm
// above parks.

/// The scheduled-status listing, and the prefix of the three id-keyed routes.
const SCHEDULES: &str = "/api/v1/scheduled_statuses";
/// The scope the two reads on these routes ask the token for.
const READ: &str = "read:statuses";
/// The 403 body the scope gate renders for a token without [`READ`].
const NO_READ: &str = "Insufficient permissions: read:statuses.";
/// The 422 wording of the stock test instance's 300-second schedule window.
const TOO_SOON: &str = "Scheduled time must be at least 5 minutes in the future";
/// The state of a fire no worker has claimed yet.
const WAITING: &str = "available";
/// Id text that is no id, for the shared-404 probes.
const NOT_AN_ID: &str = "not-an-id";

/// A local user named `nickname` with a `read:statuses` bearer token.
async fn reader(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, READ).await;
    (created, token)
}

/// A local user named `nickname` with both of these routes' tokens: the
/// `read:statuses` one the looks need and the `write:statuses` one the moves
/// and cancels need.
async fn manager(pool: &PgPool, nickname: &str) -> (User, String, String) {
    let created = common::create_local_user(pool, nickname).await;
    let read = common::create_app_and_token(pool, &created, READ).await;
    let write = common::create_app_and_token(pool, &created, WRITE).await;
    (created, read, write)
}

/// A fixed future instant: `days` after 2027-03-01, in whole seconds, so the
/// instant a body submits, the row that stores it and the entity that answers
/// it are all one and the same moment.
fn day(days: i64) -> DateTime<Utc> {
    let base = parse_time("2027-03-01T00:00:00Z").expect("the fixture base is a timestamp");
    base + TimeDelta::days(days)
}

/// A pending schedule for `who`, due `when`, parking `params` as its stored
/// request, seeded straight through the model.
async fn seeded(
    pool: &PgPool,
    who: &User,
    when: DateTime<Utc>,
    params: Value,
) -> scheduled_status::ScheduledStatus {
    scheduled_status::insert(
        pool,
        &NewScheduledStatus {
            user_id: who.id,
            scheduled_at: when,
            params,
        },
    )
    .await
    .expect("the seed schedule inserts")
}

/// A seeded schedule parking nothing but `text`, for the listings.
async fn seeded_text(
    pool: &PgPool,
    who: &User,
    when: DateTime<Utc>,
    text: &str,
) -> scheduled_status::ScheduledStatus {
    seeded(pool, who, when, json!({ "status": text })).await
}

/// A schedule parked through the endpoint, with its fire queued: the row
/// `POST /api/v1/statuses` leaves behind over a future `scheduled_at`.
async fn parked(
    app: &Router,
    pool: &PgPool,
    token: &str,
    text: &str,
    at: DateTime<Utc>,
) -> scheduled_status::ScheduledStatus {
    let created = schedule(
        app,
        token,
        json!({"status": text, "scheduled_at": mastodon_time(&at)}),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "a scheduled create");
    let id = created.json_value()["id"]
        .as_str()
        .expect("the entity carries its id")
        .to_owned();
    stored_schedule(pool, id.parse::<Uuid>().expect("the id is a uuid")).await
}

/// The `scheduled_statuses.publish` jobs queued for schedule `id`, in
/// insertion order.
async fn fires_for(pool: &PgPool, id: Uuid) -> Vec<wired_be::models::job::Job> {
    let key = unique_key(id);
    jobs_of_kind(pool, KIND_PUBLISH)
        .await
        .into_iter()
        .filter(|job| job.unique_key.as_deref() == Some(key.as_str()))
        .collect()
}

/// Asserts that exactly one fire stands for schedule `id`: waiting on the
/// scheduled queue at `run_at`, keyed by the schedule, and arguing for the same
/// row — what a move leaves behind, and what a refusal must not disturb.
async fn assert_one_fire(pool: &PgPool, id: Uuid, run_at: DateTime<Utc>) {
    let fires = fires_for(pool, id).await;
    assert_eq!(fires.len(), 1, "one fire for the schedule, not two");
    let job = &fires[0];
    assert_eq!(job.queue, "scheduled", "on the scheduled queue");
    assert_eq!(job.state, WAITING, "waiting for its moment");
    assert_eq!(job.args, job_args(id), "the same row to publish");
    assert_eq!(
        job.unique_key.as_deref(),
        Some(unique_key(id).as_str()),
        "keyed by the schedule"
    );
    assert_eq!(
        job.run_at, run_at,
        "due at the moment the schedule now names"
    );
}

/// `GET /api/v1/scheduled_statuses{query}` as `token`.
async fn list(app: &Router, token: &str, query: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("{SCHEDULES}{query}"))
        .bearer(token)
        .send(app)
        .await
}

/// `GET /api/v1/scheduled_statuses/{id}` as `token`.
async fn show(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("{SCHEDULES}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// `PUT /api/v1/scheduled_statuses/{id}` with `body` as JSON, as `token`.
async fn move_to(app: &Router, token: &str, id: &str, body: Value) -> common::TestResponse {
    common::TestRequest::put(&format!("{SCHEDULES}/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The same move with a urlencoded form body, for decision 10.
fn move_form(token: &str, id: &str, at: &str) -> common::TestRequest {
    common::TestRequest::put(&format!("{SCHEDULES}/{id}"))
        .bearer(token)
        .form(&[("scheduled_at", at)])
}

/// `DELETE /api/v1/scheduled_statuses/{id}` as `token`.
async fn cancel(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::delete(&format!("{SCHEDULES}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// The `id` of every entity in a listing answer, in the order they came.
fn entity_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the listing is an array")
        .iter()
        .map(|entity| {
            entity["id"]
                .as_str()
                .expect("every entity carries its id")
                .to_owned()
        })
        .collect()
}

/// The `params.text` echo of every entity in a listing answer: which parked
/// post each row is, so an order reads as text.
fn entity_texts(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the listing is an array")
        .iter()
        .map(|entity| {
            entity["params"]["text"]
                .as_str()
                .expect("every entity carries its params")
                .to_owned()
        })
        .collect()
}

/// The ids of `rows`, in the order handed in.
fn row_ids(rows: &[&scheduled_status::ScheduledStatus]) -> Vec<String> {
    rows.iter().map(|row| row.id.to_string()).collect()
}

/// The `Link` header a page of the listing carries between `newest` and
/// `oldest`: Pleroma's `add_link_headers` over the request's own path.
fn expected_link(newest: &str, oldest: &str) -> String {
    format!(
        "<{TEST_BASE_URL}{SCHEDULES}?max_id={oldest}>; rel=\"next\", \
         <{TEST_BASE_URL}{SCHEDULES}?min_id={newest}>; rel=\"prev\""
    )
}

/// The listing answers the caller's own schedules, keyed and ordered on the
/// row id exactly as Pleroma's `Pagination.fetch_paginated` leaves them:
/// newest created first, the due time nowhere in the ordering, and the whole
/// `ScheduledStatus` entity per row.
#[tokio::test]
async fn the_listing_answers_the_callers_own_schedules_newest_id_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read) = reader(&pool, "alice").await;
    let (bob, bob_read) = reader(&pool, "bob").await;
    // Created oldest-first, with the due times the other way round: a page
    // ordered by `scheduled_at` would answer `newest` last, so the order
    // below can only be the id's.
    let oldest = seeded_text(&pool, &alice, day(9), "oldest").await;
    let middle = seeded_text(&pool, &alice, day(5), "middle").await;
    let newest = seeded_text(&pool, &alice, day(1), "newest").await;
    let theirs = seeded_text(&pool, &bob, day(2), "not yours").await;
    let app = common::test_app(pool.clone());

    let first = list(&app, &read, "").await;
    common::assert_answer(&first, StatusCode::OK, "the caller's schedules");
    let body = first.json_value();
    assert_eq!(
        entity_texts(&body),
        vec!["newest", "middle", "oldest"],
        "newest id first, which is not the due-time order"
    );
    assert_eq!(
        entity_ids(&body),
        row_ids(&[&newest, &middle, &oldest]),
        "the three ids, in that order"
    );
    assert!(
        !entity_ids(&body).contains(&theirs.id.to_string()),
        "and not one word of another account's schedule"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link(&newest.id.to_string(), &oldest.id.to_string()).as_str(),),
        "next walks to the oldest row shown, prev back to the newest"
    );

    let shown = body
        .as_array()
        .expect("the page is an array")
        .first()
        .expect("the page holds a row")
        .clone();
    assert_eq!(
        object_keys(&shown),
        keys(&["id", "scheduled_at", "params", "media_attachments"]),
        "every entry is a full ScheduledStatus entity"
    );
    assert_eq!(
        shown["scheduled_at"],
        mastodon_time(&newest.scheduled_at),
        "the row's own moment, in the Mastodon format"
    );
    assert_eq!(
        shown["params"]["text"], "newest",
        "the parked post echoed as the API names it"
    );
    assert_eq!(shown["media_attachments"], json!([]), "nothing reserved");

    assert_eq!(
        entity_ids(&list(&app, &bob_read, "").await.json_value()),
        row_ids(&[&theirs]),
        "and bob's page is his own one row, not alice's three"
    );
}

/// The cursors are ids and they walk the listing from both ends: `limit`
/// shortens a page and its `Link` header with it, `max_id` continues where a
/// page ended, `min_id` answers the newer page (newest first all the same),
/// `since_id` reads everything after its cursor, and an account with nothing
/// answers `[]` with no `Link` header.
#[tokio::test]
async fn the_listing_walks_its_id_cursors_in_both_directions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read) = reader(&pool, "alice").await;
    let oldest = seeded_text(&pool, &alice, day(9), "oldest").await;
    let middle = seeded_text(&pool, &alice, day(5), "middle").await;
    let newest = seeded_text(&pool, &alice, day(1), "newest").await;
    let app = common::test_app(pool.clone());

    let page = list(&app, &read, "?limit=2").await;
    common::assert_answer(&page, StatusCode::OK, "a two-row page");
    assert_eq!(
        entity_ids(&page.json_value()),
        row_ids(&[&newest, &middle]),
        "the limit cuts the oldest row off the end"
    );
    assert_eq!(
        page.header("link"),
        Some(expected_link(&newest.id.to_string(), &middle.id.to_string()).as_str()),
        "and the cursors move with the page"
    );

    let older = list(&app, &read, &format!("?max_id={}", middle.id)).await;
    assert_eq!(
        entity_ids(&older.json_value()),
        row_ids(&[&oldest]),
        "max_id continues where the page ended"
    );
    let newer = list(&app, &read, &format!("?min_id={}", middle.id)).await;
    assert_eq!(
        entity_ids(&newer.json_value()),
        row_ids(&[&newest]),
        "min_id answers the newer page, still newest first"
    );
    let unseen = list(&app, &read, &format!("?since_id={}", middle.id)).await;
    assert_eq!(
        entity_ids(&unseen.json_value()),
        row_ids(&[&newest]),
        "since_id reads everything made after its cursor"
    );

    let carol = common::create_local_user(&pool, "carol").await;
    let carol_read = common::create_app_and_token(&pool, &carol, READ).await;
    let nothing = list(&app, &carol_read, "").await;
    common::assert_answer(&nothing, StatusCode::OK, "an account with no schedule");
    assert_eq!(nothing.json_value(), json!([]), "none is the empty array");
    assert_eq!(
        nothing.header("link"),
        None,
        "and an empty page links nowhere"
    );
}

/// The page size is Pleroma's: 20 rows when the request asks for none (or for
/// something unusable), 40 at the most however hard a client pushes, and the
/// `max_id` walk reaches every schedule exactly once.
#[tokio::test]
async fn the_listing_is_twenty_rows_by_default_and_forty_at_most() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read) = reader(&pool, "alice").await;
    let mut made = Vec::new();
    for index in 0..45_i64 {
        let when = day(1) + TimeDelta::minutes(index);
        made.push(seeded_text(&pool, &alice, when, "x").await);
    }
    let app = common::test_app(pool.clone());
    // Creation order is id order (decision 4), so the pages run the seed back
    // to front.
    let newest_first: Vec<String> = made.iter().rev().map(|row| row.id.to_string()).collect();

    let first = list(&app, &read, "").await;
    common::assert_answer(&first, StatusCode::OK, "the default page");
    assert_eq!(
        entity_ids(&first.json_value()),
        newest_first[..20],
        "20 of the 45, newest id first"
    );
    for query in ["?limit=0", "?limit=-5", "?limit=abc"] {
        assert_eq!(
            entity_ids(&list(&app, &read, query).await.json_value()).len(),
            20,
            "`{query}` is no usable limit, so the default page answers"
        );
    }
    assert_eq!(
        entity_ids(&list(&app, &read, "?limit=5").await.json_value()).len(),
        5,
        "a usable limit goes straight through"
    );
    for query in ["?limit=40", "?limit=41", "?limit=1000"] {
        assert_eq!(
            entity_ids(&list(&app, &read, query).await.json_value()).len(),
            40,
            "`{query}` is past the cap, and the cap is what the page holds"
        );
    }

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<Uuid> = None;
    for _ in 0..10 {
        let query = cursor.map_or_else(String::new, |id| format!("?max_id={id}"));
        let page = list(&app, &read, &query).await;
        let ids = entity_ids(&page.json_value());
        if ids.is_empty() {
            assert_eq!(
                page.header("link"),
                None,
                "the walk's last page is empty and links nowhere"
            );
            break;
        }
        seen.extend(ids.iter().cloned());
        cursor = Some(
            ids.last()
                .expect("a non-empty page has a last row")
                .parse::<Uuid>()
                .expect("the ids are ids"),
        );
    }
    assert_eq!(
        seen, newest_first,
        "two full pages and a last of five: every schedule exactly once, in one order"
    );
}

/// `GET /api/v1/scheduled_statuses/{id}` answers the one schedule as the same
/// entity the listing gives it, its reserved attachments read back in the
/// order its params name them.
#[tokio::test]
async fn show_answers_the_one_schedule_it_holds_back() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read) = reader(&pool, "alice").await;
    let older = upload(&pool, &alice).await;
    let newer = upload(&pool, &alice).await;
    let row = seeded(
        &pool,
        &alice,
        day(3),
        json!({
            "status": "pics tomorrow",
            "visibility": "private",
            "media_ids": [newer.id.to_string(), older.id.to_string()],
        }),
    )
    .await;
    let app = common::test_app(pool.clone());

    let response = show(&app, &read, &row.id.to_string()).await;
    common::assert_answer(&response, StatusCode::OK, "the caller's own schedule");
    let entity = response.json_value();
    assert_eq!(
        object_keys(&entity),
        keys(&["id", "scheduled_at", "params", "media_attachments"]),
        "the entity's own keys, no more"
    );
    assert_eq!(
        entity["id"],
        row.id.to_string(),
        "the id as hyphenated text"
    );
    assert_eq!(
        entity["scheduled_at"],
        mastodon_time(&row.scheduled_at),
        "the moment it is due"
    );
    assert_eq!(
        entity["params"]["text"], "pics tomorrow",
        "the parked post echoed, as is its audience"
    );
    assert_eq!(entity["params"]["visibility"], "private");
    assert_eq!(
        entity["params"]["media_ids"],
        json!([newer.id.to_string(), older.id.to_string()]),
        "and the reservation, in the stored order"
    );
    let attachments = entity["media_attachments"]
        .as_array()
        .expect("media_attachments is an array");
    assert_eq!(attachments.len(), 2, "both reserved uploads are entities");
    assert_eq!(
        attachments[0]["id"],
        newer.id.to_string(),
        "in the order the params name them, not the id order"
    );
    assert_eq!(attachments[1]["id"], older.id.to_string());

    let listed = list(&app, &read, "").await.json_value();
    assert_eq!(
        listed
            .as_array()
            .expect("the listing is an array")
            .first()
            .expect("the row is listed"),
        &entity,
        "and the listing answers the very same entity"
    );
}

/// A schedule the caller cannot reach is one 404 `{"error":"Record not
/// found"}` however it is unreachable — somebody else's row, an id the table
/// never held, id text that is no id — on each of the three id-keyed routes,
/// and none of the probes touches the row it aimed at.
#[tokio::test]
async fn an_unreachable_schedule_is_one_404_for_every_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_read, alice_write) = manager(&pool, "alice").await;
    let (_bob, bob_read, bob_write) = manager(&pool, "bob").await;
    let row = seeded_text(&pool, &alice, day(3), "alice's own").await;
    let unknown = Uuid::now_v7().to_string();
    let at = mastodon_time(&day(8));
    let body = json!({"scheduled_at": at});
    let app = common::test_app(pool.clone());

    let probes = [
        (
            show(&app, &bob_read, &row.id.to_string()).await,
            "bob looking",
        ),
        (
            move_to(&app, &bob_write, &row.id.to_string(), body.clone()).await,
            "bob moving",
        ),
        (
            cancel(&app, &bob_write, &row.id.to_string()).await,
            "bob cancelling",
        ),
        (
            show(&app, &alice_read, &unknown).await,
            "an id never stored",
        ),
        (
            move_to(&app, &alice_write, &unknown, body.clone()).await,
            "a move of an id never stored",
        ),
        (
            cancel(&app, &alice_write, &unknown).await,
            "a cancel of an id never stored",
        ),
        (
            show(&app, &alice_read, NOT_AN_ID).await,
            "id text that is no id",
        ),
        (
            move_to(&app, &alice_write, NOT_AN_ID, body).await,
            "a move over id text that is no id",
        ),
        (
            cancel(&app, &alice_write, NOT_AN_ID).await,
            "a cancel over id text that is no id",
        ),
    ];
    for (response, who) in probes {
        common::assert_answer(&response, StatusCode::NOT_FOUND, who);
        common::assert_error(&response, NOT_FOUND, who);
    }

    let intact = stored_schedule(&pool, row.id).await;
    assert_eq!(intact.scheduled_at, day(3), "none of them moved it");
    assert_eq!(
        intact.params,
        json!({ "status": "alice's own" }),
        "none of them rewrote it"
    );
    common::assert_answer(
        &cancel(&app, &alice_write, &row.id.to_string()).await,
        StatusCode::OK,
        "and alice still can",
    );
}

/// `PUT` moves the row and its fire together: the parked request is untouched,
/// exactly one `scheduled_statuses.publish` job stands for the schedule and it
/// is due at the new moment, as a fresh row — the waiting one was deleted by
/// its `unique_key` rather than edited in place.
#[tokio::test]
async fn a_move_takes_the_row_and_its_fire_to_the_new_moment() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, read, write) = manager(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    let created = schedule(
        &app,
        &write,
        json!({"status": "  move me  ", "scheduled_at": mastodon_time(&day(3))}),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "a scheduled create");
    let entity = created.json_value();
    let id = entity["id"]
        .as_str()
        .expect("the entity carries its id")
        .parse::<Uuid>()
        .expect("the id is a uuid");
    let before = stored_schedule(&pool, id).await;
    let waiting = fires_for(&pool, id).await;
    assert_eq!(waiting.len(), 1, "the create queued one fire");
    assert_eq!(waiting[0].run_at, day(3), "due at the submitted moment");

    let moved = move_to(
        &app,
        &write,
        &id.to_string(),
        json!({"scheduled_at": mastodon_time(&day(10))}),
    )
    .await;
    common::assert_answer(&moved, StatusCode::OK, "a moved schedule");
    let after = moved.json_value();
    assert_eq!(
        after["id"], entity["id"],
        "the same schedule, not a new one"
    );
    assert_eq!(
        after["scheduled_at"],
        mastodon_time(&day(10)),
        "answered at the moment it now names"
    );
    assert_eq!(
        after["params"]["text"], "move me",
        "when a post fires is all a move touches"
    );
    assert_eq!(
        object_keys(&after),
        keys(&["id", "scheduled_at", "params", "media_attachments"]),
        "the whole entity, as the create answered it"
    );

    let row = stored_schedule(&pool, id).await;
    assert_eq!(row.scheduled_at, day(10), "the row holds the new moment");
    assert_eq!(row.params, before.params, "and the same normalised request");
    assert!(
        row.updated_at > row.inserted_at,
        "and knows it was written again"
    );

    assert_one_fire(&pool, id, day(10)).await;
    assert_ne!(
        fires_for(&pool, id).await[0].id,
        waiting[0].id,
        "a fresh job row: the waiting one was deleted, not edited"
    );
    assert_eq!(
        show(&app, &read, &id.to_string()).await.json_value()["scheduled_at"],
        mastodon_time(&day(10)),
        "the single read follows the move"
    );
    assert_eq!(
        list(&app, &read, "")
            .await
            .json_value()
            .as_array()
            .expect("the listing is an array")
            .first()
            .expect("the schedule is listed")["scheduled_at"],
        mastodon_time(&day(10)),
        "and so does the listing"
    );
}

/// A form body and a query parameter move the schedule the way a JSON body
/// does (decision 10), and an offset spelling of the instant is normalised to
/// the same UTC moment the `Z` form names.
#[tokio::test]
async fn a_move_reads_json_form_and_query_alike() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, _read, write) = manager(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let row = parked(&app, &pool, &write, "every shape", day(3)).await;
    let id = row.id.to_string();

    let by_form = move_form(&write, &id, &mastodon_time(&day(11)))
        .send(&app)
        .await;
    common::assert_answer(&by_form, StatusCode::OK, "a form body's scheduled_at");
    assert_eq!(
        by_form.json_value()["scheduled_at"],
        mastodon_time(&day(11)),
        "the form moved it"
    );

    let in_query = format!("{SCHEDULES}/{id}?scheduled_at={}", mastodon_time(&day(12)));
    let by_query = common::TestRequest::put(&in_query)
        .bearer(&write)
        .send(&app)
        .await;
    common::assert_answer(
        &by_query,
        StatusCode::OK,
        "a query parameter's scheduled_at",
    );
    assert_eq!(
        stored_schedule(&pool, row.id).await.scheduled_at,
        day(12),
        "and so did the query"
    );

    let offset = day(13) + TimeDelta::hours(1);
    let by_offset = move_to(
        &app,
        &write,
        &id,
        json!({"scheduled_at": "2027-03-14T02:00:00+01:00"}),
    )
    .await;
    common::assert_answer(&by_offset, StatusCode::OK, "an offset spelling");
    assert_eq!(
        by_offset.json_value()["scheduled_at"],
        mastodon_time(&offset),
        "normalised to the UTC instant it names"
    );
    assert_one_fire(&pool, row.id, offset).await;
}

/// A moment inside the window is the create's own refusal — a past instant and
/// a too-soon one alike, the wording naming the configured minutes — and text
/// that is no instant, or none at all, is `Invalid scheduled_at`. Not one of
/// them touches the row or its fire.
#[tokio::test]
async fn a_move_inside_the_window_is_refused_like_a_create() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, _read, write) = manager(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let row = parked(&app, &pool, &write, "stay put", day(3)).await;
    let id = row.id.to_string();

    for (body, who) in [
        (json!({"scheduled_at": in_minutes(-3_600)}), "an hour ago"),
        (json!({"scheduled_at": in_minutes(0)}), "this instant"),
        (json!({"scheduled_at": in_minutes(1)}), "one minute out"),
        (
            json!({"scheduled_at": mastodon_time(&(Utc::now() + TimeDelta::minutes(4)))}),
            "a minute inside the window",
        ),
    ] {
        let refused = move_to(&app, &write, &id, body).await;
        common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, who);
        common::assert_error(&refused, TOO_SOON, who);
    }

    for (body, who) in [
        (
            json!({"scheduled_at": "not-a-date"}),
            "text that is no instant",
        ),
        (json!({"scheduled_at": ""}), "empty text"),
        (json!({"scheduled_at": null}), "a null"),
        (json!({}), "no scheduled_at at all"),
        (
            json!({"scheduled_at": "2027-02-30T00:00:00Z"}),
            "a date that never happened",
        ),
    ] {
        let refused = move_to(&app, &write, &id, body).await;
        common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, who);
        common::assert_error(&refused, INVALID_SCHEDULED_AT, who);
    }

    let intact = stored_schedule(&pool, row.id).await;
    assert_eq!(
        intact.scheduled_at,
        day(3),
        "not one refusal moved the row off its moment"
    );
    assert_eq!(
        intact.updated_at, intact.inserted_at,
        "and not one restamped it either"
    );
    assert_one_fire(&pool, row.id, day(3)).await;
}

/// Pleroma's plug order, kept: the body is read, then the row is looked for,
/// and only then is the moment weighed. So a stranger's move — or a move of an
/// id that is no id — is the 404 the id alone already was, whatever it asked
/// the schedule to do.
#[tokio::test]
async fn a_move_checks_the_row_before_it_checks_the_moment() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _read, write) = manager(&pool, "alice").await;
    let (_bob, _bob_read, bob_write) = manager(&pool, "bob").await;
    let row = seeded_text(&pool, &alice, day(3), "not yours to move").await;
    let app = common::test_app(pool.clone());

    let too_soon = in_minutes(1);
    let far_off = mastodon_time(&day(9));
    let probes = [
        (
            bob_write.as_str(),
            row.id.to_string(),
            too_soon.clone(),
            "a stranger's move of a moment inside the window",
        ),
        (
            bob_write.as_str(),
            row.id.to_string(),
            far_off.clone(),
            "a stranger's move of a moment outside it",
        ),
        (
            write.as_str(),
            NOT_AN_ID.to_owned(),
            too_soon,
            "a move over id text that is no id",
        ),
    ];
    for (token, id, at, who) in probes {
        let refused = move_to(&app, token, &id, json!({"scheduled_at": at})).await;
        common::assert_answer(&refused, StatusCode::NOT_FOUND, who);
        common::assert_error(&refused, NOT_FOUND, who);
    }

    assert_eq!(
        scheduled_count(&pool).await,
        1,
        "nothing was stored or removed"
    );
    let intact = stored_schedule(&pool, row.id).await;
    assert_eq!(
        intact.scheduled_at,
        day(3),
        "and the row every probe named stands where it was"
    );
}

/// `DELETE` takes the row and the fire waiting for it, answers `{}`, and leaves
/// nothing behind: the listing no longer carries it, the queue holds no
/// publish for it, and every later probe is the shared 404.
#[tokio::test]
async fn a_cancel_takes_the_row_and_its_fire() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, read, write) = manager(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let row = parked(&app, &pool, &write, "cancel me", day(3)).await;
    let id = row.id;
    assert_one_fire(&pool, id, day(3)).await;

    let deleted = cancel(&app, &write, &id.to_string()).await;
    common::assert_answer(&deleted, StatusCode::OK, "a cancelled schedule");
    assert_eq!(
        deleted.json_value(),
        json!({}),
        "the body is the empty object Pleroma's delete renders"
    );

    assert_eq!(scheduled_count(&pool).await, 0, "the row is gone");
    assert!(
        scheduled_status::get_by_id(&pool, id)
            .await
            .expect("the schedule reads")
            .is_none(),
        "and it is that row that went"
    );
    assert!(
        fires_for(&pool, id).await.is_empty(),
        "and its waiting fire went with it"
    );
    assert!(
        jobs_of_kind(&pool, KIND_PUBLISH).await.is_empty(),
        "no publish job of any schedule is left"
    );
    assert!(
        entity_ids(&list(&app, &read, "").await.json_value()).is_empty(),
        "and the listing has nothing left to answer"
    );

    common::assert_all_refused(
        &[
            cancel(&app, &write, &id.to_string()).await,
            show(&app, &read, &id.to_string()).await,
            move_to(
                &app,
                &write,
                &id.to_string(),
                json!({"scheduled_at": mastodon_time(&day(9))}),
            )
            .await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "what was cancelled",
    );
    assert_eq!(
        post_count(&pool).await,
        0,
        "a cancelled schedule posts nothing"
    );
}

/// Cancelling is also what releases a schedule's attachments: while the row
/// stands its uploads are reserved from every other post, and the cancel takes
/// the reservation with them.
#[tokio::test]
async fn a_cancel_releases_the_uploads_it_reserved() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _read, write) = manager(&pool, "alice").await;
    let pic = upload(&pool, &alice).await;
    let row = seeded(
        &pool,
        &alice,
        day(4),
        json!({ "status": "pics", "media_ids": [pic.id.to_string()] }),
    )
    .await;
    let app = common::test_app(pool.clone());

    assert_eq!(
        scheduled_status::reserved_media_ids(&pool, &[pic.id])
            .await
            .expect("the reservation reads"),
        vec![pic.id],
        "the pending schedule holds the upload"
    );
    common::assert_answer(
        &cancel(&app, &write, &row.id.to_string()).await,
        StatusCode::OK,
        "the cancel",
    );
    assert!(
        scheduled_status::reserved_media_ids(&pool, &[pic.id])
            .await
            .expect("the reservation reads again")
            .is_empty(),
        "and the cancel released it"
    );

    let reposted = schedule(
        &app,
        &write,
        json!({
            "status": "now",
            "media_ids": [pic.id.to_string()],
            "scheduled_at": mastodon_time(&day(5)),
        }),
    )
    .await;
    common::assert_answer(
        &reposted,
        StatusCode::OK,
        "a schedule claiming the released upload",
    );
    assert_eq!(
        reposted.json_value()["media_attachments"][0]["id"],
        pic.id.to_string(),
        "and it answers as reserved once more"
    );
}

/// Ownership is the whole gate on these routes: another account's schedule is
/// invisible in the listing and unreachable by id for every verb, so nobody
/// reads, moves or cancels a post that is not theirs.
#[tokio::test]
async fn one_account_cannot_reach_anothers_schedule() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, _alice_read, alice_write) = manager(&pool, "alice").await;
    let (_bob, bob_read, bob_write) = manager(&pool, "bob").await;
    let app = common::test_app(pool.clone());
    let row = parked(&app, &pool, &alice_write, "alice's", day(3)).await;
    let id = row.id.to_string();

    assert!(
        entity_ids(&list(&app, &bob_read, "").await.json_value()).is_empty(),
        "bob's listing does not carry alice's schedule"
    );
    common::assert_all_refused(
        &[
            show(&app, &bob_read, &id).await,
            cancel(&app, &bob_write, &id).await,
            move_to(
                &app,
                &bob_write,
                &id,
                json!({"scheduled_at": mastodon_time(&day(9))}),
            )
            .await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "a stranger at alice's schedule",
    );

    let intact = stored_schedule(&pool, row.id).await;
    assert_eq!(
        intact.scheduled_at,
        day(3),
        "alice's schedule stands where she left it"
    );
    assert_eq!(
        intact.params["status"], "alice's",
        "and still parks the post she parked"
    );
    assert_one_fire(&pool, row.id, day(3)).await;
    assert_eq!(
        entity_ids(&list(&app, &bob_read, "").await.json_value()).len(),
        0,
        "still nothing of alice's in bob's page"
    );
}

/// No token is the extractor's 401 on all four routes, and Pleroma's scope
/// pair refuses the mismatched tokens before either the row or the moment is
/// looked at: `read:statuses` to look, `write:statuses` to move and cancel.
#[tokio::test]
async fn the_routes_refuse_anonymous_and_out_of_scope_tokens() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read, write) = manager(&pool, "alice").await;
    let row = seeded_text(&pool, &alice, day(3), "gated").await;
    let id = row.id.to_string();
    let app = common::test_app(pool.clone());

    common::assert_all_refused(
        &[
            common::TestRequest::get(SCHEDULES).send(&app).await,
            common::TestRequest::get(&format!("{SCHEDULES}/{id}"))
                .send(&app)
                .await,
            common::TestRequest::put(&format!("{SCHEDULES}/{id}"))
                .json(&json!({"scheduled_at": mastodon_time(&day(9))}))
                .send(&app)
                .await,
            common::TestRequest::delete(&format!("{SCHEDULES}/{id}"))
                .send(&app)
                .await,
        ],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "no token at all",
    );
    common::assert_all_refused(
        &[list(&app, &write, "").await, show(&app, &write, &id).await],
        StatusCode::FORBIDDEN,
        NO_READ,
        "a write-only token looking",
    );
    common::assert_all_refused(
        &[
            move_to(
                &app,
                &read,
                &id,
                json!({"scheduled_at": mastodon_time(&day(9))}),
            )
            .await,
            cancel(&app, &read, &id).await,
        ],
        StatusCode::FORBIDDEN,
        NO_WRITE,
        "a read-only token changing",
    );

    assert_eq!(
        scheduled_count(&pool).await,
        1,
        "every refusal above stopped at its gate: the schedule still stands"
    );
    assert_eq!(
        entity_ids(&list(&app, &read, "").await.json_value()).len(),
        1,
        "and the token that does hold the scope still reads it"
    );
}

/// The roots of the two scope trees satisfy their children here, and satisfy
/// nothing else: a `read` token reads its own account's schedules, a `write`
/// token moves and cancels its own, and either one is the shared 404 on
/// somebody else's row.
#[tokio::test]
async fn the_root_scopes_reach_their_own_accounts_schedules() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _read, _write) = manager(&pool, "alice").await;
    let theirs = seeded_text(&pool, &alice, day(3), "alice's own").await;
    let dora = common::create_local_user(&pool, "dora").await;
    let dora_read = common::create_app_and_token(&pool, &dora, "read").await;
    let dora_row = seeded_text(&pool, &dora, day(4), "dora's own").await;
    let erin = common::create_local_user(&pool, "erin").await;
    let erin_write = common::create_app_and_token(&pool, &erin, "write").await;
    let erin_row = seeded_text(&pool, &erin, day(5), "erin's own").await;
    let erin_other = seeded_text(&pool, &erin, day(6), "erin's other").await;
    let app = common::test_app(pool.clone());

    let listed = list(&app, &dora_read, "").await;
    common::assert_answer(&listed, StatusCode::OK, "a root `read` token listing");
    assert_eq!(
        entity_ids(&listed.json_value()),
        vec![dora_row.id.to_string()],
        "and it lists only what its own account owns"
    );
    common::assert_answer(
        &show(&app, &dora_read, &dora_row.id.to_string()).await,
        StatusCode::OK,
        "the same token reading one of its own",
    );

    common::assert_answer(
        &move_to(
            &app,
            &erin_write,
            &erin_row.id.to_string(),
            json!({"scheduled_at": mastodon_time(&day(7))}),
        )
        .await,
        StatusCode::OK,
        "a root `write` token moving its own schedule",
    );
    assert_eq!(
        stored_schedule(&pool, erin_row.id).await.scheduled_at,
        day(7),
        "and the row moved"
    );
    common::assert_answer(
        &cancel(&app, &erin_write, &erin_other.id.to_string()).await,
        StatusCode::OK,
        "a root `write` token cancelling its own schedule",
    );

    common::assert_all_refused(
        &[
            show(&app, &dora_read, &theirs.id.to_string()).await,
            cancel(&app, &erin_write, &theirs.id.to_string()).await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "alice's row, whatever the root scope",
    );
    assert_eq!(
        scheduled_count(&pool).await,
        3,
        "one cancel went through, and it was erin's own"
    );
}
