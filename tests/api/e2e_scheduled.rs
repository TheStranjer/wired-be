//! End-to-end scenarios over the whole stack (WBE-223): every request
//! goes through the test router, and the background work those requests
//! enqueue runs through [`common::run_all_jobs`], the harness that drains
//! the Postgres job queues with the very registry the live [`Worker`]
//! uses.
//!
//! Three scenarios, each a ticket rule proven across the seam between
//! HTTP and the queue:
//!
//! - **Scheduling**: a `POST /api/v1/statuses` with a future
//!   `scheduled_at` answers the `ScheduledStatus` entity and writes no
//!   status anywhere; backdating the schedule's one
//!   `scheduled_statuses.publish` job and draining the queues publishes
//!   the post, and it then reads back through the public, home and tag
//!   timelines with the parked media and params honored.
//! - **Idempotency**: two posts under one `Idempotency-Key` answer the
//!   same status id and store one row; a different key stores a second.
//! - **Preview**: a `preview=true` post answers the rendered entity —
//!   content, mentions, hashtags — and writes nothing: its id is absent
//!   from every timeline and `GET /api/v1/statuses/{id}` is the shared
//!   404.
//!
//! The per-endpoint refusals behind these flows are pinned in
//! `statuses.rs` and `scheduled_statuses.rs`; this module only drives the
//! happy paths end to end, so a regression anywhere between the request,
//! the queue and the read is caught here first.
//!
//! [`Worker`]: wired_be::jobs::Worker

#![cfg(test)]

use std::path::Path;

use axum::{Router, body::Bytes, http::StatusCode};
use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use wired_be::{
    config::TEST_BASE_URL,
    jobs::scheduled_statuses::KIND_PUBLISH,
    models::user::User,
    state::AppState,
    util::{
        entities::{hashtag_html, mention_html},
        time::mastodon_time,
    },
};

use crate::common;

/// The status create endpoint every scenario drives.
const STATUSES: &str = "/api/v1/statuses";
/// The public timeline read.
const PUBLIC: &str = "/api/v1/timelines/public";
/// The home timeline read.
const HOME: &str = "/api/v1/timelines/home";
/// The tag timeline prefix; each probe appends `/{tag}`.
const TAG: &str = "/api/v1/timelines/tag";
/// The media upload endpoint the scheduling scenario stores its attachment
/// through.
const MEDIA: &str = "/api/v1/media";
/// The scopes the scenario caller carries: the create's `write:statuses`
/// and `write:media` and the reads' `read:statuses`, one token each.
const SCOPES: &str = "read write";
/// The shared 404 body every miss answers.
const NOT_FOUND: &str = "Record not found";
/// The multipart boundary of the upload fixture.
const BOUNDARY: &str = "wired-e2e-boundary";
/// An `MP3`: an `ID3v2` tag header with a zero-length tag — bytes the
/// upload endpoint names `audio/mpeg` from without decoding anything.
const MP3: &[u8] = b"ID3\x03\x00\x00\x00\x00\x00\x00";
/// The text the scheduling scenario parks and then reads back fired.
const SCHEDULED_TEXT: &str = "scheduled hello @bob see #Rust";
/// The text the preview scenario renders without writing.
const PREVIEW_TEXT: &str = "preview hello @bob see #Rust";

/// The render of `text` with `bob` mentioned and `#Rust` folded to a tag:
/// what create, preview and the fired publish all store as `content`.
fn rendered(text: &str, bob: &User) -> String {
    format!(
        "<p>{}</p>",
        text.replace(
            "@bob see #Rust",
            &format!(
                "{} see {}",
                mention_html(&bob.ap_id, "bob"),
                hashtag_html(TEST_BASE_URL, "rust")
            )
        )
    )
}

/// The router and the [`AppState`] behind it, one pool and one config:
/// requests ride the router, `run_all_jobs` drains on the state, and the
/// uploads the endpoint stores live in `uploads_dir`.
fn app_and_state(pool: PgPool, uploads_dir: &Path) -> (Router, AppState) {
    let config = common::test_config_with_uploads_dir(uploads_dir);
    let state = common::test_app_state(pool, config);
    let app = wired_be::app(state.clone());
    (app, state)
}

/// The Mastodon text of the instant `minutes` from now, past the schedule
/// window.
fn in_minutes(minutes: i64) -> String {
    mastodon_time(&(Utc::now() + TimeDelta::minutes(minutes)))
}

/// `GET`s `path`, with `token` when the scenario presents one.
async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// The ids of a rendered timeline page, in answer order.
fn ids_of(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("a timeline page is a JSON array")
        .iter()
        .map(|status| {
            status["id"]
                .as_str()
                .expect("every Status entity carries its id")
                .to_owned()
        })
        .collect()
}

/// The `id` of a status entity, as its JSON spells it.
fn entity_id(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity carries its id")
        .to_owned()
}

/// How many `statuses` rows the database holds.
async fn status_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// How many pending `scheduled_statuses` rows the database holds.
async fn scheduled_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM scheduled_statuses")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// How many `idempotency_keys` claims the database holds.
async fn key_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM idempotency_keys")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// The states of the [`KIND_PUBLISH`] job rows, in insertion order.
async fn publish_job_states(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT state FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(KIND_PUBLISH)
        .fetch_all(pool)
        .await
        .expect("the job rows read")
}

/// How many `jobs` rows have not reached a terminal state: after a drain,
/// nothing may be `available`, `executing` or `retryable` any more.
async fn unsettled_jobs(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM jobs WHERE state NOT IN ('completed', 'discarded')",
    )
    .fetch_one(pool)
    .await
    .expect("the count reads")
}

/// Moves the schedule's one fire into the past, so the next claim round
/// finds it due: rows only, the one `UPDATE` the harness contract allows.
///
/// # Panics
///
/// If the update hits anything but exactly one row.
async fn backdate_publish_jobs(pool: &PgPool) {
    let hit = sqlx::query("UPDATE jobs SET run_at = now() - interval '1 minute' WHERE kind = $1")
        .bind(KIND_PUBLISH)
        .execute(pool)
        .await
        .expect("the backdate update runs");
    assert_eq!(
        hit.rows_affected(),
        1,
        "the parked schedule queued exactly one fire"
    );
}

/// `POST /api/v1/media` with the `MP3` part as `token`, and the entity id
/// the upload answered.
async fn upload_media(app: &Router, token: &str) -> String {
    let response = common::TestRequest::post(MEDIA)
        .bearer(token)
        .multipart(
            BOUNDARY,
            &[common::MultipartPart::file(
                "file",
                "ding.mp3",
                "audio/mpeg",
                Bytes::from_static(MP3),
            )],
        )
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "an audio upload");
    entity_id(&response.json_value())
}

/// A throwaway uploads directory, removed when the returned guard drops.
fn uploads_dir() -> TempDir {
    TempDir::new().expect("temp dir for uploads")
}

/// The public `POST /api/v1/statuses` as `token` with a JSON `body`.
async fn post_json(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

// ─────────────────────  Scenario (a): the scheduled fire  ─────────────────────

/// The body: parked text, reserved upload, the params the fire must honor.
fn schedule_body(at: &str, media_id: &str) -> Value {
    json!({
        "status": format!("  {SCHEDULED_TEXT}  "),
        "scheduled_at": at,
        "media_ids": [media_id],
        "spoiler_text": "careful",
        "sensitive": true,
        "visibility": "public",
    })
}

/// The answer is the `ScheduledStatus` entity: its own keys, the submitted
/// instant, the trimmed text echoed through `params`, and the reserved
/// upload already rendered as an attachment.
fn assert_parked_entity(entity: &Value, at: &str, media_id: &str) {
    assert_eq!(
        entity["scheduled_at"], at,
        "the entity answers the submitted instant"
    );
    assert_eq!(
        entity["params"]["text"], SCHEDULED_TEXT,
        "the echo carries the trimmed text"
    );
    assert_eq!(
        entity["media_attachments"][0]["id"], media_id,
        "the reserved upload rides the entity"
    );
}

/// A parked create wrote no status anywhere: no row, no timeline entry,
/// and the schedule id is no status id — the shared 404.
async fn assert_parked_writes_nothing(pool: &PgPool, app: &Router, token: &str, schedule_id: &str) {
    assert_eq!(status_count(pool).await, 0, "no status row");
    assert_eq!(
        scheduled_count(pool).await,
        1,
        "exactly the parked schedule"
    );
    assert!(
        ids_of(&get(app, None, PUBLIC).await.json_value()).is_empty(),
        "nothing on the public timeline"
    );
    assert!(
        ids_of(&get(app, Some(token), HOME).await.json_value()).is_empty(),
        "nothing on the author's home timeline"
    );
    let missing = get(app, None, &format!("{STATUSES}/{schedule_id}")).await;
    common::assert_answer(
        &missing,
        StatusCode::NOT_FOUND,
        "a schedule id is no status id",
    );
    common::assert_error(&missing, NOT_FOUND, "the parked-schedule probe");
}

/// The fired post is a status the whole API can see: the schedule row is
/// gone and its job completed, the post sits on the public, home and tag
/// timelines, and its entity carries the render of the parked text with
/// the parked params and media honored.
async fn assert_fired_and_visible(
    pool: &PgPool,
    app: &Router,
    token: &str,
    media_id: &str,
    bob: &User,
) {
    assert_eq!(
        scheduled_count(pool).await,
        0,
        "the fire consumed the schedule"
    );
    assert_eq!(
        publish_job_states(pool).await,
        vec!["completed".to_owned()],
        "the drained publish job reached its terminal state"
    );
    let public = ids_of(&get(app, None, PUBLIC).await.json_value());
    assert_eq!(
        public.len(),
        1,
        "the published status is on the public timeline: {public:?}"
    );
    assert_eq!(
        ids_of(&get(app, Some(token), HOME).await.json_value()),
        public,
        "and the author's home timeline answers it too"
    );
    assert_eq!(
        ids_of(&get(app, None, &format!("{TAG}/rust")).await.json_value()),
        public,
        "and so does the tag the parked text carried"
    );

    let read = get(app, Some(token), &format!("{STATUSES}/{}", public[0])).await;
    common::assert_answer(&read, StatusCode::OK, "the fired status reads back");
    let entity = read.json_value();
    assert_eq!(entity["id"], public[0], "the timeline id names the status");
    assert_eq!(
        entity["account"]["acct"], "alice",
        "posted as the scheduling author"
    );
    assert_eq!(entity["visibility"], "public", "the parked visibility");
    assert_eq!(
        entity["spoiler_text"], "careful",
        "the parked content warning"
    );
    assert_eq!(
        entity["sensitive"],
        json!(true),
        "the parked sensitive flag"
    );
    assert_eq!(
        entity["content"],
        rendered(SCHEDULED_TEXT, bob),
        "the parked text, rendered with the mention and the tag"
    );
    assert_eq!(
        entity["media_attachments"][0]["id"], media_id,
        "the reserved upload is the fired status's attachment"
    );
}

/// The scheduling scenario end to end: park through HTTP, prove nothing
/// posted, backdate the one fire, drain the queues through the harness,
/// and read the published status back through HTTP.
///
/// The drain counts three: the backdated [`KIND_PUBLISH`] fire, the
/// `rich_media.fetch` job the fired create queues for its own status (the
/// post carries no page link, so that job completes as a no-op), and the
/// `federator.publish` job the create's note-federation hook queues —
/// which fans out to nothing here, bob being local. All three reach a
/// terminal state, leaving the queue settled.
#[tokio::test]
async fn a_scheduled_post_publishes_when_the_queue_drains() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = uploads_dir();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let token = common::create_app_and_token(&pool, &alice, SCOPES).await;
    let (app, state) = app_and_state(pool.clone(), uploads.path());

    let media_id = upload_media(&app, &token).await;
    let at = in_minutes(60);
    let parked = post_json(&app, &token, schedule_body(&at, &media_id)).await;
    common::assert_answer(&parked, StatusCode::OK, "a future scheduled_at");
    let entity = parked.json_value();
    assert_parked_entity(&entity, &at, &media_id);
    let schedule_id = entity_id(&entity);
    assert_parked_writes_nothing(&pool, &app, &token, &schedule_id).await;

    backdate_publish_jobs(&pool).await;
    assert_eq!(
        common::run_all_jobs(&state).await,
        3,
        "the drain ran the fire, the fired create's card fetch and its note announcement"
    );
    assert_eq!(
        unsettled_jobs(&pool).await,
        0,
        "the queue is settled: all three jobs reached a terminal state"
    );
    assert_eq!(status_count(&pool).await, 1, "the fire wrote one status");

    assert_fired_and_visible(&pool, &app, &token, &media_id, &bob).await;
}

// ─────────────────────  Scenario (b): the idempotency key  ─────────────────────

/// `POST /api/v1/statuses` with the JSON `body` under an `Idempotency-Key`
/// header.
async fn post_keyed(app: &Router, token: &str, key: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .bearer(token)
        .header("idempotency-key", key)
        .json(&body)
        .send(app)
        .await
}

/// The idempotency scenario end to end: one key replayed answers the same
/// status id and one row; a different key is a different post.
#[tokio::test]
async fn one_idempotency_key_replays_one_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, SCOPES).await;
    let app = common::test_app(pool.clone());

    let body = json!({"status": "keyed once"});
    let first = post_keyed(&app, &token, "K-1", body.clone()).await;
    let retry = post_keyed(&app, &token, "K-1", json!({"status": "a different retry"})).await;
    common::assert_answer(&first, StatusCode::OK, "the keyed post");
    common::assert_answer(&retry, StatusCode::OK, "the replay of the same key");
    assert_eq!(
        entity_id(&retry.json_value()),
        entity_id(&first.json_value()),
        "one key answers one status id"
    );
    assert_eq!(
        retry.json_value()["content"],
        first.json_value()["content"],
        "and the first body's entity — the retry body changed nothing"
    );
    assert_eq!(
        status_count(&pool).await,
        1,
        "the retry created no second row"
    );
    assert_eq!(key_count(&pool).await, 1, "and claimed the key once");

    let other = post_keyed(&app, &token, "K-2", body).await;
    common::assert_answer(&other, StatusCode::OK, "a post under a fresh key");
    assert_ne!(
        entity_id(&other.json_value()),
        entity_id(&first.json_value()),
        "a different key is a different post"
    );
    assert_eq!(status_count(&pool).await, 2, "and stored a second row");
    assert_eq!(key_count(&pool).await, 2, "with its own claim");
}

// ────────────────────────  Scenario (c): the preview  ────────────────────────

/// The preview scenario end to end: a `preview=true` post renders the
/// entity a create would answer — the same content, the mentions and
/// hashtags resolved — and writes nothing: the one live post is the real
/// one, the previewed id is absent from every timeline, and the shared
/// 404 answers for it.
#[tokio::test]
async fn a_preview_renders_the_entity_and_writes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let token = common::create_app_and_token(&pool, &alice, SCOPES).await;
    let app = common::test_app(pool.clone());

    let live = post_json(&app, &token, json!({"status": "the real post"})).await;
    common::assert_answer(&live, StatusCode::OK, "the real post");
    let live_id = entity_id(&live.json_value());

    let preview = post_json(
        &app,
        &token,
        json!({"status": PREVIEW_TEXT, "preview": true}),
    )
    .await;
    common::assert_answer(&preview, StatusCode::OK, "a preview");
    let entity = preview.json_value();
    let preview_id = entity_id(&entity);
    assert_ne!(preview_id, live_id, "the preview minted its own fresh id");
    assert_eq!(
        entity["content"],
        rendered(PREVIEW_TEXT, &bob),
        "the render a create of the same body would store"
    );
    assert_eq!(
        entity["mentions"][0]["id"],
        bob.id.to_string(),
        "the mention resolved into the entity"
    );
    assert_eq!(entity["tags"][0]["name"], "rust", "and so did the hashtag");

    assert_eq!(status_count(&pool).await, 1, "only the real post is stored");
    assert_eq!(
        ids_of(&get(&app, None, PUBLIC).await.json_value()),
        vec![live_id.clone()],
        "the public timeline holds the real post alone"
    );
    assert_eq!(
        ids_of(&get(&app, Some(&token), HOME).await.json_value()),
        vec![live_id.clone()],
        "and so does the author's home"
    );
    assert!(
        ids_of(&get(&app, None, &format!("{TAG}/rust")).await.json_value()).is_empty(),
        "the tag the preview mentioned links nothing: a preview stores no tags"
    );

    let missing = get(&app, None, &format!("{STATUSES}/{preview_id}")).await;
    common::assert_answer(
        &missing,
        StatusCode::NOT_FOUND,
        "a previewed id names no status",
    );
    common::assert_error(
        &missing,
        NOT_FOUND,
        "the previewed-id probe is the shared 404",
    );
}
