//! The remaining client-to-server doors: `GET /api/ap/whoami`, answering
//! the caller's own actor document to a `read` token, and
//! `POST /api/ap/upload_media`, running the `POST /api/v1/media`
//! processing and answering the stored row as an `ActivityStreams`
//! `Document` — with its own gates, the create that claims the upload and
//! the refusal of somebody else's.

use axum::http::StatusCode;
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

use super::support::{
    AS_PUBLIC, BASE, C2S_BOUNDARY, c2s_account, location_status_id, png_fixture, post_outbox,
    show_status, upload_to_ap,
};
use crate::common;

/// `GET /api/ap/whoami` answers the authenticated user's own actor
/// document under the `ActivityPub` media type, and demands a `read`
/// token.
#[tokio::test]
async fn whoami_answers_the_callers_own_actor_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = c2s_account(&pool, "c2s-whoami", "read").await;
    let (_, write_only) = c2s_account(&pool, "c2s-whoami-writer", "write").await;

    let response = common::TestRequest::get("/api/ap/whoami")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the caller reads itself");
    assert_eq!(
        response.header("content-type"),
        Some("application/activity+json; charset=utf-8"),
        "as an ActivityPub document"
    );
    let actor = response.json_value();
    assert_eq!(actor["id"], alice.ap_id, "the actor id of the token's user");
    assert_eq!(actor["type"], "Person", "a person");
    assert_eq!(actor["preferredUsername"], "c2s-whoami", "by its nickname");

    let anonymous = common::TestRequest::get("/api/ap/whoami").send(&app).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "whoami, no token");

    let unscoped = common::TestRequest::get("/api/ap/whoami")
        .bearer(&write_only)
        .send(&app)
        .await;
    common::assert_answer(&unscoped, StatusCode::FORBIDDEN, "whoami, no read scope");
    common::assert_error(&unscoped, "Insufficient permissions: read.", "whoami scope");
}

/// `POST /api/ap/upload_media` runs the `POST /api/v1/media` processing
/// and answers the stored row as an `ActivityStreams` `Document`: the
/// media id for a create to quote, the `Link` url, the alt text, the
/// file's type and the computed `Blurhash`.
#[tokio::test]
async fn the_c2s_upload_answers_a_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let app = common::test_app_with_uploads_dir(pool.clone(), uploads.path());
    let (_, token) = c2s_account(&pool, "c2s-upload", "read write").await;

    let response = upload_to_ap(&app, Some(&token), png_fixture(9), Some("a gradient")).await;
    assert_eq!(response.status, StatusCode::OK, "the upload answers 200");
    let document = response.json_value();
    let keys: Vec<&str> = {
        let mut keys: Vec<&str> = document
            .as_object()
            .expect("a document is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    };
    assert_eq!(
        keys,
        ["blurhash", "id", "mediaType", "name", "type", "url"],
        "the documented key set"
    );
    assert_eq!(document["type"], "Document", "the object type");
    assert_eq!(document["name"], "a gradient", "the submitted alt text");
    assert_eq!(document["mediaType"], "image/png", "read from the bytes");
    let media_id = Uuid::parse_str(document["id"].as_str().expect("an id text"))
        .expect("the id is the media id itself");
    let url = &document["url"][0];
    assert_eq!(url["type"], "Link", "a Link entry");
    assert_eq!(url["mediaType"], "image/png", "carrying the file type");
    let href = url["href"].as_str().expect("an href");
    assert!(
        href.starts_with(&format!("{BASE}/media/")),
        "the file is served under the media route: {href}"
    );

    // The row exists as the caller's own unattached upload, readable
    // through the Mastodon media route — one row, two doors.
    let shown = common::TestRequest::get(&format!("/api/v1/media/{media_id}"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(shown.status, StatusCode::OK, "the upload is readable");
    let entity = shown.json_value();
    assert_eq!(entity["id"], media_id.to_string(), "under one id");
    assert_eq!(entity["url"], href, "served at one address");

    // A bare upload carries no name text and still reports its hash.
    let bare = upload_to_ap(&app, Some(&token), png_fixture(3), None).await;
    assert_eq!(
        bare.status,
        StatusCode::OK,
        "the file-less-description upload lands"
    );
    assert_eq!(bare.json_value()["name"], "", "an absent alt text is empty");
}

/// The upload's gates and refusals: no token 401, a token without
/// `write:media` 403, and no `file` part the shared 422 — the same
/// answers `POST /api/v1/media` gives, on the C2S address.
#[tokio::test]
async fn the_c2s_upload_demands_media_write_and_a_file() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let app = common::test_app_with_uploads_dir(pool.clone(), uploads.path());
    let (_, read_only) = c2s_account(&pool, "c2s-upload-reader", "read").await;
    let (_, media_token) = c2s_account(&pool, "c2s-upload-writer", "write:media").await;

    let anonymous = upload_to_ap(&app, None, png_fixture(1), None).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "upload, no token");

    let unscoped = upload_to_ap(&app, Some(&read_only), png_fixture(2), None).await;
    common::assert_answer(&unscoped, StatusCode::FORBIDDEN, "upload, no media scope");
    common::assert_error(
        &unscoped,
        "Insufficient permissions: write:media.",
        "upload scope",
    );

    let no_file = common::TestRequest::post("/api/ap/upload_media")
        .bearer(&media_token)
        .multipart(
            C2S_BOUNDARY,
            &[common::MultipartPart::text("description", "none")],
        )
        .send(&app)
        .await;
    common::assert_answer(
        &no_file,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an upload with no file part",
    );
    common::assert_error(
        &no_file,
        "Missing parameter: file",
        "an upload with no file part",
    );
}

/// The upload the create claims: the `Document`'s `id` is exactly what a
/// posted `Create` names in its `attachment` — and the whole round trip
/// from upload to attached post runs through the C2S door.
#[tokio::test]
async fn the_c2s_upload_feeds_the_c2s_create() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let app = common::test_app_with_uploads_dir(pool.clone(), uploads.path());
    let (_alice, token) = c2s_account(&pool, "c2s-upload-claim", "read write").await;

    let upload = upload_to_ap(&app, Some(&token), png_fixture(7), None).await;
    let document = upload.json_value();
    let media_id = document["id"].as_str().expect("the media id").to_owned();

    let activity = json!({
        "type": "Create",
        "to": [AS_PUBLIC],
        "object": {
            "type": "Note",
            "content": "look at this",
            "attachment": [{"type": "Document", "id": format!("{BASE}/api/v1/media/{media_id}")}],
        },
    });
    let created = post_outbox(&app, "c2s-upload-claim", Some(&token), activity).await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "the create claims the upload"
    );
    let entity = show_status(&app, &token, location_status_id(&created))
        .await
        .json_value();
    assert_eq!(
        entity["media_attachments"][0]["id"], media_id,
        "the upload id read whether bare or as the media route's URL"
    );
}

/// Somebody else's upload is not the caller's to attach: the create that
/// names a stranger's media id earns the media model's own 422.
#[tokio::test]
async fn the_c2s_create_cannot_claim_another_users_upload() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let app = common::test_app_with_uploads_dir(pool.clone(), uploads.path());
    let (_, carol_token) = c2s_account(&pool, "c2s-upload-stranger", "read write").await;
    let (_, token) = c2s_account(&pool, "c2s-upload-thief", "read write").await;

    let upload = upload_to_ap(&app, Some(&carol_token), png_fixture(5), None).await;
    let media_id = upload.json_value()["id"]
        .as_str()
        .expect("the media id")
        .to_owned();
    let activity = json!({
        "type": "Create",
        "to": [AS_PUBLIC],
        "object": {
            "type": "Note",
            "content": "stolen goods",
            "attachment": [{"id": media_id}],
        },
    });
    let refused = post_outbox(&app, "c2s-upload-thief", Some(&token), activity).await;
    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a foreign upload",
    );
    common::assert_error(&refused, "Media not found", "a foreign upload");
}
