//! Pinned statuses over HTTP (WBE-82): `POST /api/v1/statuses/{id}/pin`
//! and its `…/unpin` twin, plus the `pinned` and `pleroma.pinned_at` halves
//! of the [`Status`] entity.
//!
//! The writes are Pleroma's `pin` and `unpin`: a user token with
//! `write:accounts`, the status resolved through the shared visibility gate
//! (a malformed, unknown, deleted or invisible id is the one 404), and the
//! answer the [`Status`] entity as the caller sees it — `pinned` true with
//! `pleroma.pinned_at` beside it after a pin, both empty after a
//! withdrawal, the row re-read for the render. Behind the gate stand
//! Pleroma's refusals, in its order and with the ticket's exact bodies:
//! somebody else's status (422), a `private`, `direct` or `local` one (422
//! — only `public` and `unlisted` pass), a reblog row (422) and the
//! instance's own limit,
//! [`max_pinned_statuses`](wired_be::config::Config::max_pinned_statuses)
//! (422). Restating a pin that is already there is the unchanged entity,
//! even at the limit, because Pleroma's own write skips its check for it,
//! and withdrawing a pin that is not there is the same unchanged entity.
//!
//! The pin writes federate the fact they store: the `Featured`
//! collection's `Add` and `Remove` queue through the module's
//! `after_pin_changed` hook and
//! [`wired_be::services::federation_out::after_pin_changed`], delivered to
//! the pinner's remote followers when the fan-out job runs. These routes
//! ride no throttle bucket either — Pleroma's rate-limited status actions
//! are the like writes and the delete, not the pins.
//!
//! [`Status`]: wired_be::entities::status::Status

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::publisher,
    models::{
        pinned_status,
        status::{self, NewStatus, Status as StatusRow, Visibility},
        user::{self, User},
    },
    util::time::mastodon_time,
};

use crate::common;

/// The 404 body of the shared status gate.
const NOT_FOUND: &str = "Record not found";
/// The 401 body `AuthUser` sends when no usable token was presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for an app-only token on a route that needs a user.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 401 body for a token that does not resolve.
const BAD_TOKEN: &str = "The access token is invalid";
/// The 403 wording of the pin gate: these two routes ask `write:accounts`.
const NO_WRITE: &str = "Insufficient permissions: write:accounts.";
/// The 422 body for a status of somebody else's, on the pin.
const NOT_YOURS: &str = "Someone else's status cannot be pinned";
/// The 422 body for a status of somebody else's, on the withdrawal.
const NOT_YOURS_UNPIN: &str = "Someone else's status cannot be unpinned";
/// The 422 body for a `private`, `direct` or `local` status.
const NOT_PUBLIC: &str = "Non-public status cannot be pinned";
/// The 422 body for a reblog row.
const NO_REBLOG: &str = "Reblogs cannot be pinned";
/// The 422 body past the instance's pin limit.
const AT_LIMIT: &str = "You have already pinned the maximum number of statuses";
/// An id that names no stored status.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// The limit the raised-limit fixture configures, and walks through.
const RAISED_LIMIT: i64 = 3;

/// The limit the multi-pin fixtures configure: the shipped default is
/// Pleroma's one, and a test that pins two posts needs more room than that.
const MULTI_LIMIT: usize = 5;

/// An application with room for [`MULTI_LIMIT`] pins, for the fixtures that
/// need to pin more than one status.
fn multi_pin_app(pool: &PgPool) -> Router {
    let mut config = common::test_config();
    config.max_pinned_statuses = MULTI_LIMIT;
    common::test_app_with_config(pool.clone(), config)
}

/// A local user named `nickname` who may pin and unpin (`write:accounts`),
/// delete their own posts (`write:statuses`, which the delete-fixture tests
/// need) and read a status through a presented token (`read`).
async fn pinner(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token =
        common::create_app_and_token(pool, &created, "write:accounts write:statuses read").await;
    (created, token)
}

/// A local user named `nickname` with exactly `scopes`, for the gate tests
/// that must *lack* `write:accounts`.
async fn scoped(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A public post by `author`.
async fn post(pool: &PgPool, author: &User) -> StatusRow {
    common::create_status(pool, author, "a post", Visibility::Public).await
}

/// A public post by a fresh local author named `nickname`.
async fn post_by(pool: &PgPool, nickname: &str) -> StatusRow {
    post(pool, &common::create_local_user(pool, nickname).await).await
}

/// A post by `author` with `visibility`.
async fn post_with(pool: &PgPool, author: &User, visibility: Visibility) -> StatusRow {
    common::create_status(pool, author, "hidden from timelines", visibility).await
}

/// A reply `author` left under `parent`.
async fn reply(pool: &PgPool, author: &User, parent: &StatusRow) -> StatusRow {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent.user_id),
            ..NewStatus::local(author, "an answer")
        },
    )
    .await
    .expect("the fixture reply inserts")
}

/// A reblog `reblogger` wrote of `original`, as its row.
async fn reblog(pool: &PgPool, reblogger: &User, original: Uuid) -> StatusRow {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            reblog_of_id: Some(original),
            ..NewStatus::local(reblogger, "")
        },
    )
    .await
    .expect("the fixture reblog inserts")
}

/// `POST …/pin` for status `id`.
async fn pin_route(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/pin"))
        .bearer(token)
        .send(app)
        .await
}

/// `POST …/unpin` for status `id`.
async fn unpin_route(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/unpin"))
        .bearer(token)
        .send(app)
        .await
}

/// `DELETE /api/v1/statuses/{id}` — the write that must take a pin with it.
async fn delete_route(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::delete(&format!("/api/v1/statuses/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// A `GET /api/v1/statuses/{id}` as `token`.
async fn show(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/api/v1/statuses/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// A `GET /api/v1/statuses?id[]=a&id[]=b` batch read as `token`.
async fn show_batch(app: &Router, token: &str, ids: &[Uuid]) -> common::TestResponse {
    let joined = ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<String>>()
        .join("&id[]=");
    common::TestRequest::get(&format!("/api/v1/statuses?id[]={joined}"))
        .bearer(token)
        .send(app)
        .await
}

/// A pin a test only needs to have happened, answered as JSON.
async fn pinned(app: &Router, token: &str, id: &str) -> Value {
    let response = pin_route(app, token, id).await;
    common::assert_answer(&response, StatusCode::OK, "the pin stores");
    response.json_value()
}

/// A pin a test only needs to have happened: the row is stored, and the
/// body it answers is the test's business elsewhere.
async fn pin_ok(app: &Router, token: &str, id: &str) {
    let response = pin_route(app, token, id).await;
    common::assert_answer(&response, StatusCode::OK, "the pin stores");
}

/// How many pin rows exist in the table.
async fn row_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM pinned_statuses")
        .fetch_one(pool)
        .await
        .expect("the table counts");
    row.0
}

/// The `(user, status)` pin rows of the table, so an assertion says what
/// storage holds rather than what an endpoint chose to answer.
async fn stored(pool: &PgPool) -> Vec<(Uuid, Uuid)> {
    sqlx::query_as::<_, (Uuid, Uuid)>("SELECT user_id, status_id FROM pinned_statuses")
        .fetch_all(pool)
        .await
        .expect("the rows read")
}

/// The moment the account `user_id` pinned `status_id`, as the table holds
/// it and as `pleroma.pinned_at` must answer with it.
async fn pin_moment(pool: &PgPool, user_id: Uuid, status_id: Uuid) -> Value {
    let moment = pinned_status::pinned_ids(pool, user_id, &[status_id])
        .await
        .expect("the pin reads")
        .get(&status_id)
        .copied()
        .expect("the fixture pinned this status");
    Value::String(mastodon_time(&moment))
}

/// The `pinned` and `pleroma.pinned_at` pair a response body carries, so a
/// test asserts the two halves of one read together — they can never
/// disagree, because one map answers both.
fn flag_and_stamp(body: &Value) -> (Value, Value) {
    (body["pinned"].clone(), body["pleroma"]["pinned_at"].clone())
}

#[tokio::test]
async fn pinning_marks_the_status_and_repeating_restates_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = pinner(&pool, "alice").await;
    let status = post(&pool, &alice).await;
    let id = status.id.to_string();

    let first = pinned(&app, &token, &id).await;
    assert_eq!(first["id"], id, "the answer is the status entity");
    assert_eq!(first["pinned"], true, "flagged for its author");
    let stamp = pin_moment(&pool, alice.id, status.id).await;
    assert_eq!(
        first["pleroma"]["pinned_at"], stamp,
        "and the same read's timestamp rides beside the flag"
    );

    let again = pinned(&app, &token, &id).await;
    assert_eq!(again["pinned"], true, "a repeat restates the flag");
    assert_eq!(
        again["pleroma"]["pinned_at"], stamp,
        "and keeps the moment of the first pin, since it keeps the row"
    );
    assert_eq!(stored(&pool).await, vec![(alice.id, status.id)], "one row");

    let shown = show(&app, &token, &id).await;
    common::assert_answer(&shown, StatusCode::OK, "the show route answers");
    assert_eq!(
        flag_and_stamp(&shown.json_value()),
        (Value::Bool(true), stamp),
        "the batched pins read flags the status for its pinner"
    );

    let gone = unpin_route(&app, &token, &id).await;
    common::assert_answer(&gone, StatusCode::OK, "the withdrawal answers");
    assert_eq!(
        flag_and_stamp(&gone.json_value()),
        (Value::Bool(false), Value::Null),
        "the flag and the stamp clear together"
    );
    assert_eq!(row_count(&pool).await, 0, "and the row is gone");
}

#[tokio::test]
async fn a_pin_is_the_pinner_s_own_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = pinner(&pool, "alice").await;
    let status = post(&pool, &alice).await;
    let id = status.id.to_string();
    pin_ok(&app, &token, &id).await;

    let (_carol, her_token) = pinner(&pool, "carol").await;
    let for_carol = show(&app, &her_token, &id).await;
    assert_eq!(
        flag_and_stamp(&for_carol.json_value()),
        (Value::Bool(false), Value::Null),
        "alice's pin is nobody else's flag, not even on the same read"
    );

    let anonymous = common::TestRequest::get(&format!("/api/v1/statuses/{id}"))
        .send(&app)
        .await;
    assert_eq!(
        flag_and_stamp(&anonymous.json_value()),
        (Value::Bool(false), Value::Null),
        "and an anonymous reader has no pins to name"
    );
}

#[tokio::test]
async fn a_whole_page_flags_the_pinned_rows_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = pinner(&pool, "alice").await;
    let kept = post(&pool, &alice).await;
    let ignored = post(&pool, &alice).await;
    pin_ok(&app, &token, &kept.id.to_string()).await;

    let page = show_batch(&app, &token, &[kept.id, ignored.id]).await;
    common::assert_answer(&page, StatusCode::OK, "the batch read answers");
    let body = page.json_value();
    let entries = body.as_array().expect("the answer is an array");
    assert_eq!(
        common::ids_of(&body),
        vec![kept.id.to_string(), ignored.id.to_string()],
        "the batch answers in the requested order"
    );
    let stamp = pin_moment(&pool, alice.id, kept.id).await;
    assert_eq!(
        flag_and_stamp(&entries[0]),
        (Value::Bool(true), stamp),
        "the pinned post carries both halves"
    );
    assert_eq!(
        flag_and_stamp(&entries[1]),
        (Value::Bool(false), Value::Null),
        "and its unpinned neighbour carries neither"
    );
}

#[tokio::test]
async fn withdrawing_a_pin_that_was_never_made_is_the_unchanged_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = pinner(&pool, "alice").await;
    let status = post(&pool, &alice).await;

    let response = unpin_route(&app, &token, &status.id.to_string()).await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        "a withdrawal with nothing to withdraw",
    );
    assert_eq!(
        flag_and_stamp(&response.json_value()),
        (Value::Bool(false), Value::Null),
        "is the unchanged entity"
    );
    assert_eq!(row_count(&pool).await, 0, "and stores nothing");
}

#[tokio::test]
async fn an_unlisted_post_and_a_reply_are_pinnable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = multi_pin_app(&pool);
    let (alice, token) = pinner(&pool, "alice").await;
    let quiet = post_with(&pool, &alice, Visibility::Unlisted).await;
    let root = post(&pool, &alice).await;
    let answer = reply(&pool, &alice, &root).await;

    for status in [&quiet, &answer] {
        let entity = pinned(&app, &token, &status.id.to_string()).await;
        assert_eq!(
            entity["pinned"], true,
            "Pleroma admits an unlisted post and a reply, so {} pins",
            status.id
        );
    }
    assert_eq!(row_count(&pool).await, 2, "two pins, two rows");
}

#[tokio::test]
async fn a_post_that_is_not_public_refuses_the_pin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = pinner(&pool, "alice").await;

    for visibility in [Visibility::Private, Visibility::Direct, Visibility::Local] {
        let hidden = post_with(&pool, &alice, visibility).await;
        let response = pin_route(&app, &token, &hidden.id.to_string()).await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("a {visibility} post of one's own"),
        );
        common::assert_error(&response, NOT_PUBLIC, "the visibility refusal");
    }
    assert_eq!(row_count(&pool).await, 0, "none of the three stored a pin");
}

#[tokio::test]
async fn somebody_else_s_status_refuses_both_writes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = pinner(&pool, "alice").await;
    let theirs = post_by(&pool, "bob").await;
    let id = theirs.id.to_string();

    let refused = pin_route(&app, &token, &id).await;
    common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, "a pin");
    common::assert_error(&refused, NOT_YOURS, "the ownership refusal");

    let withdrawn = unpin_route(&app, &token, &id).await;
    common::assert_answer(&withdrawn, StatusCode::UNPROCESSABLE_ENTITY, "an unpin");
    common::assert_error(
        &withdrawn,
        NOT_YOURS_UNPIN,
        "the same rule on the withdrawal",
    );
    assert_eq!(
        stored(&pool).await,
        Vec::new(),
        "and neither of them stored a pin"
    );
}

#[tokio::test]
async fn a_reblog_has_no_pin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = pinner(&pool, "alice").await;
    let original = post_by(&pool, "bob").await;
    let boost = reblog(&pool, &alice, original.id).await;

    let response = pin_route(&app, &token, &boost.id.to_string()).await;
    common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, "a boost");
    common::assert_error(&response, NO_REBLOG, "the shape refusal");

    let withdrawn = unpin_route(&app, &token, &boost.id.to_string()).await;
    common::assert_answer(
        &withdrawn,
        StatusCode::OK,
        "a boost has no pin to withdraw, which is not an error",
    );
    assert_eq!(row_count(&pool).await, 0, "and nothing was stored");
}

#[tokio::test]
async fn the_second_pin_passes_the_default_limit_of_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = pinner(&pool, "alice").await;
    let first = post(&pool, &alice).await;
    let second = post(&pool, &alice).await;
    pin_ok(&app, &token, &first.id.to_string()).await;

    let refused = pin_route(&app, &token, &second.id.to_string()).await;
    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "one past the shipped limit",
    );
    common::assert_error(&refused, AT_LIMIT, "the limit refusal");
    assert_eq!(
        stored(&pool).await,
        vec![(alice.id, first.id)],
        "only the first pin is stored"
    );

    let restated = pinned(&app, &token, &first.id.to_string()).await;
    assert_eq!(
        restated["pinned"], true,
        "and at the limit, restating the pin that is already there is still \
         the unchanged entity — Pleroma skips its own check for it"
    );
}

#[tokio::test]
async fn a_raised_limit_admits_exactly_that_many_pins() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.max_pinned_statuses = usize::try_from(RAISED_LIMIT).unwrap_or_default();
    let app = common::test_app_with_config(pool.clone(), config);
    let (alice, token) = pinner(&pool, "alice").await;

    let mut pinned_ids = Vec::new();
    for _ in 0..RAISED_LIMIT {
        let status = post(&pool, &alice).await;
        pin_ok(&app, &token, &status.id.to_string()).await;
        pinned_ids.push(status.id);
    }
    let over = post(&pool, &alice).await;
    let refused = pin_route(&app, &token, &over.id.to_string()).await;
    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "one past the cap",
    );
    common::assert_error(&refused, AT_LIMIT, "the limit refusal");
    assert_eq!(row_count(&pool).await, RAISED_LIMIT, "no pin more");

    // Unpinning frees a slot, which is how the limit is a live budget and
    // not a one-time allowance.
    let first = pinned_ids
        .first()
        .expect("the fixture pinned at least one status");
    let released = unpin_route(&app, &token, &first.to_string()).await;
    common::assert_answer(&released, StatusCode::OK, "the withdrawal frees a slot");
    pin_ok(&app, &token, &over.id.to_string()).await;
    assert_eq!(
        row_count(&pool).await,
        RAISED_LIMIT,
        "and the new pin took it"
    );
}

#[tokio::test]
async fn a_status_the_gate_hides_is_the_shared_four_zero_four() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = pinner(&pool, "alice").await;
    let stranger = common::create_local_user(&pool, "secret").await;
    let hidden = post_with(&pool, &stranger, Visibility::Private).await;
    let deleted = post(&pool, &alice).await;
    common::assert_answer(
        &delete_route(&app, &token, &deleted.id.to_string()).await,
        StatusCode::OK,
        "the fixture delete runs",
    );

    for id in [
        hidden.id.to_string(),
        deleted.id.to_string(),
        UNKNOWN_ID.to_owned(),
        "not-an-uuid".to_owned(),
    ] {
        common::assert_all_refused(
            &[
                pin_route(&app, &token, &id).await,
                unpin_route(&app, &token, &id).await,
            ],
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            "what the gate hides, on both writes",
        );
    }
    assert_eq!(row_count(&pool).await, 0, "nothing was pinned behind it");
}

#[tokio::test]
async fn deleting_a_pinned_status_removes_the_pin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    // Room for two pins: the test is about what a delete leaves behind, not
    // about the limit that `a_raised_limit_admits_exactly_that_many_pins`
    // walks.
    let app = multi_pin_app(&pool);
    let (alice, token) = pinner(&pool, "alice").await;
    let doomed = post(&pool, &alice).await;
    let kept = post(&pool, &alice).await;
    pin_ok(&app, &token, &doomed.id.to_string()).await;
    pin_ok(&app, &token, &kept.id.to_string()).await;
    assert_eq!(row_count(&pool).await, 2, "two pins before the delete");

    let response = delete_route(&app, &token, &doomed.id.to_string()).await;
    common::assert_answer(&response, StatusCode::OK, "the delete answers");
    assert_eq!(
        stored(&pool).await,
        vec![(alice.id, kept.id)],
        "the tombstone's pin went with it, and the survivor's stayed"
    );
    assert_eq!(
        pinned_status::count(&pool, alice.id)
            .await
            .expect("the pins count"),
        1,
        "so the limit budget frees up on a delete"
    );
    assert_eq!(
        pinned_status::list_for_user(&pool, alice.id)
            .await
            .expect("the profile list reads")
            .iter()
            .map(|row| row.id)
            .collect::<Vec<Uuid>>(),
        vec![kept.id],
        "and the profile lists the survivor only"
    );
}

/// The activity of every queued `federator.publish` job, insertion order.
async fn publish_activities(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args->'activity' FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read")
    .into_iter()
    .map(|(activity,)| activity)
    .collect()
}

/// The federation half of the pair: the pin queues the `Add` that features
/// the status in the author's own collection, and the unpin the `Remove`
/// that takes it back down — one announcement per write that changed
/// something, restatements silent.
#[tokio::test]
async fn the_pin_writes_queue_the_feature_and_its_withdrawal() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = pinner(&pool, "alice").await;
    let featured_post = post(&pool, &alice).await;

    pin_ok(&app, &token, &featured_post.id.to_string()).await;

    let after_pin = publish_activities(&pool).await;
    assert_eq!(after_pin.len(), 1, "the pin announces once");
    assert_eq!(after_pin[0]["type"], "Add");
    assert_eq!(after_pin[0]["actor"], alice.ap_id);
    assert_eq!(after_pin[0]["object"], featured_post.uri);
    assert_eq!(
        after_pin[0]["target"],
        user::featured(&alice.ap_id),
        "the bare fixture carries no featured_url, so the built collection address rides along"
    );
    assert_eq!(
        after_pin[0]["to"][0],
        "https://www.w3.org/ns/activitystreams#Public"
    );

    let response = unpin_route(&app, &token, &featured_post.id.to_string()).await;
    common::assert_answer(&response, StatusCode::OK, "the withdrawal answers");

    let after_unpin = publish_activities(&pool).await;
    assert_eq!(
        after_unpin.len(),
        2,
        "and the withdrawal announces once more"
    );
    assert_eq!(after_unpin[1]["type"], "Remove");
    assert_eq!(after_unpin[1]["object"], featured_post.uri);

    let restated = unpin_route(&app, &token, &featured_post.id.to_string()).await;
    common::assert_answer(&restated, StatusCode::OK, "a second withdrawal answers");
    assert_eq!(
        publish_activities(&pool).await.len(),
        2,
        "a withdrawal that found no pin announces nothing"
    );
}

#[tokio::test]
async fn the_write_gate_demands_a_user_with_the_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, read_only) = scoped(&pool, "viewer", "read").await;
    let (_poster, write_statuses) = scoped(&pool, "poster", "write:statuses read").await;
    let status = post_by(&pool, "bob").await;
    let id = status.id.to_string();

    let nobody = pin_route(&app, "", &id).await;
    common::assert_answer(&nobody, StatusCode::UNAUTHORIZED, "no usable token");
    common::assert_error(&nobody, NO_TOKEN, "no usable token");
    let bogus = unpin_route(&app, "not-a-token", &id).await;
    common::assert_answer(
        &bogus,
        StatusCode::UNAUTHORIZED,
        "a token that does not resolve",
    );
    common::assert_error(&bogus, BAD_TOKEN, "an unresolvable token");

    let app_only = common::create_app_only_token(&pool, "read write").await;
    let headless = pin_route(&app, &app_only, &id).await;
    common::assert_answer(&headless, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(&headless, USER_REQUIRED, "an app-only token");

    common::assert_all_refused(
        &[
            pin_route(&app, &read_only, &id).await,
            unpin_route(&app, &read_only, &id).await,
            pin_route(&app, &write_statuses, &id).await,
            unpin_route(&app, &write_statuses, &id).await,
        ],
        StatusCode::FORBIDDEN,
        NO_WRITE,
        "a token that does not grant write:accounts",
    );
    assert_eq!(row_count(&pool).await, 0, "nothing was refused in vain");
}
