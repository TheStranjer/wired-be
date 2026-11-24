//! Shared consts and fixture helpers for the four `/api/v1/scheduled_statuses`
//! routes (WBE-107): the listing, the single schedule, the move and the
//! cancel, over the same rows the create arm parks. The canned bodies and
//! scopes these routes answer with, the token fixtures, the fixed instants
//! and seeding helpers, the thin wrappers around the four verbs, and the
//! listing readers that pin their order, cursors and `Link` header.
//! Everything here is `pub(super)`, and a submodule imports exactly the
//! members it calls.

use axum::{Router, http::StatusCode};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    jobs::scheduled_statuses::{KIND_PUBLISH, job_args, unique_key},
    models::{
        scheduled_status::{self, NewScheduledStatus},
        user::User,
    },
    util::time::{mastodon_time, parse_time},
};

use super::support::{WRITE, jobs_of_kind, schedule, stored_schedule};
use crate::common;

/// The scheduled-status listing, and the prefix of the three id-keyed routes.
pub(super) const SCHEDULES: &str = "/api/v1/scheduled_statuses";
/// The scope the two reads on these routes ask the token for.
pub(super) const READ: &str = "read:statuses";
/// The 403 body the scope gate renders for a token without [`READ`].
pub(super) const NO_READ: &str = "Insufficient permissions: read:statuses.";
/// The 422 wording of the stock test instance's 300-second schedule window.
pub(super) const TOO_SOON: &str = "Scheduled time must be at least 5 minutes in the future";
/// The state of a fire no worker has claimed yet.
pub(super) const WAITING: &str = "available";
/// Id text that is no id, for the shared-404 probes.
pub(super) const NOT_AN_ID: &str = "not-an-id";

/// A local user named `nickname` with a `read:statuses` bearer token.
pub(super) async fn reader(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, READ).await;
    (created, token)
}

/// A local user named `nickname` with both of these routes' tokens: the
/// `read:statuses` one the looks need and the `write:statuses` one the moves
/// and cancels need.
pub(super) async fn manager(pool: &PgPool, nickname: &str) -> (User, String, String) {
    let created = common::create_local_user(pool, nickname).await;
    let read = common::create_app_and_token(pool, &created, READ).await;
    let write = common::create_app_and_token(pool, &created, WRITE).await;
    (created, read, write)
}

/// A fixed future instant: `days` after 2027-03-01, in whole seconds, so the
/// instant a body submits, the row that stores it and the entity that answers
/// it are all one and the same moment.
pub(super) fn day(days: i64) -> DateTime<Utc> {
    let base = parse_time("2027-03-01T00:00:00Z").expect("the fixture base is a timestamp");
    base + TimeDelta::days(days)
}

/// A pending schedule for `who`, due `when`, parking `params` as its stored
/// request, seeded straight through the model.
pub(super) async fn seeded(
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
pub(super) async fn seeded_text(
    pool: &PgPool,
    who: &User,
    when: DateTime<Utc>,
    text: &str,
) -> scheduled_status::ScheduledStatus {
    seeded(pool, who, when, json!({ "status": text })).await
}

/// A schedule parked through the endpoint, with its fire queued: the row
/// `POST /api/v1/statuses` leaves behind over a future `scheduled_at`.
pub(super) async fn parked(
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
pub(super) async fn fires_for(pool: &PgPool, id: Uuid) -> Vec<wired_be::models::job::Job> {
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
pub(super) async fn assert_one_fire(pool: &PgPool, id: Uuid, run_at: DateTime<Utc>) {
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
pub(super) async fn list(app: &Router, token: &str, query: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("{SCHEDULES}{query}"))
        .bearer(token)
        .send(app)
        .await
}

/// `GET /api/v1/scheduled_statuses/{id}` as `token`.
pub(super) async fn show(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("{SCHEDULES}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// `PUT /api/v1/scheduled_statuses/{id}` with `body` as JSON, as `token`.
pub(super) async fn move_to(
    app: &Router,
    token: &str,
    id: &str,
    body: Value,
) -> common::TestResponse {
    common::TestRequest::put(&format!("{SCHEDULES}/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The same move with a urlencoded form body, for decision 10.
pub(super) fn move_form(token: &str, id: &str, at: &str) -> common::TestRequest {
    common::TestRequest::put(&format!("{SCHEDULES}/{id}"))
        .bearer(token)
        .form(&[("scheduled_at", at)])
}

/// `DELETE /api/v1/scheduled_statuses/{id}` as `token`.
pub(super) async fn cancel(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::delete(&format!("{SCHEDULES}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// The `params.text` echo of every entity in a listing answer: which parked
/// post each row is, so an order reads as text.
pub(super) fn entity_texts(body: &Value) -> Vec<String> {
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
pub(super) fn row_ids(rows: &[&scheduled_status::ScheduledStatus]) -> Vec<String> {
    rows.iter().map(|row| row.id.to_string()).collect()
}

/// The `Link` header a page of the listing carries between `newest` and
/// `oldest`: Pleroma's `add_link_headers` over the request's own path.
pub(super) fn expected_link(newest: &str, oldest: &str) -> String {
    format!(
        "<{TEST_BASE_URL}{SCHEDULES}?max_id={oldest}>; rel=\"next\", \
         <{TEST_BASE_URL}{SCHEDULES}?min_id={newest}>; rel=\"prev\""
    )
}
