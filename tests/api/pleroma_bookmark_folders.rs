//! Bookmark folders over HTTP (WBE-88): the four routes under
//! `/api/v1/pleroma/bookmark_folders` — list, create, update (`PATCH`) and
//! delete — plus the entity's `emoji`/`emoji_url` pair.
//!
//! Folders are strictly per-account: the caller's list holds only their own,
//! and a foreign, unknown or malformed `{id}` is one indistinguishable 404
//! on both mutating routes. `name` is required and 1–50 characters, and a
//! name the owner already took is the 422 `{"error":"name: has already been
//! taken"}` — another account may reuse it freely. `DELETE` answers the
//! deleted entity and keeps the bookmarks filed in the folder, freed back
//! to no folder. Every route gates on the bookmark scopes:
//! `read:bookmarks` to look, `write:bookmarks` to change.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        bookmark, bookmark_folder,
        custom_emoji::{self, NewCustomEmoji},
        status::{NewStatus, insert as insert_status},
        user::{NewUser, User, insert as insert_user, local_ap_id},
    },
};

use crate::common;

/// The folder routes' base path.
const ROUTE: &str = "/api/v1/pleroma/bookmark_folders";
/// The 404 body of the one indistinguishable refusal.
const NOT_FOUND: &str = "Record not found";
/// The 401 body `AuthUser` sends when no usable token was presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for an app-only token on a route that needs a user.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 wording of the write gate.
const NO_WRITE: &str = "Insufficient permissions: write:bookmarks.";
/// The 403 wording of the read gate.
const NO_READ: &str = "Insufficient permissions: read:bookmarks.";
/// The 401 body for a token that does not resolve.
const BAD_TOKEN: &str = "The access token is invalid";
/// The 422 body for a name the owner already uses (the ticket's wording).
const NAME_TAKEN: &str = "name: has already been taken";
/// The 422 body for an absent or blank name.
const NAME_BLANK: &str = "name: can't be blank";
/// The 422 body for a name over 50 characters.
const NAME_TOO_LONG: &str = "name: should be at most 50 characters";
/// The 422 body for an emoji that names no usable emoji.
const EMOJI_INVALID: &str = "emoji: Invalid emoji";
/// An id that names no stored folder.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

/// A local user named `nickname` with a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` who may read and change folders.
async fn folder_owner(pool: &PgPool, nickname: &str) -> (User, String) {
    fixture(pool, nickname, "read:bookmarks write:bookmarks").await
}

/// `GET /api/v1/pleroma/bookmark_folders` as `token`.
async fn list(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(ROUTE)
        .bearer(token)
        .send(app)
        .await
}

/// `POST /api/v1/pleroma/bookmark_folders` with a JSON `body`.
async fn create(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(ROUTE)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// A create a test only needs to have happened, answered as JSON.
async fn created(app: &Router, token: &str, body: Value) -> Value {
    let response = create(app, token, body).await;
    common::assert_answer(&response, StatusCode::OK, "the folder creates");
    response.json_value()
}

/// `PATCH /api/v1/pleroma/bookmark_folders/{id}` with a JSON `body`.
async fn update(app: &Router, token: &str, id: &str, body: Value) -> common::TestResponse {
    common::TestRequest::patch(&format!("{ROUTE}/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `DELETE /api/v1/pleroma/bookmark_folders/{id}` as `token`.
async fn remove(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::delete(&format!("{ROUTE}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// The names of the folder array `body`, in order.
fn names(body: &Value) -> Vec<&str> {
    body.as_array()
        .expect("the answer is an array")
        .iter()
        .map(|entry| entry["name"].as_str().unwrap_or_default())
        .collect()
}

/// The keys of a rendered entity, sorted — the exact shape the ticket pins.
fn entity_keys(folder: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = folder
        .as_object()
        .expect("an entity is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

#[tokio::test]
async fn a_folder_round_trips_create_list_update_and_delete() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = folder_owner(&pool, "alice").await;

    let folder = created(&app, &token, json!({"name": "Read later", "emoji": "📚"})).await;
    assert_eq!(
        entity_keys(&folder),
        vec!["emoji", "emoji_url", "id", "name"],
        "exactly the ticket's four keys"
    );
    assert!(
        Uuid::parse_str(folder["id"].as_str().expect("the id is text")).is_ok(),
        "the id is the UUIDv7 as text (decision 4): {folder}"
    );
    assert_eq!(folder["name"], "Read later", "the name verbatim");
    assert_eq!(folder["emoji"], "📚", "a unicode emoji is kept as sent");
    assert_eq!(folder["emoji_url"], Value::Null, "and unicode has no image");

    let page = list(&app, &token).await;
    common::assert_answer(&page, StatusCode::OK, "the list answers");
    assert_eq!(names(&page.json_value()), ["Read later"], "creation order");

    let id = folder["id"].as_str().expect("the id is text").to_owned();
    let renamed = update(&app, &token, &id, json!({"name": "Later"})).await;
    common::assert_answer(&renamed, StatusCode::OK, "the folder updates");
    let renamed = renamed.json_value();
    assert_eq!(renamed["id"], folder["id"], "the same folder, rewritten");
    assert_eq!(renamed["name"], "Later", "the named value changed");
    assert_eq!(
        renamed["emoji"], "📚",
        "an absent parameter keeps the stored one"
    );

    let gone = remove(&app, &token, &id).await;
    common::assert_answer(&gone, StatusCode::OK, "the folder deletes");
    assert_eq!(
        gone.json_value(),
        renamed,
        "the answer is the deleted entity"
    );

    let empty = list(&app, &token).await;
    assert_eq!(
        empty.json_value().as_array().map(Vec::len),
        Some(0),
        "and the folder is gone from the list"
    );
    assert_eq!(
        remove(&app, &token, &id).await.status,
        StatusCode::NOT_FOUND,
        "deleting it again is the 404"
    );
}

#[tokio::test]
async fn a_shortcode_emoji_renders_its_image_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = folder_owner(&pool, "alice").await;
    let image = format!("{TEST_BASE_URL}/emoji/custom/folder_cat.png");
    let _stored = custom_emoji::upsert(&pool, &NewCustomEmoji::local("folder_cat", &image))
        .await
        .expect("the fixture custom emoji upserts");

    let folder = created(
        &app,
        &token,
        json!({"name": "Cats", "emoji": ":folder_cat:"}),
    )
    .await;
    assert_eq!(folder["emoji"], ":folder_cat:", "the shortcode as stored");
    assert_eq!(folder["emoji_url"], image, "and its image URL");

    // The list resolves the same URL from one batched read.
    let page = list(&app, &token).await.json_value();
    assert_eq!(page[0]["emoji_url"], image, "the list carries the URL too");

    // Once the custom emoji is gone the shortcode matches nothing: the
    // folder keeps its text and loses its URL.
    let deleted = custom_emoji::delete(&pool, "folder_cat", None)
        .await
        .expect("the fixture emoji deletes");
    assert!(deleted, "the local row was there to delete");
    let shown = list(&app, &token).await.json_value();
    assert_eq!(
        shown[0]["emoji"], ":folder_cat:",
        "the text is not rewritten"
    );
    assert_eq!(
        shown[0]["emoji_url"],
        Value::Null,
        "the no-match URL is null"
    );
}

#[tokio::test]
async fn writes_refuse_names_and_emojis_they_cannot_take() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = folder_owner(&pool, "alice").await;

    // Name: absent, blank, too long; 50 characters fit.
    common::assert_error(&create(&app, &token, json!({})).await, NAME_BLANK, "absent");
    common::assert_error(
        &create(&app, &token, json!({"name": "   "})).await,
        NAME_BLANK,
        "blank",
    );
    let long = "x".repeat(51);
    common::assert_answer(
        &create(&app, &token, json!({"name": long})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "51 characters are refused",
    );
    common::assert_error(
        &create(&app, &token, json!({"name": long})).await,
        NAME_TOO_LONG,
        "with the limit wording",
    );
    let edge = "🦊".repeat(50);
    let fits = created(&app, &token, json!({"name": edge.clone()})).await;
    assert_eq!(fits["name"], edge, "50 characters fit, verbatim");

    // Emoji: neither one unicode emoji nor a local `:shortcode:`.
    for value in ["cat", "not an emoji", "::"] {
        let refused = create(
            &app,
            &token,
            json!({"name": "nobody's business", "emoji": value}),
        )
        .await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a non-emoji is refused",
        );
        common::assert_error(&refused, EMOJI_INVALID, "with Pleroma's wording");
    }
    // A well-spelled shortcode of an emoji this instance does not have.
    common::assert_error(
        &create(&app, &token, json!({"name": "x", "emoji": ":nope:"})).await,
        EMOJI_INVALID,
        "a missing custom emoji",
    );

    // The same refusals on the `PATCH` arm, ahead of the lookup: even the
    // known id is not reached.
    let folder = created(&app, &token, json!({"name": "gate"})).await;
    let id = folder["id"].as_str().expect("the id is text").to_owned();
    for body in [json!({"name": ""}), json!({"emoji": "cat"})] {
        let refused = update(&app, &token, &id, body.clone()).await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            "the patch refuses an invalid parameter",
        );
    }
    let kept = list(&app, &token).await.json_value();
    assert_eq!(
        kept.as_array()
            .expect("the list is an array")
            .iter()
            .find(|entry| entry["id"] == json!(&id))
            .expect("the folder is still listed")["name"],
        "gate",
        "and the refused updates rewrote nothing"
    );
}

#[tokio::test]
async fn a_taken_name_is_refused_for_its_owner_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = folder_owner(&pool, "alice").await;
    let (_bob, bob_token) = folder_owner(&pool, "bob").await;

    let _dup = created(&app, &alice_token, json!({"name": "dup"})).await;
    let twin = create(&app, &alice_token, json!({"name": "dup"})).await;
    common::assert_answer(
        &twin,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the second `dup` is refused",
    );
    common::assert_error(&twin, NAME_TAKEN, "with the ticket's wording");

    // Another owner may take the same name, and so may alice on a rename.
    let _theirs = created(&app, &bob_token, json!({"name": "dup"})).await;
    let mine = created(&app, &alice_token, json!({"name": "mine"})).await;
    let id = mine["id"].as_str().expect("the id is text").to_owned();
    common::assert_error(
        &update(&app, &alice_token, &id, json!({"name": "dup"})).await,
        NAME_TAKEN,
        "the rename onto the taken name",
    );
    assert_eq!(
        bookmark_folder::list(&pool, alice.id)
            .await
            .expect("alice's folders read")
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<&str>>(),
        vec!["dup", "mine"],
        "and nothing changed"
    );
}

#[tokio::test]
async fn folders_are_per_account_and_a_foreign_id_is_the_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = folder_owner(&pool, "alice").await;
    let (_bob, bob_token) = folder_owner(&pool, "bob").await;

    let hers = created(&app, &alice_token, json!({"name": "hers"})).await;
    let id = hers["id"].as_str().expect("the id is text").to_owned();

    assert_eq!(
        names(&list(&app, &bob_token).await.json_value()),
        Vec::<&str>::new(),
        "bob's list is empty"
    );
    assert_eq!(alice.id.as_bytes().len(), 16, "the fixture user is real");

    for response in [
        update(&app, &bob_token, &id, json!({"name": "theft"})).await,
        remove(&app, &bob_token, &id).await,
    ] {
        common::assert_answer(&response, StatusCode::NOT_FOUND, "a foreign folder");
        common::assert_error(&response, NOT_FOUND, "the one 404 wording");
    }
    for bogus in [UNKNOWN_ID, "not-an-uuid"] {
        for response in [
            update(&app, &alice_token, bogus, json!({"name": "x"})).await,
            remove(&app, &alice_token, bogus).await,
        ] {
            common::assert_answer(
                &response,
                StatusCode::NOT_FOUND,
                "an unknown or malformed id",
            );
            common::assert_error(&response, NOT_FOUND, "the same 404 wording");
        }
    }
    assert_eq!(
        names(&list(&app, &alice_token).await.json_value()),
        ["hers"],
        "and alice's folder survived every attempt on it"
    );
}

#[tokio::test]
async fn a_form_body_creates_a_folder_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = folder_owner(&pool, "alice").await;

    let response = common::TestRequest::post(ROUTE)
        .bearer(&token)
        .form(&[("name", "from the form"), ("emoji", "📚")])
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "decision 10's form spelling");
    let body = response.json_value();
    assert_eq!(body["name"], "from the form", "the params arrive");
    assert_eq!(body["emoji"], "📚", "and so does the emoji");

    // An absent body is a parameter-less body: the name is what's missing.
    let bare = common::TestRequest::post(ROUTE)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&bare, StatusCode::UNPROCESSABLE_ENTITY, "a bodyless create");
    common::assert_error(&bare, NAME_BLANK, "fails on the required name");
}

#[tokio::test]
async fn deleting_a_folder_keeps_the_bookmarks_filed_in_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = folder_owner(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let status = insert_status(
        &pool,
        TEST_BASE_URL,
        &NewStatus::local(&bob, "a post to keep"),
    )
    .await
    .expect("the fixture status inserts");

    let folder = created(&app, &token, json!({"name": "doomed"})).await;
    let folder_id = Uuid::parse_str(folder["id"].as_str().expect("the id is text"))
        .expect("the folder id parses");
    let filed = bookmark::add(&pool, alice.id, status.id, Some(folder_id))
        .await
        .expect("the fixture bookmark files");
    assert_eq!(filed.folder_id, Some(folder_id), "filed before freed");

    let id = folder["id"].as_str().expect("the id is text").to_owned();
    let gone = remove(&app, &token, &id).await;
    common::assert_answer(&gone, StatusCode::OK, "the folder deletes");
    assert_eq!(
        gone.json_value(),
        folder,
        "the answer is the deleted entity"
    );

    // The bookmark stands, unfiled — and still on the caller's list.
    let row: (Option<Uuid>,) = sqlx::query_as("SELECT folder_id FROM bookmarks WHERE id = $1")
        .bind(filed.id)
        .fetch_one(&pool)
        .await
        .expect("the bookmark row reads");
    assert_eq!(row.0, None, "the foreign key set it NULL");
    let page = common::TestRequest::get("/api/v1/bookmarks")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&page, StatusCode::OK, "the list answers");
    let body = page.json_value();
    assert_eq!(body.as_array().map(Vec::len), Some(1), "the kept bookmark");
    assert_eq!(body[0]["id"], status.id.to_string(), "names its status");
    assert_eq!(
        body[0]["pleroma"]["bookmark_folder"],
        Value::Null,
        "and renders no folder any more"
    );
}

#[tokio::test]
async fn the_folder_routes_gate_on_the_bookmark_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_reader, read_token) = fixture(&pool, "reader", "read:bookmarks").await;
    let (_writer, write_token) = fixture(&pool, "writer", "write:bookmarks").await;
    let (_owner, owner_token) = folder_owner(&pool, "owner").await;
    let folder = created(&app, &owner_token, json!({"name": "gated"})).await;
    let id = folder["id"].as_str().expect("the id is text").to_owned();

    // Each unauthenticated spelling is refused on every route: no token and
    // an unresolvable one are the 401 pair, an app-only token the 403.
    let no_user = common::create_app_only_token(&pool, "read write").await;
    let refusals = [
        ("", StatusCode::UNAUTHORIZED, NO_TOKEN),
        ("not-a-token", StatusCode::UNAUTHORIZED, BAD_TOKEN),
        (no_user.as_str(), StatusCode::FORBIDDEN, USER_REQUIRED),
    ];
    for (token, status, gate) in refusals {
        for response in [
            list(&app, token).await,
            create(&app, token, json!({"name": "x"})).await,
            update(&app, token, &id, json!({"name": "x"})).await,
            remove(&app, token, &id).await,
        ] {
            common::assert_answer(&response, status, gate);
            common::assert_error(&response, gate, "at the gate");
        }
    }

    // Each half of the scope pair opens only its own side.
    let write_on_read = create(&app, &read_token, json!({"name": "nope"})).await;
    common::assert_answer(&write_on_read, StatusCode::FORBIDDEN, "a read-only writer");
    common::assert_error(&write_on_read, NO_WRITE, "the write gate");
    common::assert_error(
        &update(&app, &read_token, &id, json!({"name": "nope"})).await,
        NO_WRITE,
        "the patch gate",
    );
    common::assert_error(
        &remove(&app, &read_token, &id).await,
        NO_WRITE,
        "the delete gate",
    );
    common::assert_error(&list(&app, &write_token).await, NO_READ, "the read gate");
}

#[tokio::test]
async fn a_deleted_owner_takes_their_folders_through_the_cascade() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = insert_user(
        &pool,
        &NewUser::local("carol", &local_ap_id(TEST_BASE_URL, "carol")),
    )
    .await
    .expect("the fixture user inserts");
    let folder = bookmark_folder::create(&pool, carol.id, "hers", None)
        .await
        .expect("the fixture folder creates");

    let erased = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(carol.id)
        .execute(&pool)
        .await
        .expect("the fixture user deletes")
        .rows_affected();
    assert_eq!(erased, 1, "the owner went");
    assert!(
        bookmark_folder::get_owned(&pool, folder.user_id, folder.id)
            .await
            .expect("the read runs")
            .is_none(),
        "and the cascade took the folder"
    );
}
