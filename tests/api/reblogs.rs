//! Reblogs over HTTP (WBE-81): `POST /api/v1/statuses/{id}/reblog` and its
//! `…/unreblog` twin, the state listing `GET /api/v1/statuses/{id}/reblogged_by`,
//! and the `reblogged` half of the [`Status`] entity.
//!
//! A reblog is a status row with `reblog_of_id` set — decision 6's `Announce`
//! wearing status clothing — written by [`create_reblog`] with empty content,
//! the requested `visibility` and the `{base}/activities/{id}` uri, counting
//! once against the original's `reblogs_count` and once against the
//! reblogger's `statuses_count`. The writes run under `write:statuses` on
//! the `statuses_actions` bucket, resolve the status through the shared
//! visibility gate (the invisible, unknown and deleted are the one 404
//! `{"error":"Record not found"}`), and fold a reblog row's id through to
//! its original: nobody boosts a boost. Behind the gate the ticket's two
//! 422s stand: a `visibility` outside `public` / `unlisted` / `private` is
//! `{"error":"Invalid visibility"}`, and a `private` or `direct` target —
//! even the caller's own — is `{"error":"Could not reblog private status"}`.
//! `POST …/reblog` answers the reblog row's entity (outer, `reblogged: true`,
//! original nested); `POST …/unreblog` answers the original (with
//! `reblogged: false`), unchanged and 200 when the caller never boosted.
//! `GET …/reblogged_by` lists the boosters' [`Account`] entities, newest
//! boost first, paged with `Link` cursors over the *reblog row ids*. Both
//! writes are idempotent on the partial unique index over
//! `(user_id, reblog_of_id)`: a repeat reblog restates the same row and one
//! count, and a withdrawal with nothing to withdraw moves nothing.
//!
//! The `Announce` and its `Undo` federate at the module's
//! `after_reblog_changed` hook; the queueing itself is the federation
//! service's territory (`services::federation_out`'s tests own the
//! deliveries), so no delivery is asserted here.
//! The boost notification that hook also sends is the notification
//! generation area's (`notification_generation.rs`), not this file's.
//!
//! [`Status`]: wired_be::entities::status::Status
//! [`Account`]: wired_be::entities::account::Account
//! [`create_reblog`]: wired_be::models::status::create_reblog

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        status::{self, Status as StatusRow, Visibility},
        user::{self, User},
    },
};

use crate::common;

/// The 404 body of the shared status gate.
const NOT_FOUND: &str = "Record not found";
/// The 401 body `AuthUser` sends when no usable token was presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body for a token that does not resolve.
const BAD_TOKEN: &str = "The access token is invalid";
/// The 403 body for an app-only token on a route that needs a user.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 422 body for a `visibility` the boost write does not take.
const INVALID_VISIBILITY: &str = "Invalid visibility";
/// The 422 body for a `private` or `direct` target.
const CANNOT_REBLOG: &str = "Could not reblog private status";
/// The 403 body both writes answer when the token lacks their scope.
const NO_WRITE: &str = "Insufficient permissions: write:statuses.";
/// An id that names no stored status.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

/// A local user named `nickname` who may boost and withdraw
/// (`write:statuses`) and read through a presented token (`read`).
async fn booster(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, "write:statuses read").await;
    (created, token)
}

/// A local user named `nickname` with exactly `scopes`, for the gate tests
/// that must *lack* one of the route's scopes.
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

/// `POST …/reblog` for status `id` with no body: the default public boost.
async fn reblog_route(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/reblog"))
        .bearer(token)
        .send(app)
        .await
}

/// `POST …/reblog` with a form `visibility` parameter.
async fn reblog_form(
    app: &Router,
    token: &str,
    id: &str,
    visibility: &str,
) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/reblog"))
        .bearer(token)
        .form(&[("visibility", visibility)])
        .send(app)
        .await
}

/// `POST …/reblog` with a JSON body.
async fn reblog_json(app: &Router, token: &str, id: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/reblog"))
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// `POST …/unreblog` for status `id`.
async fn unreblog_route(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/unreblog"))
        .bearer(token)
        .send(app)
        .await
}

/// `GET …/reblogged_by` for status `id`, optionally as `token`, with `query`
/// appended to the path.
async fn reblogged_by(
    app: &Router,
    token: Option<&str>,
    id: &str,
    query: &str,
) -> common::TestResponse {
    let path = format!("/api/v1/statuses/{id}/reblogged_by");
    let url = if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    };
    let request = common::TestRequest::get(&url);
    match token {
        Some(bearer) => request.bearer(bearer).send(app).await,
        None => request.send(app).await,
    }
}

/// A `GET /api/v1/statuses/{id}`, optionally as `token`.
async fn show(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}"));
    match token {
        Some(bearer) => request.bearer(bearer).send(app).await,
        None => request.send(app).await,
    }
}

/// A boost a test only needs to have happened, answered as JSON.
async fn reblogged(app: &Router, token: &str, id: &str) -> Value {
    let response = reblog_route(app, token, id).await;
    common::assert_answer(&response, StatusCode::OK, "the reblog stores");
    response.json_value()
}

/// A boost a test only needs to have happened.
async fn reblog_ok(app: &Router, token: &str, id: &str) {
    let response = reblog_route(app, token, id).await;
    common::assert_answer(&response, StatusCode::OK, "the reblog stores");
}

/// The live reblog rows as `(reblog_id, user_id)`, newest id first — the
/// order the page walks.
async fn stored_reblogs(pool: &PgPool) -> Vec<(Uuid, Uuid)> {
    sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT id, user_id FROM statuses
          WHERE reblog_of_id IS NOT NULL AND deleted_at IS NULL
          ORDER BY id DESC",
    )
    .fetch_all(pool)
    .await
    .expect("the reblog rows read")
}

/// The stored `visibility` of every live reblog of `original_id`.
async fn stored_reblog_visibilities(pool: &PgPool, original_id: Uuid) -> Vec<String> {
    sqlx::query_as::<_, (String,)>(
        "SELECT visibility FROM statuses
          WHERE reblog_of_id = $1 AND deleted_at IS NULL
          ORDER BY id",
    )
    .bind(original_id)
    .fetch_all(pool)
    .await
    .expect("the rows read")
    .into_iter()
    .map(|(visibility,)| visibility)
    .collect()
}

/// The stored `reblogs_count` of status `id`.
async fn reblogs_count(pool: &PgPool, id: Uuid) -> i32 {
    let row: (i32,) = sqlx::query_as("SELECT reblogs_count FROM statuses WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the counter reads");
    row.0
}

/// The stored `statuses_count` of account `id`.
async fn statuses_count(pool: &PgPool, id: Uuid) -> i32 {
    user::get_by_id(pool, id)
        .await
        .expect("the user reads")
        .expect("the user exists")
        .statuses_count
}

/// Whether the status row `id` names is a tombstone.
async fn is_tombstone(pool: &PgPool, id: &str) -> bool {
    let uuid = Uuid::parse_str(id).expect("the fixture id is a uuid");
    sqlx::query_as::<_, (bool,)>("SELECT deleted_at IS NOT NULL FROM statuses WHERE id = $1")
        .bind(uuid)
        .fetch_one(pool)
        .await
        .expect("the row reads")
        .0
}

/// The account ids of an [`Account`] array, in answer order.
///
/// [`Account`]: wired_be::entities::account::Account
fn account_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the answer is an array")
        .iter()
        .map(|entry| entry["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// A `Link` header keyed on reblog row ids: `next` walks to `oldest`, `prev`
/// back to `newest`, both on the route's own `path`.
fn assert_boost_keyed_link(link: Option<&str>, path: &str, newest: Uuid, oldest: Uuid) {
    let value = link.expect("a non-empty page carries a Link header");
    assert!(
        value.contains(&format!("{path}?max_id={oldest}>; rel=\"next\"")),
        "next keys on the oldest reblog row id: {value}"
    );
    assert!(
        value.contains(&format!("{path}?min_id={newest}>; rel=\"prev\"")),
        "prev keys on the newest reblog row id: {value}"
    );
}

#[tokio::test]
async fn reblog_writes_the_activity_row_and_both_counters_step() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _hers) = booster(&pool, "alice").await;
    let (bob, his_token) = booster(&pool, "bob").await;
    let original = post(&pool, &alice).await;

    let body = reblogged(&app, &his_token, &original.id.to_string()).await;

    let reblog_id = body["id"].as_str().expect("the answer has an id");
    assert_ne!(
        reblog_id,
        original.id.to_string(),
        "the answer is the boost"
    );
    assert_eq!(
        body["uri"],
        format!("{TEST_BASE_URL}/activities/{reblog_id}"),
        "a reblog row is the Announce: its uri is an activity id (decision 6)"
    );
    assert_eq!(body["content"], "", "the outer status carries no text");
    assert_eq!(
        body["visibility"], "public",
        "with no parameter the boost is public"
    );
    assert_eq!(
        body["account"]["id"],
        bob.id.to_string(),
        "the reblogger speaks"
    );
    assert_eq!(body["reblogged"], true, "one's own boost flags itself");
    assert_eq!(
        body["reblog"]["id"],
        original.id.to_string(),
        "and the original nests inside"
    );
    assert_eq!(
        body["reblog"]["reblogged"], true,
        "the original carries the flag too"
    );
    assert_eq!(
        body["reblog"]["reblogs_count"], 1,
        "the nested original counts the boost"
    );
    assert_eq!(
        body["reblogs_count"], 0,
        "the boost row counts nothing of its own"
    );

    let all = stored_reblogs(&pool).await;
    assert_eq!(
        all.as_slice(),
        [(Uuid::parse_str(reblog_id).unwrap(), bob.id)],
        "one live reblog row, naming its reblogger"
    );
    assert_eq!(
        reblogs_count(&pool, original.id).await,
        1,
        "the original counts the boost"
    );
    assert_eq!(
        statuses_count(&pool, bob.id).await,
        1,
        "and bob counts it as his own"
    );
    assert_eq!(
        statuses_count(&pool, alice.id).await,
        1,
        "the author's own count is just her post"
    );

    let shown = show(&app, Some(&his_token), reblog_id).await;
    common::assert_answer(&shown, StatusCode::OK, "the boost reads back by its own id");
    assert_eq!(
        shown.json_value()["reblog"]["id"],
        original.id.to_string(),
        "nested exactly as the write answered"
    );
}

#[tokio::test]
async fn reblogging_twice_restates_the_same_row_and_count() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _hers) = booster(&pool, "alice").await;
    let (bob, his_token) = booster(&pool, "bob").await;
    let original = post(&pool, &alice).await;
    let id = original.id.to_string();

    let first = reblogged(&app, &his_token, &id).await;
    let again = reblogged(&app, &his_token, &id).await;

    assert_eq!(again["id"], first["id"], "the repeat answers the same row");
    assert_eq!(again["reblogged"], true, "and the same entity");
    assert_eq!(
        stored_reblogs(&pool).await.len(),
        1,
        "one live reblog per pair, however many calls"
    );
    assert_eq!(reblogs_count(&pool, original.id).await, 1, "one count");
    assert_eq!(
        statuses_count(&pool, bob.id).await,
        1,
        "and one step for the reblogger"
    );

    let withdrawn = unreblog_route(&app, &his_token, &id).await;
    common::assert_answer(&withdrawn, StatusCode::OK, "the withdrawal answers");
    let second_living = reblogged(&app, &his_token, &id).await;
    assert_ne!(
        second_living["id"], first["id"],
        "and a boost after the withdrawal is a new row"
    );
    assert_eq!(
        reblogs_count(&pool, original.id).await,
        1,
        "counted once again"
    );
}

#[tokio::test]
async fn unreblog_returns_the_original_and_undoes_the_counts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _hers) = booster(&pool, "alice").await;
    let (bob, his_token) = booster(&pool, "bob").await;
    let original = post(&pool, &alice).await;

    let boost_id = reblogged(&app, &his_token, &original.id.to_string()).await["id"]
        .as_str()
        .expect("the boost has an id")
        .to_owned();

    let response = unreblog_route(&app, &his_token, &original.id.to_string()).await;
    common::assert_answer(&response, StatusCode::OK, "the withdrawal answers");
    let body = response.json_value();
    assert_eq!(
        body["id"],
        original.id.to_string(),
        "the unreblog answers the ORIGINAL status"
    );
    assert_eq!(body["reblogged"], false, "and its own flag is clear");
    assert_eq!(body["reblogs_count"], 0, "the count stepped back");
    assert_eq!(body["reblog"], Value::Null, "the original nests nothing");

    assert!(
        stored_reblogs(&pool).await.is_empty(),
        "the boost row left the live set"
    );
    assert!(
        is_tombstone(&pool, &boost_id).await,
        "the boost row is a tombstone, not gone"
    );
    assert_eq!(
        statuses_count(&pool, bob.id).await,
        0,
        "and bob stops counting it"
    );

    let restated = unreblog_route(&app, &his_token, &original.id.to_string()).await;
    common::assert_answer(
        &restated,
        StatusCode::OK,
        "withdrawing twice is not an error",
    );
    assert_eq!(
        restated.json_value()["id"],
        original.id.to_string(),
        "and answers the unchanged original"
    );
    assert_eq!(
        reblogs_count(&pool, original.id).await,
        0,
        "stepped down only once"
    );
}

#[tokio::test]
async fn unreblog_by_the_boost_id_withdraws_through_the_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _hers) = booster(&pool, "alice").await;
    let (_bob, his_token) = booster(&pool, "bob").await;
    let original = post(&pool, &alice).await;
    let boost = reblogged(&app, &his_token, &original.id.to_string()).await;
    let boost_id = boost["id"]
        .as_str()
        .expect("the boost has an id")
        .to_owned();

    let response = unreblog_route(&app, &his_token, &boost_id).await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        "unreblogging the boost's own id works through it",
    );
    assert_eq!(
        response.json_value()["id"],
        original.id.to_string(),
        "and the answer is still the original"
    );
    assert!(
        stored_reblogs(&pool).await.is_empty(),
        "the row was withdrawn"
    );
}

#[tokio::test]
async fn unreblogging_a_status_never_boosted_is_the_unchanged_original() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _hers) = booster(&pool, "alice").await;
    let (_bob, his_token) = booster(&pool, "bob").await;
    let original = post(&pool, &alice).await;

    let response = unreblog_route(&app, &his_token, &original.id.to_string()).await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        "a withdrawal with nothing to withdraw",
    );
    let body = response.json_value();
    assert_eq!(
        body["id"],
        original.id.to_string(),
        "the original, unchanged"
    );
    assert_eq!(body["reblogged"], false, "never flagged, so never cleared");
    assert!(
        stored_reblogs(&pool).await.is_empty(),
        "and nothing was written"
    );
}

#[tokio::test]
async fn a_private_or_direct_target_refuses_the_boost() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, her_token) = booster(&pool, "alice").await;

    for visibility in [Visibility::Private, Visibility::Direct] {
        let hidden = post_with(&pool, &alice, visibility).await;
        let response = reblog_route(&app, &her_token, &hidden.id.to_string()).await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("one's own {visibility} post"),
        );
        common::assert_error(&response, CANNOT_REBLOG, "the address refusal");
    }
    assert!(
        stored_reblogs(&pool).await.is_empty(),
        "neither refusal wrote a row"
    );
}

#[tokio::test]
async fn a_reblog_of_a_reblog_reblogs_the_original() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _hers) = booster(&pool, "alice").await;
    let (_bob, his_token) = booster(&pool, "bob").await;
    let (_carol, her_token) = booster(&pool, "carol").await;
    let original = post(&pool, &alice).await;
    let boost = reblogged(&app, &his_token, &original.id.to_string()).await;
    let boost_id = boost["id"]
        .as_str()
        .expect("the boost has an id")
        .to_owned();

    let for_carol = reblogged(&app, &her_token, &boost_id).await;

    assert_ne!(
        for_carol["id"], boost_id,
        "carol's boost is her own new row"
    );
    assert_eq!(
        for_carol["reblog"]["id"],
        original.id.to_string(),
        "and it nests the original — nobody boosts a boost"
    );
    assert_eq!(
        for_carol["reblog"]["reblogs_count"], 2,
        "the original counts both boosts"
    );
    let pairs = stored_reblogs(&pool).await;
    assert_eq!(pairs.len(), 2, "two live reblog rows");
    let named: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
        "SELECT reblog_of_id FROM statuses
          WHERE reblog_of_id IS NOT NULL AND deleted_at IS NULL",
    )
    .fetch_all(&pool)
    .await
    .expect("the rows read")
    .into_iter()
    .map(|(target,)| target)
    .collect();
    assert_eq!(
        named,
        vec![original.id, original.id],
        "both point at alice's post, never at bob's row"
    );
    assert_eq!(
        reblogs_count(&pool, original.id).await,
        2,
        "and it counts both"
    );
}

#[tokio::test]
async fn the_visibility_parameter_wears_the_boost_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _hers) = booster(&pool, "alice").await;
    let (_bob, his_token) = booster(&pool, "bob").await;
    let (_carol, her_token) = booster(&pool, "carol").await;
    let original = post(&pool, &alice).await;
    let id = original.id.to_string();

    let quiet = reblog_form(&app, &his_token, &id, "unlisted").await;
    common::assert_answer(&quiet, StatusCode::OK, "the unlisted boost stores");
    assert_eq!(
        quiet.json_value()["visibility"],
        "unlisted",
        "the outer entity wears it"
    );

    let followers_only =
        reblog_json(&app, &her_token, &id, &json!({"visibility": "private"})).await;
    common::assert_answer(&followers_only, StatusCode::OK, "the private boost stores");
    assert_eq!(
        followers_only.json_value()["visibility"],
        "private",
        "a followers-only boost is allowed"
    );

    assert_eq!(
        stored_reblog_visibilities(&pool, original.id).await,
        vec!["unlisted".to_owned(), "private".to_owned()],
        "both rows stored at the visibility their call asked for"
    );
    assert_eq!(
        reblogs_count(&pool, original.id).await,
        2,
        "and the original counts both"
    );
}

#[tokio::test]
async fn a_visibility_the_boost_does_not_take_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _hers) = booster(&pool, "alice").await;
    let (_bob, his_token) = booster(&pool, "bob").await;
    let original = post(&pool, &alice).await;
    let id = original.id.to_string();

    for asked in ["direct", "local", "Public", "audience"] {
        let refused = reblog_form(&app, &his_token, &id, asked).await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("visibility={asked}"),
        );
        common::assert_error(&refused, INVALID_VISIBILITY, "the parameter refusal");
    }
    let empty_spelling = reblog_form(&app, &his_token, &id, "").await;
    common::assert_answer(
        &empty_spelling,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a bare visibility=",
    );
    common::assert_error(&empty_spelling, INVALID_VISIBILITY, "the parameter refusal");
    let as_json = reblog_json(&app, &his_token, &id, &json!({"visibility": "direct"})).await;
    common::assert_answer(
        &as_json,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the same refusal as JSON",
    );
    common::assert_error(&as_json, INVALID_VISIBILITY, "the parameter refusal");

    assert!(
        stored_reblogs(&pool).await.is_empty(),
        "and none of the refusals wrote a row"
    );
    assert_eq!(
        reblogs_count(&pool, original.id).await,
        0,
        "nor stepped a counter"
    );
}

#[tokio::test]
async fn reblogged_by_lists_the_boosters_and_pages_on_boost_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _hers) = booster(&pool, "alice").await;
    let original = post(&pool, &alice).await;
    let id = original.id.to_string();
    let path = format!("/api/v1/statuses/{id}/reblogged_by");

    let mut boosters = Vec::new();
    for n in 0..3 {
        let (account, token) = booster(&pool, &format!("booster{n}")).await;
        reblog_ok(&app, &token, &id).await;
        boosters.push((account, token));
    }
    let all: Vec<Uuid> = stored_reblogs(&pool)
        .await
        .into_iter()
        .map(|(reblog_id, _)| reblog_id)
        .collect();
    assert_eq!(all.len(), 3, "three live boosts");

    let page = reblogged_by(&app, None, &id, "").await;
    common::assert_answer(&page, StatusCode::OK, "the listing answers");
    assert_eq!(
        account_ids(&page.json_value()),
        boosters
            .iter()
            .rev()
            .map(|(account, _)| account.id.to_string())
            .collect::<Vec<String>>(),
        "newest boost first, every booster once"
    );
    assert_boost_keyed_link(page.header("link"), &path, all[0], all[2]);

    let first = reblogged_by(&app, None, &id, "limit=2").await;
    assert_eq!(
        account_ids(&first.json_value()),
        vec![boosters[2].0.id.to_string(), boosters[1].0.id.to_string()],
        "limit cuts the page at the newest two"
    );
    assert_boost_keyed_link(first.header("link"), &path, all[0], all[1]);

    let tail = reblogged_by(&app, None, &id, &format!("max_id={}", all[1])).await;
    assert_eq!(
        account_ids(&tail.json_value()),
        vec![boosters[0].0.id.to_string()],
        "max_id continues where the page ended, keyed on the reblog row id"
    );

    let past = reblogged_by(&app, None, &id, &format!("max_id={}", all[2])).await;
    assert_eq!(
        account_ids(&past.json_value()),
        Vec::<String>::new(),
        "a walk past the oldest boost answers the empty page"
    );
    assert_eq!(past.header("link"), None, "with no Link header");

    let forward = reblogged_by(&app, None, &id, &format!("min_id={}", all[2])).await;
    assert_eq!(
        account_ids(&forward.json_value()),
        vec![boosters[2].0.id.to_string(), boosters[1].0.id.to_string()],
        "min_id answers the newer page, still newest-first"
    );

    let quiet = post(&pool, &alice).await;
    let empty = reblogged_by(&app, None, &quiet.id.to_string(), "").await;
    common::assert_answer(&empty, StatusCode::OK, "an unboosted status answers");
    assert_eq!(
        account_ids(&empty.json_value()),
        Vec::<String>::new(),
        "with the empty list"
    );
    assert_eq!(empty.header("link"), None, "and no Link header");
}

#[tokio::test]
async fn reblogged_by_answers_the_original_through_a_boost_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _hers) = booster(&pool, "alice").await;
    let (bob, his_token) = booster(&pool, "bob").await;
    let (carol, her_token) = booster(&pool, "carol").await;
    let original = post(&pool, &alice).await;
    let boost = reblogged(&app, &his_token, &original.id.to_string()).await;
    reblog_ok(&app, &her_token, &original.id.to_string()).await;
    let boost_id = boost["id"]
        .as_str()
        .expect("the boost has an id")
        .to_owned();

    let through_the_row = reblogged_by(&app, None, &boost_id, "").await;
    common::assert_answer(
        &through_the_row,
        StatusCode::OK,
        "a boost's id answers its original's list",
    );
    assert_eq!(
        account_ids(&through_the_row.json_value()),
        vec![carol.id.to_string(), bob.id.to_string()],
        "the boosters of the original, newest boost first"
    );
}

#[tokio::test]
async fn a_withdrawn_boost_leaves_the_listing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _hers) = booster(&pool, "alice").await;
    let (_bob, his_token) = booster(&pool, "bob").await;
    let (carol, her_token) = booster(&pool, "carol").await;
    let original = post(&pool, &alice).await;
    let id = original.id.to_string();
    reblog_ok(&app, &his_token, &id).await;
    reblog_ok(&app, &her_token, &id).await;

    let withdrawn = unreblog_route(&app, &his_token, &id).await;
    common::assert_answer(&withdrawn, StatusCode::OK, "bob withdraws");

    let page = reblogged_by(&app, None, &id, "").await;
    assert_eq!(
        account_ids(&page.json_value()),
        vec![carol.id.to_string()],
        "a tombstoned boost names no reblogger"
    );
}

#[tokio::test]
async fn the_reblogged_flag_follows_the_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, her_token) = booster(&pool, "alice").await;
    let original = post(&pool, &alice).await;
    let id = original.id.to_string();
    reblog_ok(&app, &her_token, &id).await;

    let for_alice = show(&app, Some(&her_token), &id).await.json_value();
    assert_eq!(for_alice["reblogged"], true, "alice boosted it");

    let (_dave, dave_token) = booster(&pool, "dave").await;
    let for_dave = show(&app, Some(&dave_token), &id).await.json_value();
    assert_eq!(
        for_dave["reblogged"], false,
        "another account's boost is not mine"
    );

    let anonymous = show(&app, None, &id).await.json_value();
    assert_eq!(
        anonymous["reblogged"], false,
        "an anonymous read flags nothing"
    );

    let batch = common::TestRequest::get(&format!("/api/v1/statuses?id[]={id}"))
        .bearer(&her_token)
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        batch[0]["reblogged"], true,
        "the page renderer answers the flag from its batched read"
    );

    let withdrawn = unreblog_route(&app, &her_token, &id).await;
    common::assert_answer(&withdrawn, StatusCode::OK, "alice withdraws");
    let after = show(&app, Some(&her_token), &id).await.json_value();
    assert_eq!(
        after["reblogged"], false,
        "and the flag clears with the row"
    );
}

#[tokio::test]
async fn what_the_gate_hides_is_the_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, her_token) = booster(&pool, "alice").await;
    let stranger = common::create_local_user(&pool, "secret").await;
    let hidden = post_with(&pool, &stranger, Visibility::Private).await;
    let deleted = post(&pool, &alice).await;
    let tombstone = status::soft_delete_with_reblogs(&pool, deleted.id)
        .await
        .expect("the delete runs");
    assert!(tombstone.is_some(), "the fixture post was live");

    for id in [
        hidden.id.to_string(),
        deleted.id.to_string(),
        UNKNOWN_ID.to_owned(),
        "not-an-uuid".to_owned(),
    ] {
        common::assert_all_refused(
            &[
                reblog_route(&app, &her_token, &id).await,
                unreblog_route(&app, &her_token, &id).await,
            ],
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            "what the gate hides, on both writes",
        );
        let listed = reblogged_by(&app, None, &id, "").await;
        common::assert_answer(&listed, StatusCode::NOT_FOUND, "the listing too");
        common::assert_error(&listed, NOT_FOUND, "the listing too");
    }
    assert!(
        stored_reblogs(&pool).await.is_empty(),
        "nothing was boosted behind a 404"
    );
}

#[tokio::test]
async fn the_write_gate_demands_a_user_with_the_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, read_only) = scoped(&pool, "viewer", "read").await;
    let (_pinner, wrong_write) = scoped(&pool, "pinner", "write:accounts read").await;
    let status = post_by(&pool, "bob").await;
    let id = status.id.to_string();

    let nobody = reblog_route(&app, "", &id).await;
    common::assert_answer(&nobody, StatusCode::UNAUTHORIZED, "no usable token");
    common::assert_error(&nobody, NO_TOKEN, "no usable token");
    let nobody_off = unreblog_route(&app, "", &id).await;
    common::assert_answer(&nobody_off, StatusCode::UNAUTHORIZED, "no usable token");
    let bogus = reblog_route(&app, "not-a-token", &id).await;
    common::assert_answer(
        &bogus,
        StatusCode::UNAUTHORIZED,
        "a token that does not resolve",
    );
    common::assert_error(&bogus, BAD_TOKEN, "an unresolvable token");

    let app_only = common::create_app_only_token(&pool, "read write").await;
    let headless = reblog_route(&app, &app_only, &id).await;
    common::assert_answer(&headless, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(&headless, USER_REQUIRED, "an app-only token");

    common::assert_all_refused(
        &[
            reblog_route(&app, &read_only, &id).await,
            unreblog_route(&app, &read_only, &id).await,
            reblog_route(&app, &wrong_write, &id).await,
            unreblog_route(&app, &wrong_write, &id).await,
        ],
        StatusCode::FORBIDDEN,
        NO_WRITE,
        "a token that does not grant write:statuses",
    );
    assert!(
        stored_reblogs(&pool).await.is_empty(),
        "nothing was refused in vain"
    );
}

#[tokio::test]
async fn reblogged_by_demands_read_from_a_presented_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, write_only) = scoped(&pool, "alice", "write:statuses").await;
    let status = post_by(&pool, "bob").await;
    let id = status.id.to_string();

    let refused = reblogged_by(&app, Some(&write_only), &id, "").await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "a user token that presents without read:statuses",
    );
    common::assert_error(
        &refused,
        "Insufficient permissions: read:statuses.",
        "the scope refusal",
    );

    let anonymous = reblogged_by(&app, None, &id, "").await;
    common::assert_answer(&anonymous, StatusCode::OK, "an anonymous read proceeds");
    assert_eq!(
        account_ids(&anonymous.json_value()),
        Vec::<String>::new(),
        "and answers the empty list of an unboosted post"
    );

    let malformed = reblogged_by(&app, None, &id, "max_id=nope").await;
    common::assert_answer(
        &malformed,
        StatusCode::BAD_REQUEST,
        "a malformed cursor is the page extractor's 400",
    );
}
