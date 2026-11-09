//! Bookmarks over HTTP (WBE-83): `POST /api/v1/statuses/{id}/bookmark`,
//! its `…/unbookmark` twin, and `GET /api/v1/bookmarks`.
//!
//! The writes are Pleroma's `bookmark`/`unbookmark`: a user token with
//! `write:bookmarks`, the status resolved through the shared visibility
//! gate (a malformed, unknown, deleted or invisible id is the one 404),
//! and the answer the [`Status`] entity as the caller sees it —
//! `bookmarked` true or false, the row re-read for the render. A reblog's
//! id bookmarks *the reblog row itself* (Pleroma's `Bookmark.create` takes
//! the activity the client named), and the answer stays that row's entity.
//! Both writes are idempotent: a repeat bookmark stores one row, and a
//! withdrawal that found nothing to remove is the unchanged status with
//! 200. Nothing federates, so no throttle bucket rides these routes.
//!
//! `GET /api/v1/bookmarks` is the caller's own list under `read:bookmarks`:
//! statuses newest-BOOKMARK first, [`PageParams`] (20 by default, 40 at
//! most) keyed on the bookmark row id, and narrowed to what the caller may
//! still see. The optional `folder_id` query parameter narrows the list to
//! one folder's bookmarks (the folders ticket, WBE-88): a folder holding
//! none of the caller's rows — unknown or another account's — answers the
//! empty page, and a malformed id is the cursor's 400.
//!
//! The writes file bookmarks too: `POST …/bookmark` takes a `folder_id`
//! that must name one of the caller's folders (else the shared 404), a
//! re-bookmark with a different folder moves the row, and the Status
//! entity's `pleroma.bookmark_folder` renders the id or `null`.
//!
//! [`Status`]: wired_be::entities::status::Status

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        bookmark_folder,
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
/// The 403 wording of the bookmark write gate.
const NO_WRITE: &str = "Insufficient permissions: write:bookmarks.";
/// The 403 wording of the bookmarks list gate.
const NO_READ: &str = "Insufficient permissions: read:bookmarks.";
/// The 401 body for a token that does not resolve.
const BAD_TOKEN: &str = "The access token is invalid";
/// The 400 body of a malformed pagination cursor.
const BAD_ID: &str = "Invalid id";
/// An id that names no stored status.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// How many bookmarks the pagination fixtures seed.
const PAGE_SEED: usize = 3;

/// A local user named `nickname` with a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` who may bookmark, read their own list,
/// and read statuses through a presented token (`read` covers the tree).
async fn bookmarker(pool: &PgPool, nickname: &str) -> (User, String) {
    fixture(pool, nickname, "write:bookmarks read").await
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

/// `POST …/bookmark` for status `id`.
async fn bookmark_route(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/bookmark"))
        .bearer(token)
        .send(app)
        .await
}

/// `POST …/unbookmark` for status `id`.
async fn unbookmark_route(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/unbookmark"))
        .bearer(token)
        .send(app)
        .await
}

/// A bookmark write a test only needs to have happened, answered as JSON.
async fn bookmarked(app: &Router, token: &str, id: &str) -> Value {
    let response = bookmark_route(app, token, id).await;
    common::assert_answer(&response, StatusCode::OK, "the bookmark stores");
    response.json_value()
}

/// A bookmark write a test only needs to have happened: the row is
/// stored, and the body it answers is the test's business elsewhere.
async fn bookmark_ok(app: &Router, token: &str, id: &str) {
    let response = bookmark_route(app, token, id).await;
    common::assert_answer(&response, StatusCode::OK, "the bookmark stores");
}

/// A `GET /api/v1/bookmarks[?query]` as `token`.
async fn bookmarks(app: &Router, token: &str, query: &str) -> common::TestResponse {
    let suffix = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    common::TestRequest::get(&format!("/api/v1/bookmarks{suffix}"))
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

/// How many bookmark rows exist in the table.
async fn row_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM bookmarks")
        .fetch_one(pool)
        .await
        .expect("the table counts");
    row.0
}

/// The bookmark row ids of `user_id`, newest first — the order the list
/// pages in and the cursors walk.
async fn bookmark_ids(pool: &PgPool, user_id: Uuid) -> Vec<Uuid> {
    sqlx::query_as::<_, (Uuid,)>("SELECT id FROM bookmarks WHERE user_id = $1 ORDER BY id DESC")
        .bind(user_id)
        .fetch_all(pool)
        .await
        .expect("the bookmark ids read")
        .into_iter()
        .map(|(id,)| id)
        .collect()
}

/// The `(bookmark id, status id)` pairs of `user_id`'s live bookmarks,
/// newest bookmark first — the answer the list must give while every
/// status is still public.
async fn bookmarked_pairs(pool: &PgPool, user_id: Uuid) -> Vec<(Uuid, Uuid)> {
    sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT b.id, s.id FROM bookmarks b \
           JOIN statuses s ON s.id = b.status_id \
          WHERE b.user_id = $1 AND s.deleted_at IS NULL \
          ORDER BY b.id DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("the pairs read")
}

/// The status ids of a `Status` array, in answer order.
fn status_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the answer is an array")
        .iter()
        .map(|entry| entry["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// A `Link` header keyed on bookmark row ids: `next` walks to `oldest`,
/// `prev` back to `newest`.
fn assert_bookmark_keyed_link(link: Option<&str>, path: &str, newest: Uuid, oldest: Uuid) {
    let value = link.expect("a non-empty page carries a Link header");
    assert!(
        value.contains(&format!("{path}?max_id={oldest}>; rel=\"next\"")),
        "next walks to the oldest bookmark: {value}"
    );
    assert!(
        value.contains(&format!("{path}?min_id={newest}>; rel=\"prev\"")),
        "prev walks back to the newest bookmark: {value}"
    );
}

#[tokio::test]
async fn bookmarking_marks_the_status_and_is_idempotent() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = bookmarker(&pool, "alice").await;
    let status = post_by(&pool, "bob").await;
    let id = status.id.to_string();

    let first = bookmarked(&app, &token, &id).await;
    assert_eq!(first["id"], id, "the answer is the status entity");
    assert_eq!(first["bookmarked"], true, "flagged for its bookmarker");
    assert_eq!(
        first["favourites_count"], 0,
        "a bookmark never touches the like counter"
    );

    let again = bookmarked(&app, &token, &id).await;
    assert_eq!(again["bookmarked"], true, "a repeat restates the flag");
    assert_eq!(row_count(&pool).await, 1, "one row for the pair");

    let shown = show(&app, Some(&token), &id).await;
    common::assert_answer(&shown, StatusCode::OK, "the show route answers");
    assert_eq!(
        shown.json_value()["bookmarked"],
        true,
        "the batched bookmarks read flags the status for its bookmarker"
    );
    let anonymous = show(&app, None, &id).await;
    assert_eq!(
        anonymous.json_value()["bookmarked"],
        false,
        "and it is nobody's bookmark for an anonymous viewer"
    );

    let gone = unbookmark_route(&app, &token, &id).await;
    common::assert_answer(&gone, StatusCode::OK, "the withdrawal answers");
    assert_eq!(gone.json_value()["bookmarked"], false, "the flag clears");

    let repeated = unbookmark_route(&app, &token, &id).await;
    common::assert_answer(
        &repeated,
        StatusCode::OK,
        "a no-op withdrawal is not an error",
    );
    assert_eq!(
        repeated.json_value()["bookmarked"],
        false,
        "and it stays cleared"
    );
    assert_eq!(row_count(&pool).await, 0, "no rows left");
}

#[tokio::test]
async fn a_reblog_bookmark_lands_on_the_reblog_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = bookmarker(&pool, "alice").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let original = post_by(&pool, "bob").await;
    let boost = reblog(&pool, &carol, original.id).await;

    let entity = bookmarked(&app, &token, &boost.id.to_string()).await;
    assert_eq!(entity["id"], boost.id.to_string(), "the reblog answers");
    assert_eq!(entity["bookmarked"], true, "the named row carries the flag");
    assert_eq!(
        entity["reblog"]["id"],
        original.id.to_string(),
        "with the original nested"
    );
    assert_eq!(
        entity["reblog"]["bookmarked"], false,
        "and the original, never named, unflagged — unlike a like, which \
         a boost borrows from its original"
    );
    let keyed: (Uuid,) = sqlx::query_as("SELECT status_id FROM bookmarks")
        .fetch_one(&pool)
        .await
        .expect("the row reads");
    assert_eq!(keyed.0, boost.id, "one row, keyed to the reblog's id");

    let back = unbookmark_route(&app, &token, &boost.id.to_string()).await;
    common::assert_answer(&back, StatusCode::OK, "unbookmarking a boost runs");
    assert_eq!(
        back.json_value()["bookmarked"],
        false,
        "and clears the boost's own flag"
    );
}

#[tokio::test]
async fn an_invisible_status_is_the_shared_four_zero_four() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = bookmarker(&pool, "alice").await;
    let author = common::create_local_user(&pool, "secret").await;
    let hidden = common::create_status(&pool, &author, "for followers", Visibility::Private).await;
    let hidden_id = hidden.id.to_string();

    for response in [
        bookmark_route(&app, &token, &hidden_id).await,
        unbookmark_route(&app, &token, &hidden_id).await,
    ] {
        common::assert_answer(&response, StatusCode::NOT_FOUND, "a private post hides");
        common::assert_error(&response, NOT_FOUND, "the one indistinguishable 404");
    }
    assert_eq!(
        row_count(&pool).await,
        0,
        "nothing was bookmarked behind the gate"
    );

    for bogus in [UNKNOWN_ID, "not-an-uuid"] {
        for response in [
            bookmark_route(&app, &token, bogus).await,
            unbookmark_route(&app, &token, bogus).await,
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

    let nobody = bookmark_route(&app, "", &id).await;
    common::assert_answer(&nobody, StatusCode::UNAUTHORIZED, "no usable token");
    common::assert_error(&nobody, NO_TOKEN, "no usable token");
    let bogus = bookmark_route(&app, "not-a-token", &id).await;
    common::assert_answer(
        &bogus,
        StatusCode::UNAUTHORIZED,
        "a token that does not resolve",
    );
    common::assert_error(&bogus, BAD_TOKEN, "an unresolvable token");

    let app_only = common::create_app_only_token(&pool, "read write").await;
    let headless = bookmark_route(&app, &app_only, &id).await;
    common::assert_answer(&headless, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(&headless, USER_REQUIRED, "an app-only token");

    for response in [
        bookmark_route(&app, &read_only, &id).await,
        unbookmark_route(&app, &read_only, &id).await,
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
async fn the_bookmarks_list_demands_its_own_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_write_only, write_token) = fixture(&pool, "writer", "write:bookmarks").await;

    let nobody = bookmarks(&app, "", "").await;
    common::assert_answer(&nobody, StatusCode::UNAUTHORIZED, "the list needs a user");
    common::assert_error(&nobody, NO_TOKEN, "no token at all");
    let refused = bookmarks(&app, &write_token, "").await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "write is not read");
    common::assert_error(&refused, NO_READ, "the list gate");
    let malformed = bookmarks(&app, &write_token, "max_id=nope").await;
    common::assert_answer(&malformed, StatusCode::BAD_REQUEST, "a bogus cursor");
    common::assert_error(&malformed, BAD_ID, "the cursor wording");
}

/// Seeds `PAGE_SEED` public posts by fresh authors and bookmarks them all
/// as a fresh `alice`: answers her row, token, bookmark ids newest-first,
/// and the `(bookmark id, status id)` pairs of her live bookmarks.
async fn seed_list(pool: &PgPool, app: &Router) -> (User, String, Vec<Uuid>, Vec<(Uuid, Uuid)>) {
    let (alice, token) = bookmarker(pool, "alice").await;
    for n in 0..PAGE_SEED {
        let author = common::create_local_user(pool, &format!("author{n}")).await;
        let status = post(pool, &author).await;
        bookmark_ok(app, &token, &status.id.to_string()).await;
    }
    let all = bookmark_ids(pool, alice.id).await;
    let live = bookmarked_pairs(pool, alice.id).await;
    (alice, token, all, live)
}

#[tokio::test]
async fn the_bookmarks_list_pages_on_bookmark_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token, all, live) = seed_list(&pool, &app).await;

    let page = bookmarks(&app, &token, "").await;
    common::assert_answer(&page, StatusCode::OK, "the list answers");
    let body = page.json_value();
    let want: Vec<String> = live
        .iter()
        .map(|(_, status_id)| status_id.to_string())
        .collect();
    assert_eq!(status_ids(&body), want, "newest bookmark first");
    assert!(
        body.as_array()
            .expect("an array")
            .iter()
            .all(|entry| entry["bookmarked"] == Value::Bool(true)),
        "every entry on the list is the caller's bookmark"
    );
    assert_bookmark_keyed_link(page.header("link"), "/api/v1/bookmarks", all[0], all[2]);

    let first = bookmarks(&app, &token, "limit=2").await;
    common::assert_answer(&first, StatusCode::OK, "a sized page answers");
    assert_eq!(
        status_ids(&first.json_value()),
        vec![live[0].1.to_string(), live[1].1.to_string()],
        "the two newest bookmarks"
    );
    assert_bookmark_keyed_link(first.header("link"), "/api/v1/bookmarks", all[0], all[1]);
    let tail = bookmarks(&app, &token, &format!("max_id={}", all[1])).await;
    common::assert_answer(&tail, StatusCode::OK, "the continuation answers");
    assert_eq!(
        status_ids(&tail.json_value()),
        vec![live[2].1.to_string()],
        "max_id walks alice's own bookmark ids, not status ids"
    );

    let past = bookmarks(
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
async fn the_bookmarks_list_drops_the_invisible_and_the_deleted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token, _all, live) = seed_list(&pool, &app).await;

    // A bookmark the viewer can no longer see leaves the list: the
    // visibility step runs after the page walk, and the Link edges are the
    // entries that came back.
    let _hidden = sqlx::query("UPDATE statuses SET visibility = 'private' WHERE id = $1")
        .bind(live[1].1)
        .execute(&pool)
        .await
        .expect("the fixture hides the second post");
    let narrowed = bookmarks(&app, &token, "").await;
    common::assert_answer(&narrowed, StatusCode::OK, "the narrowed page answers");
    assert_eq!(
        status_ids(&narrowed.json_value()),
        vec![live[0].1.to_string(), live[2].1.to_string()],
        "the private one dropped out and the order stayed"
    );
    assert_bookmark_keyed_link(
        narrowed.header("link"),
        "/api/v1/bookmarks",
        live[0].0,
        live[2].0,
    );

    // A tombstone is off the list too, before the visibility step even
    // gets it: the model pairs bookmarks only with live rows.
    let _deleted = sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
        .bind(live[0].1)
        .execute(&pool)
        .await
        .expect("the fixture tombstones the newest post");
    let rest = bookmarks(&app, &token, "").await;
    assert_eq!(
        status_ids(&rest.json_value()),
        vec![live[2].1.to_string()],
        "only the still-visible bookmark is left"
    );
}

/// A folder named `name` owned by `owner`, seeded through the model — the
/// filing fixtures need the row, not the HTTP round trip the folder file
/// covers.
async fn folder_of(pool: &PgPool, owner: &User, name: &str) -> bookmark_folder::BookmarkFolder {
    bookmark_folder::create(pool, owner.id, name, None)
        .await
        .expect("the fixture folder creates")
}

/// The stored `folder_id` of `user_id`'s single bookmark row.
async fn filed_folder(pool: &PgPool, user_id: Uuid) -> Option<Uuid> {
    sqlx::query_as::<_, (Option<Uuid>,)>("SELECT folder_id FROM bookmarks WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("the one row reads")
        .0
}

/// `POST …/bookmark` for status `id`, filing it in `folder_id` (a JSON
/// body, decision 10's usual spelling).
async fn bookmark_into(
    app: &Router,
    token: &str,
    id: &str,
    folder_id: &str,
) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/bookmark"))
        .bearer(token)
        .json(&serde_json::json!({"folder_id": folder_id}))
        .send(app)
        .await
}

#[tokio::test]
async fn a_filed_bookmark_moves_frees_and_renders_its_folder() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = bookmarker(&pool, "alice").await;
    let status = post_by(&pool, "bob").await;
    let id = status.id.to_string();
    let first = folder_of(&pool, &alice, "first").await;
    let second = folder_of(&pool, &alice, "second").await;

    // The write files the row and the answered entity carries the folder —
    // the same read fills every other surface.
    let filed = common::TestRequest::post(&format!(
        "/api/v1/statuses/{id}/bookmark?folder_id={}",
        first.id
    ))
    .bearer(&token)
    .send(&app)
    .await;
    common::assert_answer(&filed, StatusCode::OK, "the query filing stores");
    assert_eq!(
        filed.json_value()["pleroma"]["bookmark_folder"],
        first.id.to_string(),
        "pleroma.bookmark_folder is the folder id"
    );
    assert_eq!(
        filed_folder(&pool, alice.id).await,
        Some(first.id),
        "the row filed"
    );

    let moved = bookmark_into(&app, &token, &id, &second.id.to_string()).await;
    common::assert_answer(&moved, StatusCode::OK, "the re-bookmark moves");
    assert_eq!(
        moved.json_value()["pleroma"]["bookmark_folder"],
        second.id.to_string(),
        "a re-bookmark with a different folder moves it"
    );
    assert_eq!(
        filed_folder(&pool, alice.id).await,
        Some(second.id),
        "same row, new folder"
    );

    // A re-bookmark that names no folder frees the row (Pleroma's
    // on_conflict restates the folder the request carries).
    let freed = bookmarked(&app, &token, &id).await;
    assert_eq!(
        freed["pleroma"]["bookmark_folder"],
        Value::Null,
        "and the freed bookmark renders no folder"
    );
    assert_eq!(filed_folder(&pool, alice.id).await, None, "back to unfiled");

    // The flag and the folder are one read: only the bookmarker sees the
    // folder, and an un-bookmarked row carries neither.
    let _refiled = bookmark_into(&app, &token, &id, &first.id.to_string()).await;
    let shown = show(&app, Some(&token), &id).await.json_value();
    assert_eq!(shown["bookmarked"], true, "the flag for its bookmarker");
    assert_eq!(
        shown["pleroma"]["bookmark_folder"],
        first.id.to_string(),
        "and the folder from the same batched read"
    );
    let anonymous = show(&app, None, &id).await.json_value();
    assert_eq!(
        anonymous["bookmarked"], false,
        "an anonymous viewer has no bookmark"
    );
    assert_eq!(
        anonymous["pleroma"]["bookmark_folder"],
        Value::Null,
        "and no folder"
    );
    let (_viewer, viewer_token) = fixture(&pool, "viewer", "read").await;
    let foreign = show(&app, Some(&viewer_token), &id).await.json_value();
    assert_eq!(
        foreign["pleroma"]["bookmark_folder"],
        Value::Null,
        "alice's folder is nobody else's key"
    );
}

#[tokio::test]
async fn a_folder_the_caller_cannot_name_is_the_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = bookmarker(&pool, "alice").await;
    let status = post_by(&pool, "bob").await;
    let id = status.id.to_string();
    let (mallory, _mallory_token) = fixture(&pool, "mallory", "write:bookmarks read").await;
    let his = folder_of(&pool, &mallory, "his").await;

    for folder_id in [
        his.id.to_string(),
        UNKNOWN_ID.to_owned(),
        "not-a-uuid".to_owned(),
    ] {
        let refused = bookmark_into(&app, &token, &id, &folder_id).await;
        common::assert_answer(
            &refused,
            StatusCode::NOT_FOUND,
            "an unknown, foreign or malformed folder id",
        );
        common::assert_error(&refused, NOT_FOUND, "the one 404 wording");
    }
    assert_eq!(
        row_count(&pool).await,
        0,
        "and nothing was bookmarked behind a refused folder"
    );

    // A withdrawal is folder-blind: it removes the row however filed.
    let _filed = bookmark_into(
        &app,
        &token,
        &id,
        &folder_of(&pool, &alice, "shelf").await.id.to_string(),
    )
    .await;
    let gone = unbookmark_route(&app, &token, &id).await;
    common::assert_answer(&gone, StatusCode::OK, "unbookmarking a filed bookmark runs");
    assert_eq!(row_count(&pool).await, 0, "and the filed row went");
}

#[tokio::test]
async fn the_bookmarks_list_filters_by_folder() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = bookmarker(&pool, "alice").await;
    let shelved = folder_of(&pool, &alice, "shelved").await;
    let later = folder_of(&pool, &alice, "later").await;
    let author = common::create_local_user(&pool, "author").await;
    let first = post(&pool, &author).await;
    let second = post(&pool, &author).await;
    let third = post(&pool, &author).await;
    let fourth = post(&pool, &author).await;
    let _filed_first =
        bookmark_into(&app, &token, &first.id.to_string(), &shelved.id.to_string()).await;
    let _filed_second = bookmark_into(
        &app,
        &token,
        &second.id.to_string(),
        &shelved.id.to_string(),
    )
    .await;
    let _filed_third =
        bookmark_into(&app, &token, &third.id.to_string(), &later.id.to_string()).await;
    bookmark_ok(&app, &token, &fourth.id.to_string()).await; // unfiled
    let all = bookmark_ids(&pool, alice.id).await;

    let page = bookmarks(&app, &token, &format!("folder_id={}", shelved.id)).await;
    common::assert_answer(&page, StatusCode::OK, "the folder page answers");
    let body = page.json_value();
    assert_eq!(
        status_ids(&body),
        vec![second.id.to_string(), first.id.to_string()],
        "the shelved bookmarks only, newest bookmark first"
    );
    assert!(
        body.as_array()
            .expect("an array")
            .iter()
            .all(|entry| entry["pleroma"]["bookmark_folder"] == shelved.id.to_string()),
        "every entry of the narrowed page renders the folder it was asked for"
    );
    assert_bookmark_keyed_link(page.header("link"), "/api/v1/bookmarks", all[2], all[3]);

    let other = bookmarks(&app, &token, &format!("folder_id={}", later.id)).await;
    assert_eq!(
        status_ids(&other.json_value()),
        vec![third.id.to_string()],
        "the other folder answers its own bookmark"
    );

    let everything = bookmarks(&app, &token, "").await;
    assert_eq!(
        status_ids(&everything.json_value()).len(),
        4,
        "no folder named is the whole list, filed or not"
    );
    let blank = bookmarks(&app, &token, "folder_id=").await;
    common::assert_answer(
        &blank,
        StatusCode::OK,
        "an empty folder_id asks for nothing",
    );
    assert_eq!(
        status_ids(&blank.json_value()).len(),
        4,
        "and the list is the whole list"
    );
}

/// Well-formed ids that hold none of the caller's bookmarks — unknown,
/// another account's — answer the empty page; a malformed `folder_id` is
/// the page cursor's own 400.
#[tokio::test]
async fn a_folder_holding_narrowed_nothing_or_a_malformed_id_narrows_the_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = bookmarker(&pool, "alice").await;
    let kept = folder_of(&pool, &alice, "kept").await;
    let author = common::create_local_user(&pool, "author").await;
    let status = post(&pool, &author).await;
    let _filed = bookmark_into(&app, &token, &status.id.to_string(), &kept.id.to_string()).await;

    let mallory = common::create_local_user(&pool, "mallory").await;
    let his_folder = folder_of(&pool, &mallory, "his").await;
    for query in [
        format!("folder_id={}", Uuid::now_v7()),
        format!("folder_id={}", his_folder.id),
        // The caller's OWN empty folder: well-formed, holds nothing.
        format!("folder_id={}", folder_of(&pool, &alice, "empty").await.id),
    ] {
        let empty = bookmarks(&app, &token, &query).await;
        common::assert_answer(&empty, StatusCode::OK, "a folder holding nothing");
        assert_eq!(
            empty.json_value().as_array().map(Vec::len),
            Some(0),
            "answers the empty page: `{query}`"
        );
        assert_eq!(empty.header("link"), None, "which links nowhere");
    }
    let malformed = bookmarks(&app, &token, "folder_id=not-a-uuid").await;
    common::assert_answer(&malformed, StatusCode::BAD_REQUEST, "a malformed folder id");
    common::assert_error(&malformed, BAD_ID, "is the cursor's own 400");
}
