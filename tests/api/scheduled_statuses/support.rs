//! Shared consts and fixture helpers for the scheduling half of the area:
//! the canned error bodies and scopes the `POST /api/v1/statuses` arm
//! answers, the thin wrappers around the HTTP harness, the table counters
//! and model-level readers the tests probe through, and the whole-shape
//! assertions a scheduled create owes. Everything here is `pub(super)`, and
//! a submodule imports exactly the members it calls.

use std::collections::BTreeSet;

use axum::Router;
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
        user::User,
    },
    util::time::{mastodon_time, parse_time},
};

use crate::common;

/// The endpoint under test.
pub(super) const STATUSES: &str = "/api/v1/statuses";
/// The scope the create's gate — and this arm — asks the token for.
pub(super) const WRITE: &str = "write:statuses";
/// The 401 body `AuthUser` renders when the request carries no token.
pub(super) const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body the scope gate renders for a token without [`WRITE`].
pub(super) const NO_WRITE: &str = "Insufficient permissions: write:statuses.";
/// The 422 body for text that trims to nothing over no attachments.
pub(super) const EMPTY_STATUS: &str = "Cannot post an empty status without attachments";
/// The shared 404 body, for the parent probes.
pub(super) const NOT_FOUND: &str = "Record not found";
/// The 422 body for an id that is no id, and for a media id the poster
/// may not reserve.
pub(super) const MEDIA_NOT_FOUND: &str = "Media not found";
/// The 422 body for an account at either schedule limit.
pub(super) const TOO_MANY: &str = "Too many scheduled statuses";
/// The 422 body for `scheduled_at` text that is no RFC3339 instant.
pub(super) const INVALID_SCHEDULED_AT: &str = "Invalid scheduled_at";
/// The 422 wording of the stock test instance's 600-second expiration floor.
pub(super) const SHORT_EXPIRY: &str = "expires_in must be at least 600 seconds";
/// The id of a status that is not stored here, for the parent probe.
pub(super) const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

/// The Mastodon text of the instant `minutes` from now.
pub(super) fn in_minutes(minutes: i64) -> String {
    mastodon_time(&(Utc::now() + TimeDelta::minutes(minutes)))
}

/// A local user named `nickname` with a `write:statuses` bearer token.
pub(super) async fn writer(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, WRITE).await;
    (created, token)
}

/// `POST /api/v1/statuses` with `body` as JSON, as `token`.
pub(super) async fn schedule(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The keys of a JSON object, sorted: for pinning a shape key-for-key.
pub(super) fn object_keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("the value is a JSON object")
        .keys()
        .cloned()
        .collect()
}

/// The expected key set built from the names a shape must carry exactly.
pub(super) fn keys(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// How many schedules the table holds (every row is pending: the fire and
/// the cancel both delete).
pub(super) async fn scheduled_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM scheduled_statuses")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// How many `statuses` rows the database holds at all.
pub(super) async fn post_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// How many `idempotency_keys` rows the database holds.
pub(super) async fn key_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM idempotency_keys")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// The `jobs` rows of `kind`, in insertion order.
pub(super) async fn jobs_of_kind(pool: &PgPool, kind: &str) -> Vec<wired_be::models::job::Job> {
    sqlx::query_as::<_, wired_be::models::job::Job>(
        "SELECT * FROM jobs WHERE kind = $1 ORDER BY id",
    )
    .bind(kind)
    .fetch_all(pool)
    .await
    .expect("the job rows read")
}

/// The schedule stored under `id`.
pub(super) async fn stored_schedule(pool: &PgPool, id: Uuid) -> scheduled_status::ScheduledStatus {
    scheduled_status::get_by_id(pool, id)
        .await
        .expect("the schedule reads")
        .expect("the schedule is stored")
}

/// An unposted `image/png` upload by `owner`, as a `media_ids` entry names it.
pub(super) async fn upload(pool: &PgPool, owner: &User) -> media_attachment::MediaAttachment {
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
pub(super) async fn seed_schedule(pool: &PgPool, who: &User, when: DateTime<Utc>) {
    let row = scheduled_status::insert(
        pool,
        &NewScheduledStatus {
            user_id: who.id,
            scheduled_at: when,
            params: json!({ "status": "seeded" }),
            idempotency_key: None,
        },
    )
    .await
    .expect("the seed schedule inserts")
    .expect("a keyless seed never conflicts");
    assert_eq!(row.user_id, who.id, "the seed row is its account's");
}

/// The whole answer of a scheduled create, pinned key for key: the
/// entity's own keys, the fire moment in the Mastodon format, the empty
/// attachments of an attachment-free body, and the complete params echo —
/// the request re-named the way the API spells it, with `poll` and
/// `scheduled_at` fixed to `null` and the two echo keys filled in.
pub(super) fn assert_entity_answers_the_request(entity: &Value, at: &str, application_id: &str) {
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
pub(super) async fn assert_row_parks_the_request(
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
            "quote_approval_policy": null,
            "expires_in": 3600,
            "in_reply_to_id": null,
            "in_reply_to_conversation_id": null,
            "quote_id": null,
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
pub(super) async fn assert_only_the_fire_is_queued(
    pool: &PgPool,
    row: &scheduled_status::ScheduledStatus,
) {
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
