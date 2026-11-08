//! Favourites over HTTP (WBE-80): `POST /api/v1/statuses/{id}/favourite`
//! and its `…/unfavourite` twin, `GET /api/v1/statuses/{id}/favourited_by`
//! and `GET /api/v1/favourites`.
//!
//! The writes are Pleroma's `favourite`/`unfavourite`: a user token with
//! `write:favourites` on the `statuses_actions` bucket, the status resolved
//! through the shared visibility gate (a malformed, unknown, deleted or
//! invisible id is the one 404), and the answer the [`Status`] entity as the
//! caller sees it — `favourited` true or false and `favourites_count`
//! stepped, because the row is re-read for the render. A reblog's id
//! favourites the *original* (Mastodon's behaviour) and answers the reblog
//! entity. Both writes are idempotent: a repeat favourite counts once, and a
//! withdrawal that found nothing to remove is the unchanged status with 200.
//!
//! `favourited_by` answers the [`Account`] entities behind a status's
//! favourites, newest like first, paged and `Link`-keyed on the *favourite
//! row ids* (20 by default, 80 at most); the author's `hide_favorites` —
//! true by default — never gates this list, and a presented user token only
//! needs `read:statuses`. `GET /api/v1/favourites` is the caller's own list
//! under `read:favourites`: statuses newest-favourite first, the same
//! like-id cursors, and narrowed to what the caller may still see.
//!
//! [`Status`]: wired_be::entities::status::Status
//! [`Account`]: wired_be::entities::account::Account

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        status::{self, NewStatus, Visibility},
        user::User,
    },
};

use crate::common;

/// The 404 body of the shared status gate.
const NOT_FOUND: &str = "Record not found";
/// The 401 body `AuthUser` sends when no usable token was presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for an app-only token on a route that needs a user.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 wording of the favourite write gate.
const NO_WRITE: &str = "Insufficient permissions: write:favourites.";
/// The 403 wording of the favourites list gate.
const NO_READ: &str = "Insufficient permissions: read:favourites.";
/// The 403 wording of the optional read gate, for a presented token
/// without it.
const NO_STATUSES_READ: &str = "Insufficient permissions: read:statuses.";
/// The 401 body for a token that does not resolve.
const BAD_TOKEN: &str = "The access token is invalid";
/// The 400 body of a malformed pagination cursor.
const BAD_ID: &str = "Invalid id";
/// An id that names no stored status.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// How many favourites the pagination fixtures seed.
const PAGE_SEED: usize = 3;

/// A local user named `nickname` with a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` who may favourite, read their own list,
/// and read statuses through a presented token (`read` covers the tree).
async fn liker(pool: &PgPool, nickname: &str) -> (User, String) {
    fixture(pool, nickname, "write:favourites read").await
}

/// A public post by `author`.
async fn post(pool: &PgPool, author: &User) -> status::Status {
    common::create_status(pool, author, "a post", Visibility::Public).await
}

/// A public post by a fresh local author named `nickname`.
async fn post_by(pool: &PgPool, nickname: &str) -> status::Status {
    let author = common::create_local_user(pool, nickname).await;
    post(pool, &author).await
}

/// A reblog `reblogger` wrote of `original`, as its row.
async fn reblog(pool: &PgPool, reblogger: &User, original: Uuid) -> status::Status {
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

/// `POST …/favourite` for status `id`.
async fn favourite_route(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/favourite"))
        .bearer(token)
        .send(app)
        .await
}

/// `POST …/unfavourite` for status `id`.
async fn unfavourite_route(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/unfavourite"))
        .bearer(token)
        .send(app)
        .await
}

/// A favourite write a test only needs to have happened, answered as JSON.
async fn favourited(app: &Router, token: &str, id: &str) -> Value {
    let response = favourite_route(app, token, id).await;
    common::assert_answer(&response, StatusCode::OK, "the favourite stores");
    response.json_value()
}

/// A favourite write a test only needs to have happened: the row is
/// stored, and the body it answers is the test's business elsewhere.
async fn like_ok(app: &Router, token: &str, id: &str) {
    let response = favourite_route(app, token, id).await;
    common::assert_answer(&response, StatusCode::OK, "the favourite stores");
}

/// A `GET …/favourited_by[?query]`, anonymously when `token` is `None`.
async fn favourited_by(
    app: &Router,
    token: Option<&str>,
    id: &str,
    query: &str,
) -> common::TestResponse {
    let suffix = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}/favourited_by{suffix}"));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// A `GET /api/v1/favourites[?query]` as `token`.
async fn favourites(app: &Router, token: &str, query: &str) -> common::TestResponse {
    let suffix = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    common::TestRequest::get(&format!("/api/v1/favourites{suffix}"))
        .bearer(token)
        .send(app)
        .await
}

/// A `GET /api/v1/statuses/{id}`, anonymously when `token` is `None`.
async fn show(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}"));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// The stored `favourites_count` of status `id`.
async fn stored_count(pool: &PgPool, id: Uuid) -> i32 {
    let row: (i32,) = sqlx::query_as("SELECT favourites_count FROM statuses WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the counter reads");
    row.0
}

/// How many favourite rows exist in the table.
async fn row_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM favourites")
        .fetch_one(pool)
        .await
        .expect("the table counts");
    row.0
}

/// The favourite row ids of `user_id`'s likes, newest first — the order
/// the list pages in and the cursors walk.
async fn like_ids(pool: &PgPool, user_id: Uuid) -> Vec<Uuid> {
    sqlx::query_as::<_, (Uuid,)>(
        "SELECT f.id FROM favourites f WHERE f.user_id = $1 ORDER BY f.id DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("the like ids read")
    .into_iter()
    .map(|(id,)| id)
    .collect()
}

/// The live likes of `status_id` as `(like id, liker id)`, newest first —
/// the answer `favourited_by` must give.
async fn likes_of(pool: &PgPool, status_id: Uuid) -> Vec<(Uuid, Uuid)> {
    sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT f.id, f.user_id FROM favourites f \
           JOIN statuses s ON s.id = f.status_id \
          WHERE f.status_id = $1 AND s.deleted_at IS NULL \
          ORDER BY f.id DESC",
    )
    .bind(status_id)
    .fetch_all(pool)
    .await
    .expect("the like rows read")
}

/// The `(like id, status id)` pairs of `user_id`'s live likes, newest
/// like first — the answer the favourites list must give while all three
/// statuses are still public.
async fn liked_pairs(pool: &PgPool, user_id: Uuid) -> Vec<(Uuid, Uuid)> {
    sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT f.id, s.id FROM favourites f \
           JOIN statuses s ON s.id = f.status_id \
          WHERE f.user_id = $1 AND s.deleted_at IS NULL \
          ORDER BY f.id DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("the pairs read")
}

/// The account ids of an `Account` array, in answer order.
fn account_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the answer is an array")
        .iter()
        .map(|entry| entry["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The status ids of a `Status` array, in answer order.
fn status_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the answer is an array")
        .iter()
        .map(|entry| entry["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// A `Link` header keyed on favourite row ids: `next` walks to `oldest`,
/// `prev` back to `newest`.
fn assert_like_keyed_link(link: Option<&str>, path: &str, newest: Uuid, oldest: Uuid) {
    let value = link.expect("a non-empty page carries a Link header");
    assert!(
        value.contains(&format!("{path}?max_id={oldest}>; rel=\"next\"")),
        "next walks to the oldest like: {value}"
    );
    assert!(
        value.contains(&format!("{path}?min_id={newest}>; rel=\"prev\"")),
        "prev walks back to the newest like: {value}"
    );
}

#[tokio::test]
async fn favouriting_marks_the_status_and_counts_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = liker(&pool, "alice").await;
    let status = post_by(&pool, "bob").await;
    let id = status.id.to_string();

    let first = favourited(&app, &token, &id).await;
    assert_eq!(first["id"], id, "the answer is the status entity");
    assert_eq!(first["favourited"], true, "flagged for its favouriter");
    assert_eq!(first["favourites_count"], 1, "and counted once");
    assert_eq!(
        stored_count(&pool, status.id).await,
        1,
        "the stored counter"
    );

    let again = favourited(&app, &token, &id).await;
    assert_eq!(again["favourited"], true, "a repeat restates the flag");
    assert_eq!(again["favourites_count"], 1, "but never counts twice");
    assert_eq!(row_count(&pool).await, 1, "one row for the pair");

    let shown = show(&app, Some(&token), &id).await;
    common::assert_answer(&shown, StatusCode::OK, "the show route answers");
    assert_eq!(
        shown.json_value()["favourited"],
        true,
        "the batched favourites read flags the status for its liker"
    );
    let anonymous = show(&app, None, &id).await;
    assert_eq!(
        anonymous.json_value()["favourited"],
        false,
        "and it is nobody's favourite for an anonymous viewer"
    );
}

#[tokio::test]
async fn unfavouriting_clears_the_flag_and_is_idempotent() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = liker(&pool, "alice").await;
    let status = post_by(&pool, "bob").await;
    let id = status.id.to_string();
    like_ok(&app, &token, &id).await;

    let gone = unfavourite_route(&app, &token, &id).await;
    common::assert_answer(&gone, StatusCode::OK, "the withdrawal answers");
    assert_eq!(gone.json_value()["favourited"], false, "the flag clears");
    assert_eq!(gone.json_value()["favourites_count"], 0, "the count steps");
    assert_eq!(stored_count(&pool, status.id).await, 0, "in the row too");

    let again = unfavourite_route(&app, &token, &id).await;
    common::assert_answer(&again, StatusCode::OK, "a no-op withdrawal is not an error");
    assert_eq!(
        again.json_value()["favourites_count"],
        0,
        "and steps nothing further"
    );
    assert_eq!(row_count(&pool).await, 0, "no rows left");
}

#[tokio::test]
async fn favouriting_a_reblog_likes_the_original() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = liker(&pool, "alice").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let original = post_by(&pool, "bob").await;
    let boost = reblog(&pool, &carol, original.id).await;

    let entity = favourited(&app, &token, &boost.id.to_string()).await;
    assert_eq!(entity["id"], boost.id.to_string(), "the reblog answers");
    assert_eq!(
        entity["favourites_count"], 0,
        "the reblog row's own counter never moved"
    );
    assert_eq!(
        entity["reblog"]["id"],
        original.id.to_string(),
        "with the original nested"
    );
    assert_eq!(
        entity["reblog"]["favourites_count"], 1,
        "the like counted where it lives"
    );
    assert_eq!(
        entity["favourited"], true,
        "the reblog entity is flagged through its original"
    );
    assert_eq!(
        entity["reblog"]["favourited"], true,
        "and so is the nested original"
    );
    assert_eq!(
        stored_count(&pool, original.id).await,
        1,
        "the stored counter sits on the original"
    );
    let keyed: (Uuid,) = sqlx::query_as("SELECT status_id FROM favourites")
        .fetch_one(&pool)
        .await
        .expect("the row reads");
    assert_eq!(keyed.0, original.id, "one row, keyed to the original's id");

    let back = unfavourite_route(&app, &token, &boost.id.to_string()).await;
    common::assert_answer(&back, StatusCode::OK, "unliking a boost runs");
    assert_eq!(
        back.json_value()["favourited"],
        false,
        "and clears the boost's borrowed flag"
    );
    assert_eq!(
        stored_count(&pool, original.id).await,
        0,
        "the original steps back"
    );
}

#[tokio::test]
async fn an_invisible_status_is_the_shared_four_zero_four() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = liker(&pool, "alice").await;
    let author = common::create_local_user(&pool, "secret").await;
    let hidden = common::create_status(&pool, &author, "for followers", Visibility::Private).await;
    let hidden_id = hidden.id.to_string();

    for response in [
        favourite_route(&app, &token, &hidden_id).await,
        unfavourite_route(&app, &token, &hidden_id).await,
        favourited_by(&app, Some(&token), &hidden_id, "").await,
    ] {
        common::assert_answer(&response, StatusCode::NOT_FOUND, "a private post hides");
        common::assert_error(&response, NOT_FOUND, "the one indistinguishable 404");
    }
    assert_eq!(
        row_count(&pool).await,
        0,
        "nothing was liked behind the gate"
    );

    for bogus in [UNKNOWN_ID, "not-an-uuid"] {
        for response in [
            favourite_route(&app, &token, bogus).await,
            unfavourite_route(&app, &token, bogus).await,
            favourited_by(&app, None, bogus, "").await,
        ] {
            common::assert_answer(
                &response,
                StatusCode::NOT_FOUND,
                "an unknown or malformed id",
            );
            common::assert_error(&response, NOT_FOUND, "the same 404 wording");
        }
    }
}

#[tokio::test]
async fn the_write_gate_demands_a_user_with_the_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, read_only) = fixture(&pool, "viewer", "read").await;
    let status = post_by(&pool, "bob").await;
    let id = status.id.to_string();

    let nobody = favourite_route(&app, "", &id).await;
    common::assert_answer(&nobody, StatusCode::UNAUTHORIZED, "no usable token");
    common::assert_error(&nobody, NO_TOKEN, "no usable token");
    let bogus = favourite_route(&app, "not-a-token", &id).await;
    common::assert_answer(
        &bogus,
        StatusCode::UNAUTHORIZED,
        "a token that does not resolve",
    );
    common::assert_error(&bogus, BAD_TOKEN, "an unresolvable token");

    let app_only = common::create_app_only_token(&pool, "read write").await;
    let headless = favourite_route(&app, &app_only, &id).await;
    common::assert_answer(&headless, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(&headless, USER_REQUIRED, "an app-only token");

    for response in [
        favourite_route(&app, &read_only, &id).await,
        unfavourite_route(&app, &read_only, &id).await,
    ] {
        common::assert_answer(
            &response,
            StatusCode::FORBIDDEN,
            "a token without the scope",
        );
        common::assert_error(&response, NO_WRITE, "the write gate");
    }
    assert_eq!(row_count(&pool).await, 0, "nothing was refused in vain");
}

#[tokio::test]
async fn the_favourites_list_demands_its_own_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_write_only, write_token) = fixture(&pool, "writer", "write:favourites").await;

    let nobody = favourites(&app, "", "").await;
    common::assert_answer(&nobody, StatusCode::UNAUTHORIZED, "the list needs a user");
    common::assert_error(&nobody, NO_TOKEN, "no token at all");
    let refused = favourites(&app, &write_token, "").await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "write is not read");
    common::assert_error(&refused, NO_READ, "the list gate");
    let malformed = favourites(&app, &write_token, "max_id=nope").await;
    common::assert_answer(&malformed, StatusCode::BAD_REQUEST, "a bogus cursor");
    common::assert_error(&malformed, BAD_ID, "the cursor wording");
}

#[tokio::test]
async fn favourited_by_lists_accounts_keyed_on_like_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "author").await;
    let status = post(&pool, &author).await;
    let status_id = status.id.to_string();
    let path = format!("/api/v1/statuses/{status_id}/favourited_by");
    for n in 0..PAGE_SEED {
        let (_liker, token) = liker(&pool, &format!("liker{n}")).await;
        like_ok(&app, &token, &status_id).await;
    }
    let all = likes_of(&pool, status.id).await;

    let page = favourited_by(&app, None, &status_id, "").await;
    common::assert_answer(&page, StatusCode::OK, "the list answers anonymously");
    let body = page.json_value();
    let want: Vec<String> = all
        .iter()
        .map(|(_, liker_id)| liker_id.to_string())
        .collect();
    assert_eq!(account_ids(&body), want, "newest like first");
    assert_eq!(
        body[0]["pleroma"]["relationship"],
        Value::Object(Map::new()),
        "the entries embed no relationship"
    );
    assert_like_keyed_link(page.header("link"), &path, all[0].0, all[2].0);

    let first = favourited_by(&app, None, &status_id, "limit=2").await;
    common::assert_answer(&first, StatusCode::OK, "a sized page answers");
    assert_eq!(
        account_ids(&first.json_value()),
        vec![all[0].1.to_string(), all[1].1.to_string()],
        "the two newest likes"
    );
    assert_like_keyed_link(first.header("link"), &path, all[0].0, all[1].0);

    let tail = favourited_by(&app, None, &status_id, &format!("max_id={}", all[1].0)).await;
    common::assert_answer(&tail, StatusCode::OK, "the continuation answers");
    assert_eq!(
        account_ids(&tail.json_value()),
        vec![all[2].1.to_string()],
        "max_id walks to the older like, keyed on the like id"
    );

    let forward = favourited_by(&app, None, &status_id, &format!("min_id={}", all[2].0)).await;
    assert_eq!(
        account_ids(&forward.json_value()),
        vec![all[0].1.to_string(), all[1].1.to_string()],
        "min_id answers the newer page, still newest-first"
    );

    let capped = favourited_by(&app, None, &status_id, "limit=80").await;
    common::assert_answer(&capped, StatusCode::OK, "the cap admits an 80-page");

    let quiet = post_by(&pool, "quiet").await;
    let empty = favourited_by(&app, None, &quiet.id.to_string(), "").await;
    common::assert_answer(&empty, StatusCode::OK, "an unfavourited status answers");
    assert_eq!(
        empty.json_value().as_array().map(Vec::len),
        Some(0),
        "the empty array"
    );
    assert_eq!(empty.header("link"), None, "and no Link header");
}

#[tokio::test]
async fn hide_favorites_never_gates_the_favourited_by_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    // The author's `hide_favorites` is true by default: the flag hides
    // *their* favourites list, never a list of who favourited a post of
    // theirs.
    let (author, token) = fixture(&pool, "hider", "read").await;
    assert!(
        author.hide_favorites.get(),
        "the fixture author hides their favourites by default"
    );
    let status = post(&pool, &author).await;
    let (liker, liker_token) = liker(&pool, "fan").await;
    like_ok(&app, &liker_token, &status.id.to_string()).await;

    let as_author = favourited_by(&app, Some(&token), &status.id.to_string(), "").await;
    common::assert_answer(&as_author, StatusCode::OK, "the author sees the list");
    assert_eq!(
        account_ids(&as_author.json_value()),
        vec![liker.id.to_string()],
        "hide_favorites does not gate it"
    );
    let anonymous = favourited_by(&app, None, &status.id.to_string(), "").await;
    assert_eq!(
        account_ids(&anonymous.json_value()),
        vec![liker.id.to_string()],
        "and neither does it gate anyone else"
    );
}

#[tokio::test]
async fn favourited_by_gates_a_presented_token_by_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_stranger, no_statuses) = fixture(&pool, "stranger", "read:accounts").await;
    let status = post_by(&pool, "bob").await;

    let refused = favourited_by(&app, Some(&no_statuses), &status.id.to_string(), "").await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "a presented token needs the scope",
    );
    common::assert_error(&refused, NO_STATUSES_READ, "the read gate");

    let anonymous = favourited_by(&app, None, &status.id.to_string(), "").await;
    common::assert_answer(
        &anonymous,
        StatusCode::OK,
        "without a token the list answers",
    );
    let malformed = favourited_by(&app, None, &status.id.to_string(), "min_id=nope").await;
    common::assert_answer(&malformed, StatusCode::BAD_REQUEST, "a bogus cursor is 400");
    common::assert_error(&malformed, BAD_ID, "the cursor wording");
}

#[tokio::test]
async fn the_favourites_list_pages_on_like_ids_and_drops_the_invisible() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = liker(&pool, "alice").await;
    for n in 0..PAGE_SEED {
        let author = common::create_local_user(&pool, &format!("author{n}")).await;
        let status = post(&pool, &author).await;
        like_ok(&app, &token, &status.id.to_string()).await;
    }
    let all = like_ids(&pool, alice.id).await;
    let live = liked_pairs(&pool, alice.id).await;

    let page = favourites(&app, &token, "").await;
    common::assert_answer(&page, StatusCode::OK, "the list answers");
    let body = page.json_value();
    let want: Vec<String> = live
        .iter()
        .map(|(_, status_id)| status_id.to_string())
        .collect();
    assert_eq!(status_ids(&body), want, "newest favourite first");
    assert!(
        body.as_array()
            .expect("an array")
            .iter()
            .all(|entry| entry["favourited"] == Value::Bool(true)),
        "every entry on the list is the caller's favourite"
    );
    assert_like_keyed_link(page.header("link"), "/api/v1/favourites", all[0], all[2]);

    let first = favourites(&app, &token, "limit=2").await;
    common::assert_answer(&first, StatusCode::OK, "a sized page answers");
    assert_like_keyed_link(first.header("link"), "/api/v1/favourites", all[0], all[1]);
    let tail = favourites(&app, &token, &format!("max_id={}", all[1])).await;
    common::assert_answer(&tail, StatusCode::OK, "the continuation answers");
    assert_eq!(
        status_ids(&tail.json_value()),
        vec![live[2].1.to_string()],
        "max_id walks alice's own like ids, not status ids"
    );

    // A favourite the viewer can no longer see leaves the list: the
    // visibility step runs after the page walk, exactly like the timeline
    // filters, and the Link edges are the entries that came back.
    let _hidden = sqlx::query("UPDATE statuses SET visibility = 'private' WHERE id = $1")
        .bind(live[1].1)
        .execute(&pool)
        .await
        .expect("the fixture hides the second post");
    let narrowed = favourites(&app, &token, "").await;
    common::assert_answer(&narrowed, StatusCode::OK, "the narrowed page answers");
    assert_eq!(
        status_ids(&narrowed.json_value()),
        vec![live[0].1.to_string(), live[2].1.to_string()],
        "the private one dropped out and the order stayed"
    );
    assert_like_keyed_link(
        narrowed.header("link"),
        "/api/v1/favourites",
        live[0].0,
        live[2].0,
    );

    // A tombstone is off the list too, before the visibility step even
    // gets it: the model pairs likes only with live rows.
    let _deleted = sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
        .bind(live[0].1)
        .execute(&pool)
        .await
        .expect("the fixture tombstones the newest post");
    let rest = favourites(&app, &token, "").await;
    assert_eq!(
        status_ids(&rest.json_value()),
        vec![live[2].1.to_string()],
        "only the still-visible favourite is left"
    );

    let past = favourites(
        &app,
        &token,
        "limit=1&max_id=00000000-0000-7000-8000-000000000000",
    )
    .await;
    common::assert_answer(&past, StatusCode::OK, "a walk past the end answers");
    assert_eq!(
        past.json_value().as_array().map(Vec::len),
        Some(0),
        "the empty page"
    );
    assert_eq!(past.header("link"), None, "with no Link header");
}

#[tokio::test]
async fn the_favourite_routes_spend_their_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("statuses_actions=2:600"),
    );
    let (_alice, token) = liker(&pool, "alice").await;
    let first = post_by(&pool, "bob").await;
    let second = post_by(&pool, "carol").await;

    for (spent, remaining) in [(&first, "1"), (&second, "0")] {
        let response = favourite_route(&app, &token, &spent.id.to_string()).await;
        common::assert_answer(&response, StatusCode::OK, "a request inside the budget");
        assert_eq!(
            response.header("x-ratelimit-limit"),
            Some("2"),
            "the bucket's budget"
        );
        assert_eq!(
            response.header("x-ratelimit-remaining"),
            Some(remaining),
            "the window counts down"
        );
    }

    let throttled = unfavourite_route(&app, &token, &first.id.to_string()).await;
    common::assert_answer(&throttled, StatusCode::TOO_MANY_REQUESTS, "over the budget");
    common::assert_error(
        &throttled,
        "Throttled",
        "the unfavourite twin shares the bucket",
    );
    assert_eq!(
        stored_count(&pool, first.id).await,
        1,
        "the refused withdrawal never reached the handler"
    );
}
