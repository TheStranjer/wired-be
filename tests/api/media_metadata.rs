//! Tests for the media metadata endpoints: `GET` and `PUT`
//! `/api/v1/media/{id}` and `/api/v2/media/{id}` (`src/api/media.rs`). They
//! cover reading an own unattached upload, the shared 404 "Record not
//! found" for foreign, unknown and already-attached media, present-only
//! updates of `description` and `focus`, the ticket's 422 wordings
//! ("Description too long", "Invalid focus"), a `thumbnail` part replacing
//! the preview image, the `read:statuses` / `write:media` scope gates and
//! the absence of a `DELETE` route. Upload itself is `tests/media_upload.rs`'s
//! subject; the `GET /media/{key}` file route is `tests/media.rs`'s.
//!
//! As in `tests/media_upload.rs`, uploads land in a [`TempDir`] through the
//! `common::test_app_with_*` helpers, and the media fixtures are the
//! shortest byte sequences whose magic bytes the detector names.

#![cfg(test)]

use axum::{Router, body::Bytes, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;
use wired_be::models::{
    media_attachment::{self, MediaAttachment as Row, NewMediaAttachment},
    status::Visibility,
    user::User,
};

use crate::common::{self, MultipartPart, images};

/// Boundary the multipart bodies are framed with.
const BOUNDARY: &str = "wired-media-meta-test-boundary";
/// The instance origin the fixtures serve from.
const MEDIA_BASE_URL: &str = "https://test.example/media";
/// The scope `GET /media/{id}` demands, per the ticket.
const READ_SCOPE: &str = "read:statuses";
/// The scope `PUT /media/{id}` demands.
const WRITE_SCOPE: &str = "write:media";
/// The scope both endpoints answer a missing credential with.
const BOTH_SCOPES: &str = "read:statuses write:media";

/// A `PNG`: the signature `infer` matches on, plus the start of an `IHDR`.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
/// Text: bytes no matcher can name at all.
const TEXT: &[u8] = b"a plain text file, neither image, video nor audio\n";

/// A throwaway uploads directory, removed when the returned guard drops.
fn uploads_dir() -> TempDir {
    TempDir::new().expect("temp dir for uploads")
}

/// The exact body of a JSON error response.
fn error_body(message: &str) -> String {
    format!(r#"{{"error":"{message}"}}"#)
}

/// A local user named `nickname` with a token granting `scopes`.
///
/// # Panics
///
/// If a fixture fails to insert.
async fn uploader(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// An unattached `PNG` upload of `owner` with a caption and a focal point,
/// stored straight through the model: these tests are about the routes on
/// an existing row, not about posting the row.
///
/// # Panics
///
/// If the fixture insert fails.
async fn insert_media(pool: &PgPool, owner: &User) -> Row {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(owner.id),
            url: format!("{MEDIA_BASE_URL}/{}/orig.png", owner.id),
            storage_key: Some(format!("{}/orig.png", owner.id)),
            content_type: "image/png".to_owned(),
            description: Some("a stored caption".to_owned()),
            focus_x: Some(0.25),
            focus_y: Some(-0.5),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("media fixture inserts")
}

/// [`insert_media`]'s row after a status claimed it: attached media, which
/// these routes no longer serve.
///
/// # Panics
///
/// If a fixture fails to insert.
async fn insert_attached_media(pool: &PgPool, owner: &User) -> Row {
    let posted = common::create_status(pool, owner, "a posted photo", Visibility::Public).await;
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            status_id: Some(posted.id),
            ..upload_of(owner)
        },
    )
    .await
    .expect("attached media fixture inserts")
}

/// The unattached-upload fixture of `owner`, before insertion.
fn upload_of(owner: &User) -> NewMediaAttachment {
    NewMediaAttachment {
        user_id: Some(owner.id),
        url: format!("{MEDIA_BASE_URL}/{}/orig.png", owner.id),
        storage_key: Some(format!("{}/orig.png", owner.id)),
        content_type: "image/png".to_owned(),
        description: Some("a stored caption".to_owned()),
        focus_x: Some(0.25),
        focus_y: Some(-0.5),
        ..NewMediaAttachment::default()
    }
}

/// The row behind `id`, panicking when it is gone.
///
/// # Panics
///
/// If the read fails or the row does not exist.
async fn fetch(pool: &PgPool, id: Uuid) -> Row {
    media_attachment::get_by_id(pool, id)
        .await
        .expect("the row reads")
        .expect("the fixture row exists")
}

/// The `file`/`thumbnail` part named `field`, `bytes` uploaded as `filename`.
fn file_part(field: &'static str, filename: &'static str, bytes: &[u8]) -> MultipartPart {
    MultipartPart::file(field, filename, "image/jpeg", Bytes::copy_from_slice(bytes))
}

/// `GET`s `path` with `token`.
async fn get(app: &Router, path: &str, token: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// `PUT`s `parts` as `multipart/form-data` to `path` with `token`.
async fn put_multipart(
    app: &Router,
    path: &str,
    token: &str,
    parts: &[MultipartPart],
) -> common::TestResponse {
    common::TestRequest::put(path)
        .bearer(token)
        .multipart(BOUNDARY, parts)
        .send(app)
        .await
}

/// `PUT`s `fields` as `application/x-www-form-urlencoded` to `path` with
/// `token`.
async fn put_form(
    app: &Router,
    path: &str,
    token: &str,
    fields: &[(&str, &str)],
) -> common::TestResponse {
    common::TestRequest::put(path)
        .bearer(token)
        .form(fields)
        .send(app)
        .await
}

/// `PUT`s `body` as `application/json` to `path` with `token`.
async fn put_json(app: &Router, path: &str, token: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::put(path)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// The 401 a route answers when no credential arrived: Pleroma's wording and
/// the bearer challenge, for `route`.
///
/// # Panics
///
/// If the response is not that 401 pair.
fn assert_unauthorized(response: &common::TestResponse, route: &str) {
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "{route} needs a token"
    );
    assert_eq!(
        response.text(),
        error_body("Invalid credentials"),
        "the anonymous {route} 401 uses Pleroma's wording"
    );
    assert_eq!(
        response.header("www-authenticate"),
        Some("Bearer"),
        "and carries the bearer challenge"
    );
}

/// The `/api/v1/media/{id}` path of a row.
fn v1_path(id: Uuid) -> String {
    format!("/api/v1/media/{id}")
}

/// `own_unattached_media_reads_back_on_both_versions`: a fresh unattached
/// upload answers the full `Attachment` entity on either version's path,
/// the broader `read` scope included; Mastodon's 206-while-unprocessed arm
/// cannot happen here because uploads are processed synchronously.
#[tokio::test]
async fn own_unattached_media_reads_back_on_both_versions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, reader) = uploader(&pool, "alice", READ_SCOPE).await;
    let read_all = common::create_app_and_token(&pool, &alice, "read").await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let row = insert_media(&pool, &alice).await;

    let entity = get(&app, &v1_path(row.id), &reader).await.json_value();
    assert_eq!(entity["id"], row.id.to_string(), "the entity is the row");
    assert_eq!(entity["type"], "image", "a png is an image");
    assert_eq!(entity["url"], row.url, "the stored url");
    assert_eq!(
        entity["preview_url"], row.url,
        "with no thumbnail the preview falls back to the file"
    );
    assert_eq!(
        entity["remote_url"],
        Value::Null,
        "a local upload has no origin"
    );
    assert_eq!(
        entity["description"], "a stored caption",
        "the stored alt text comes back"
    );
    assert_eq!(
        entity["meta"],
        json!({ "focus": { "x": 0.25, "y": -0.5 } }),
        "and the focal point is meta's one filled part"
    );

    let on_v2 = get(&app, &format!("/api/v2/media/{id}", id = row.id), &read_all)
        .await
        .json_value();
    assert_eq!(on_v2["id"], entity["id"], "v2 serves the same row");
    assert_eq!(on_v2["meta"], entity["meta"], "with the same entity");
}

/// `GET` answers one 404 "Record not found" for a foreign row, a claimed
/// one, an ownerless one, an unknown id and an id that is not a `UUID`;
/// none of them is distinguishable from the others.
#[tokio::test]
async fn foreign_attached_unknown_and_ownerless_are_one_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, reader) = uploader(&pool, "alice", READ_SCOPE).await;
    let (bob, _) = uploader(&pool, "bob", READ_SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let mine_attached = insert_attached_media(&pool, &alice).await;
    let foreign = insert_media(&pool, &bob).await;
    let ownerless = media_attachment::insert(
        &pool,
        &NewMediaAttachment {
            user_id: None,
            ..upload_of(&alice)
        },
    )
    .await
    .expect("ownerless media fixture inserts");

    for nope in [foreign.id, mine_attached.id, ownerless.id, Uuid::now_v7()] {
        let response = get(&app, &v1_path(nope), &reader).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{nope}` is not the caller's unattached media"
        );
        assert_eq!(
            response.text(),
            error_body("Record not found"),
            "the 404 is Pleroma's wording"
        );
    }
    let malformed = get(&app, "/api/v1/media/not-a-uuid", &reader).await;
    assert_eq!(
        malformed.status,
        StatusCode::NOT_FOUND,
        "an id that is not a UUID is the same 404"
    );
    assert_eq!(malformed.text(), error_body("Record not found"), "and body");
}

/// `PUT` writes the fields the body carries and leaves the others alone:
/// a caption update keeps the stored focal point and vice versa, present
/// values may arrive as multipart, form or JSON, and a body with no fields
/// at all answers the unchanged entity.
#[tokio::test]
async fn an_update_replaces_present_fields_and_keeps_absent_ones() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, writer) = uploader(&pool, "alice", WRITE_SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let row = insert_media(&pool, &alice).await;
    let path = v1_path(row.id);

    let parts = [
        MultipartPart::text("description", "a new caption"),
        file_part("file", "stray.png", PNG),
    ];
    let captioned = put_multipart(&app, &path, &writer, &parts)
        .await
        .json_value();
    assert_eq!(
        captioned["description"], "a new caption",
        "the present caption is written"
    );
    assert_eq!(
        captioned["meta"]["focus"],
        json!({ "x": 0.25, "y": -0.5 }),
        "the absent focus keeps the stored axes"
    );
    let stored = fetch(&pool, row.id).await;
    assert_eq!(
        stored.description.as_deref(),
        Some("a new caption"),
        "and the row agrees"
    );

    let form_response = put_form(&app, &path, &writer, &[("focus", "-1,1")]).await;
    assert_eq!(
        form_response.status,
        StatusCode::OK,
        "a urlencoded body updates too: {}",
        form_response.text()
    );
    let refocused = form_response.json_value();
    assert_eq!(
        refocused["meta"]["focus"],
        json!({ "x": -1.0, "y": 1.0 }),
        "both ends of the range are accepted"
    );
    assert_eq!(
        refocused["description"], "a new caption",
        "and the caption is still there"
    );

    let json_response = put_json(&app, &path, &writer, &json!({ "description": "from json" }))
        .await
        .json_value();
    assert_eq!(
        json_response["description"], "from json",
        "so does a JSON body"
    );
    assert_eq!(
        json_response["meta"]["focus"],
        json!({ "x": -1.0, "y": 1.0 }),
        "with the same present-only rules"
    );

    let untouched = put_multipart(&app, &path, &writer, &[]).await;
    assert_eq!(
        untouched.status,
        StatusCode::OK,
        "an empty body is a no-op, not an error"
    );
    assert_eq!(
        untouched.json_value()["description"],
        "from json",
        "and writes nothing"
    );
    assert_eq!(
        fetch(&pool, row.id).await.content_type,
        "image/png",
        "even a `file` part is not re-uploaded through the update"
    );
}

/// A blank `description` clears the alt text and a blank `focus` both axes:
/// how Mastodon's clients remove either.
#[tokio::test]
async fn empty_fields_clear_the_stored_values() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, writer) = uploader(&pool, "alice", BOTH_SCOPES).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let row = insert_media(&pool, &alice).await;

    let cleared = put_multipart(
        &app,
        &v1_path(row.id),
        &writer,
        &[
            MultipartPart::text("description", ""),
            MultipartPart::text("focus", ""),
        ],
    )
    .await
    .json_value();
    assert_eq!(
        cleared["description"],
        Value::Null,
        "an empty caption renders as null"
    );
    assert_eq!(
        cleared["meta"],
        json!({}),
        "a cleared focus leaves meta empty"
    );
    let stored = fetch(&pool, row.id).await;
    assert_eq!(stored.focus_x, None, "the x axis is gone from the row");
    assert_eq!(stored.focus_y, None, "and the y axis");
}

/// A `focus` that is not two floats in `[-1.0, 1.0]` is the ticket's 422
/// "Invalid focus", and the stored focal point stays untouched.
#[tokio::test]
async fn an_invalid_focus_is_the_ticket_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, writer) = uploader(&pool, "alice", WRITE_SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let row = insert_media(&pool, &alice).await;
    let path = v1_path(row.id);

    for focus in [
        "0.5",
        "x,y",
        "1.5,0",
        "0,-1.5",
        "0.5,0.5,0.5",
        "abc,def",
        "nan,nan",
        "inf,0",
        ",0",
        "0,",
    ] {
        let response =
            put_multipart(&app, &path, &writer, &[MultipartPart::text("focus", focus)]).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{focus}` is not a focal point"
        );
        assert_eq!(
            response.text(),
            error_body("Invalid focus"),
            "the 422 is the ticket's exact message for `{focus}`"
        );
    }
    let stored = fetch(&pool, row.id).await;
    assert_eq!(
        stored.focus_x,
        Some(0.25),
        "a rejected write changed nothing"
    );
    assert_eq!(stored.focus_y, Some(-0.5), "on either axis");
}

/// A `description` over the 1500-character default is the ticket's 422
/// "Description too long"; the limit is inclusive and counts characters,
/// not bytes.
#[tokio::test]
async fn a_description_over_the_default_limit_is_the_ticket_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, writer) = uploader(&pool, "alice", WRITE_SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let row = insert_media(&pool, &alice).await;
    let path = v1_path(row.id);

    for (length, multibyte) in [(1_500, false), (1_500, true)] {
        let text = fill(length, multibyte);
        let response = common::TestRequest::put(&path)
            .bearer(&writer)
            .form(&[("description", text.as_str())])
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "exactly {length} characters (multibyte: {multibyte}) fit"
        );
    }
    for (length, multibyte) in [(1_501, false), (1_501, true)] {
        let text = fill(length, multibyte);
        let response = common::TestRequest::put(&path)
            .bearer(&writer)
            .form(&[("description", text.as_str())])
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{length} characters (multibyte: {multibyte}) do not"
        );
        assert_eq!(
            response.text(),
            error_body("Description too long"),
            "the 422 is the ticket's exact message"
        );
    }
    assert_eq!(
        fetch(&pool, row.id).await.description.as_deref(),
        Some(fill(1_500, true).as_str()),
        "a rejected write changed nothing after the accepted ones"
    );
}

/// `fill`: `length` characters, plain `x` or the multibyte `é`.
fn fill(length: usize, multibyte: bool) -> String {
    if multibyte {
        "é".repeat(length)
    } else {
        "x".repeat(length)
    }
}

/// The limit is configuration: with `MAX_MEDIA_DESCRIPTION=10` ten
/// characters pass and eleven fail, and the last accepted one is stored.
#[tokio::test]
async fn the_configured_limit_caps_the_description() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, writer) = uploader(&pool, "alice", WRITE_SCOPE).await;
    let mut config = common::test_config_with_uploads_dir(dir.path());
    config.max_media_description = 10;
    let app = common::test_app_with_config(pool.clone(), config);
    let row = insert_media(&pool, &alice).await;
    let path = v1_path(row.id);

    let long = fill(11, false);
    let refused = common::TestRequest::put(&path)
        .bearer(&writer)
        .form(&[("description", long.as_str())])
        .send(&app)
        .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "eleven characters pass a limit of ten"
    );
    assert_eq!(
        refused.text(),
        error_body("Description too long"),
        "with the ticket's message"
    );

    let ten = fill(10, true);
    let accepted = common::TestRequest::put(&path)
        .bearer(&writer)
        .form(&[("description", ten.as_str())])
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        accepted["description"],
        Value::String(ten.clone()),
        "ten multibyte characters are exactly the limit"
    );
    assert_eq!(
        fetch(&pool, row.id).await.description.as_deref(),
        Some(ten.as_str()),
        "and they are stored"
    );
}

/// A `thumbnail` part is stored as the attachment's preview: its own key,
/// served at the announced URL, while the file itself and every other
/// column stay as they were.
#[tokio::test]
async fn a_thumbnail_part_replaces_the_preview() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, writer) = uploader(&pool, "alice", BOTH_SCOPES).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let row = insert_media(&pool, &alice).await;

    let entity = put_multipart(
        &app,
        &v1_path(row.id),
        &writer,
        &[
            file_part("thumbnail", "thumb.jpg", &images::jpeg()),
            MultipartPart::text("description", "with a preview"),
        ],
    )
    .await
    .json_value();
    assert_eq!(
        entity["description"], "with a preview",
        "the fields sent with the thumbnail are written"
    );
    assert_eq!(entity["url"], row.url, "the file itself is untouched");
    assert_eq!(
        entity["meta"],
        json!({ "focus": { "x": 0.25, "y": -0.5 } }),
        "and so is the focal point"
    );
    let preview = entity["preview_url"]
        .as_str()
        .expect("the entity announces a preview")
        .to_owned();
    assert_ne!(preview, row.url, "the preview is its own file");
    assert!(
        preview.starts_with(&format!("{MEDIA_BASE_URL}/{}/", alice.id)),
        "in the uploader's namespace: {preview}"
    );
    assert_eq!(
        preview
            .rsplit_once('.')
            .expect("the preview key names its extension")
            .1,
        "jpg",
        "keyed by the type the bytes were detected as: {preview}"
    );

    let stored = fetch(&pool, row.id).await;
    assert_eq!(
        stored.preview_url.as_deref(),
        Some(preview.as_str()),
        "the row records the announced url"
    );
    assert_eq!(stored.url, row.url, "and keeps the original file");

    let key = preview
        .strip_prefix(&format!("{MEDIA_BASE_URL}/"))
        .expect("the preview is served from the media base");
    let served = common::TestRequest::get(&format!("/media/{key}"))
        .send(&app)
        .await;
    assert_eq!(served.status, StatusCode::OK, "and the file serves");
    assert_eq!(
        served.body,
        Bytes::from(images::normalized(&images::jpeg())),
        "as the upload, decoded and encoded again"
    );
    assert_eq!(
        served.header("content-type"),
        Some("image/jpeg"),
        "under the type they were detected as"
    );
}

/// A second `thumbnail` replaces the first one's preview instead of piling
/// up: the row keeps only the newest URL.
#[tokio::test]
async fn a_second_thumbnail_replaces_the_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, writer) = uploader(&pool, "alice", WRITE_SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let row = insert_media(&pool, &alice).await;
    let path = v1_path(row.id);

    let first = put_multipart(
        &app,
        &path,
        &writer,
        &[file_part("thumbnail", "one.png", &images::png())],
    )
    .await
    .json_value();
    let second = put_multipart(
        &app,
        &path,
        &writer,
        &[file_part("thumbnail", "two.jpg", &images::jpeg())],
    )
    .await
    .json_value();
    assert_ne!(
        first["preview_url"], second["preview_url"],
        "each upload minted its own key"
    );
    assert_eq!(
        fetch(&pool, row.id).await.preview_url.as_deref(),
        second["preview_url"].as_str(),
        "and the row keeps only the newest"
    );
}

/// A `thumbnail` whose bytes name no media type is the upload endpoint's
/// 422, and nothing is stored — on disk or in the row.
#[tokio::test]
async fn a_thumbnail_that_is_not_media_changes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, writer) = uploader(&pool, "alice", WRITE_SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let row = insert_media(&pool, &alice).await;

    let response = put_multipart(
        &app,
        &v1_path(row.id),
        &writer,
        &[
            file_part("thumbnail", "notes.txt", TEXT),
            MultipartPart::text("description", "should not land"),
        ],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "undetectable thumbnail bytes are refused"
    );
    assert_eq!(
        response.text(),
        error_body("Unsupported file type"),
        "with the upload endpoint's wording"
    );
    let stored = fetch(&pool, row.id).await;
    assert!(stored.preview_url.is_none(), "no preview was recorded");
    assert_eq!(
        stored.description.as_deref(),
        Some("a stored caption"),
        "and the field sent with the bad thumbnail was not written either"
    );
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("the uploads directory is readable")
            .count(),
        0,
        "and nothing was written to disk"
    );
}

/// `PUT` keeps ATTACHED media read-only (Pleroma reaches its description
/// through the status edit, not here): a write to a claimed row is the same
/// 404 as `GET` gives, and a foreign user cannot edit it either.
#[tokio::test]
async fn attached_and_foreign_media_cannot_be_edited() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, writer) = uploader(&pool, "alice", BOTH_SCOPES).await;
    let (bob, _bob_token) = uploader(&pool, "bob", BOTH_SCOPES).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let mine_attached = insert_attached_media(&pool, &alice).await;
    let foreign = insert_media(&pool, &bob).await;

    for (id, token) in [(mine_attached.id, &writer), (foreign.id, &writer)] {
        let response = put_multipart(
            &app,
            &v1_path(id),
            token,
            &[MultipartPart::text("description", "not allowed")],
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{id}` is out of reach: attached or foreign"
        );
        assert_eq!(
            response.text(),
            error_body("Record not found"),
            "the 404 is the shared wording"
        );
    }
    assert_eq!(
        fetch(&pool, mine_attached.id).await.description.as_deref(),
        Some("a stored caption"),
        "the attached row kept its caption"
    );
    assert_eq!(
        fetch(&pool, foreign.id).await.description.as_deref(),
        Some("a stored caption"),
        "and so did the foreign one"
    );

    let unknown = put_multipart(
        &app,
        &v1_path(Uuid::now_v7()),
        &writer,
        &[MultipartPart::text("description", "nobody's")],
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::NOT_FOUND,
        "and an unknown id was never an upload"
    );
}

/// The gates run in order: 401 without a token (with the bearer challenge),
/// 403 for an app-only token, 403 for a token without the route's scope —
/// even Pleroma's `read:media` is not the ticket's `read:statuses` — and
/// the scope check answers before the row lookup, so a wrong scope cannot
/// probe ids.
#[tokio::test]
async fn the_scope_gates_answer_401_and_403() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, reader) = uploader(&pool, "alice", READ_SCOPE).await;
    let (_narrow, read_media) = uploader(&pool, "narrow", "read:media").await;
    let (_wrong, write_statuses) = uploader(&pool, "wrong", "write:statuses").await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let row = insert_media(&pool, &alice).await;
    let path = v1_path(row.id);

    let anonymous = common::TestRequest::get(&path).send(&app).await;
    assert_unauthorized(&anonymous, "GET");
    let anonymous_put = common::TestRequest::put(&path)
        .form(&[("description", "anon")])
        .send(&app)
        .await;
    assert_unauthorized(&anonymous_put, "PUT");

    for (label, token) in [
        ("app-only", &app_only),
        ("read:media", &read_media),
        ("write:statuses", &write_statuses),
    ] {
        let response = get(&app, &path, token).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{label} cannot read"
        );
        assert_eq!(
            response.text(),
            if label == "app-only" {
                error_body("This API requires an authenticated user")
            } else {
                error_body("Insufficient permissions: read:statuses.")
            },
            "the {label} 403 names what was missing"
        );
    }

    for (label, token) in [("read:media", &read_media), ("read:statuses", &reader)] {
        let response = put_multipart(
            &app,
            &path,
            token,
            &[MultipartPart::text("description", "denied")],
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{label} cannot write"
        );
        assert_eq!(
            response.text(),
            error_body("Insufficient permissions: write:media."),
            "the {label} 403 names the missing scope"
        );
    }

    let probing = get(&app, &v1_path(Uuid::now_v7()), &write_statuses).await;
    assert_eq!(
        probing.status,
        StatusCode::FORBIDDEN,
        "a wrong scope is a 403 even on an unknown id: the scope gate runs first"
    );
    assert_eq!(
        fetch(&pool, row.id).await.description.as_deref(),
        Some("a stored caption"),
        "and none of the denials wrote anything"
    );
}

/// There is no delete on these routes: `DELETE /api/v1/media/{id}` is a
/// method mismatch, and the `{id}` path serves only `GET` and `PUT`.
#[tokio::test]
async fn no_route_deletes_media() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, token) = uploader(&pool, "alice", BOTH_SCOPES).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let row = insert_media(&pool, &alice).await;

    let response = common::TestRequest::delete(&v1_path(row.id))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "the path exists but does not answer DELETE"
    );
    assert!(
        fetch(&pool, row.id).await.id == row.id,
        "and the row is still there"
    );
}
