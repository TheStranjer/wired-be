//! The three profile images (`avatar`, `header`, `pleroma_background_image`):
//! upload and serving, replacement and clearing, the byte caps and the type
//! allowlist with both 422s.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    BMP, DEFAULT_AVATAR, DEFAULT_HEADER, GIF, JPEG, MEDIA, PDF, PNG, SMALL_PNG, TEXT, WEBP,
    assert_422, authed_app_with_uploads, extension_of, file_part, key_of, ok_body, part,
    patch_form, patch_json, patch_multipart, served, stored, stored_files, upload, uploads_dir,
    url_of,
};
use crate::common;

/// The three profile images upload through one multipart body: each column
/// holds the public URL of its own stored file, the entity renders them in
/// `avatar`, `header` and `pleroma.background_image`, and `/media` serves
/// every one of them with the bytes as sent.
#[tokio::test]
async fn the_three_profile_images_upload_and_are_served() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let body = ok_body(
        &patch_multipart(
            &app,
            &token,
            &[
                file_part("avatar", PNG),
                file_part("header", JPEG),
                file_part("pleroma_background_image", GIF),
            ],
        )
        .await,
    );
    let avatar = url_of(&body["avatar"]);
    let header = url_of(&body["header"]);
    let background = url_of(&body["pleroma"]["background_image"]);
    assert_eq!(body["avatar_static"], body["avatar"], "the static pair");
    assert_eq!(body["header_static"], body["header"], "is the same URL");
    assert_ne!(avatar, header, "and each slot got its own file");

    let stored_rows = [
        ("avatar", &avatar, "png", PNG, "image/png"),
        ("header", &header, "jpg", JPEG, "image/jpeg"),
        ("background", &background, "gif", GIF, "image/gif"),
    ];
    for (name, url, extension, bytes, mime) in stored_rows {
        assert!(
            url.starts_with(&format!("{MEDIA}/{}/", alice.id)),
            "{name} is namespaced to the uploader: {url}"
        );
        assert_eq!(extension_of(url), extension, "{name} keeps its type");
        assert_eq!(
            served(&dir, url).as_deref(),
            Some(bytes),
            "{name} is on disk under its key"
        );
        let response = common::TestRequest::get(&format!("/media/{}", key_of(url)))
            .send(&app)
            .await;
        assert_eq!(response.status, StatusCode::OK, "{name} is served");
        assert_eq!(
            response.header("content-type"),
            Some(mime),
            "{name} serves as the type its bytes are"
        );
        assert_eq!(response.body.as_ref(), bytes, "the served {name} bytes");
    }
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.avatar_url.as_deref(), Some(avatar.as_str()), "stored");
    assert_eq!(row.banner_url.as_deref(), Some(header.as_str()), "stored");
    assert_eq!(
        row.background_image_url.as_deref(),
        Some(background.as_str()),
        "stored"
    );
}

/// `webp` is the fourth accepted image type, and the endpoint accepts it for
/// the avatar: the stored key carries the extension the detected type gives.
#[tokio::test]
async fn a_webp_avatar_uploads_like_the_others() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let body = ok_body(&upload(&app, &token, "avatar", WEBP).await);
    let avatar = url_of(&body["avatar"]);
    assert_eq!(extension_of(&avatar), "webp", "stored as a webp");
    assert_eq!(served(&dir, &avatar).as_deref(), Some(WEBP), "the bytes");
    assert_eq!(
        stored(&pool, alice.id).await.banner_url,
        None,
        "the slot the body did not send stays empty"
    );
}

/// A replacement stores a new key and removes the file the column named
/// before it; the other two images are untouched, file and all.
#[tokio::test]
async fn replacing_an_image_deletes_the_previous_file() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let first = url_of(&ok_body(&upload(&app, &token, "avatar", PNG).await)["avatar"]);
    let header = url_of(&ok_body(&upload(&app, &token, "header", JPEG).await)["header"]);
    assert_eq!(stored_files(&dir), 2, "avatar and banner on disk");

    let second = url_of(&ok_body(&upload(&app, &token, "avatar", WEBP).await)["avatar"]);
    assert_ne!(first, second, "a replacement mints a fresh key");
    assert_eq!(served(&dir, &second).as_deref(), Some(WEBP), "the new file");
    assert_eq!(served(&dir, &first), None, "the replaced file is gone");
    assert_eq!(
        served(&dir, &header).as_deref(),
        Some(JPEG),
        "and the image nobody touched stays where it was"
    );
    assert_eq!(stored_files(&dir), 2, "one in, one out");
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.avatar_url.as_deref(), Some(second.as_str()), "stored");
    assert_eq!(row.banner_url.as_deref(), Some(header.as_str()), "stored");
}

/// Both spellings of the reset clear their column, answer the instance
/// default again, and remove the file the column had named: the empty
/// string through a form and a multipart text part, the `null` through
/// JSON.
#[tokio::test]
async fn clearing_an_image_resets_the_entity_and_deletes_the_file() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let avatar = url_of(&ok_body(&upload(&app, &token, "avatar", PNG).await)["avatar"]);
    let header = url_of(&ok_body(&upload(&app, &token, "header", JPEG).await)["header"]);
    let background = url_of(
        &ok_body(&upload(&app, &token, "pleroma_background_image", GIF).await)["pleroma"]["background_image"],
    );
    assert_eq!(stored_files(&dir), 3, "all three images on disk");

    let cleared = ok_body(&patch_form(&app, &token, &[("avatar", "")]).await);
    assert_eq!(
        cleared["avatar"], DEFAULT_AVATAR,
        "the empty form string resets the entity"
    );
    assert_eq!(served(&dir, &avatar), None, "and its file goes with it");

    let nulled = ok_body(&patch_json(&app, &token, &json!({"header": null})).await);
    assert_eq!(
        nulled["header"], DEFAULT_HEADER,
        "a JSON null resets the banner"
    );
    assert_eq!(served(&dir, &header), None, "and deletes its file");

    let text_part =
        ok_body(&patch_multipart(&app, &token, &[part("pleroma_background_image", "")]).await);
    assert_eq!(
        text_part["pleroma"]["background_image"],
        Value::Null,
        "an empty multipart text part clears the background too"
    );
    assert_eq!(served(&dir, &background), None, "and deletes that file");

    assert_eq!(stored_files(&dir), 0, "every replaced file is gone");
    let row = stored(&pool, alice.id).await;
    assert!(
        row.avatar_url.is_none() && row.banner_url.is_none() && row.background_image_url.is_none(),
        "all three columns are NULL again: {row:?}"
    );

    // And the slot can be filled once more after the clear.
    let again = url_of(&ok_body(&upload(&app, &token, "avatar", GIF).await)["avatar"]);
    assert_eq!(
        served(&dir, &again).as_deref(),
        Some(GIF),
        "a new upload works"
    );
}

/// Clearing an image that was never set is the accepted no-op, not an
/// error, and it leaves a stored image of another slot alone.
#[tokio::test]
async fn clearing_an_unset_image_changes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, _) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let header = url_of(&ok_body(&upload(&app, &token, "header", JPEG).await)["header"]);
    let body = ok_body(&patch_json(&app, &token, &json!({"avatar": null})).await);
    assert_eq!(body["avatar"], DEFAULT_AVATAR, "the avatar stays unset");
    assert_eq!(
        body["header"], header,
        "an absent slot keeps the stored image"
    );
    assert_eq!(stored_files(&dir), 1, "and no file was deleted");
}

/// The two byte caps: the avatar is measured by `AVATAR_LIMIT_BYTES`, the
/// banner and the background by `BANNER_LIMIT_BYTES`, and passing either is
/// the 422 "File is too large" that stores nothing.
#[tokio::test]
async fn an_oversize_profile_image_is_the_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    // The fixtures are 12 to 20 bytes, so the caps below are within reach of
    // every one of them and the general `UPLOAD_LIMIT_BYTES` never fires.
    let mut config = common::test_config_with_uploads_dir(dir.path());
    config.avatar_limit_bytes = 16;
    config.banner_limit_bytes = 14;
    let app = common::test_app_with_config(pool.clone(), config);

    // [`PNG`] is 20 bytes, over both caps; [`GIF`] is 14, within the banner's.
    for field in ["avatar", "header", "pleroma_background_image"] {
        let response = upload(&app, &token, field, PNG).await;
        assert_422(
            &response,
            "File is too large",
            &format!("{field} is over its cap"),
        );
    }
    assert_eq!(stored_files(&dir), 0, "a refused upload stores nothing");
    let row = stored(&pool, alice.id).await;
    assert!(
        row.avatar_url.is_none() && row.banner_url.is_none() && row.background_image_url.is_none(),
        "and writes nothing: {row:?}"
    );

    let banner = ok_body(&upload(&app, &token, "header", GIF).await);
    assert_eq!(
        extension_of(&url_of(&banner["header"])),
        "gif",
        "BANNER_LIMIT_BYTES is exactly what measures the banner"
    );

    // The avatar's own cap is the other number: the 14-byte [`GIF`] the
    // banner above accepted is over an 8-byte avatar cap, while the 8-byte
    // PNG signature fits it exactly.
    let mut strict = common::test_config_with_uploads_dir(dir.path());
    strict.avatar_limit_bytes = SMALL_PNG.len();
    let strict_app = common::test_app_with_config(pool.clone(), strict);
    let response = upload(&strict_app, &token, "avatar", GIF).await;
    assert_422(
        &response,
        "File is too large",
        "AVATAR_LIMIT_BYTES caps the avatar on its own number",
    );
    let avatar = ok_body(&upload(&strict_app, &token, "avatar", SMALL_PNG).await);
    assert_eq!(
        extension_of(&url_of(&avatar["avatar"])),
        "png",
        "a file exactly at the cap is accepted"
    );
}

/// The type allowlist: a `BMP` and a `PDF` are both bytes a matcher names and
/// neither is a profile image, so both are the 422 "Unsupported file type"
/// that stores nothing.
#[tokio::test]
async fn a_wrong_image_type_is_the_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    for bytes in [BMP, PDF] {
        for field in ["avatar", "header", "pleroma_background_image"] {
            let response = upload(&app, &token, field, bytes).await;
            assert_422(
                &response,
                "Unsupported file type",
                &format!("{field} cannot be {}", hex_head(bytes)),
            );
        }
    }
    assert_eq!(stored_files(&dir), 0, "a refused upload stores nothing");
    assert!(
        stored(&pool, alice.id).await.avatar_url.is_none(),
        "and writes nothing"
    );
}

/// The first bytes of a fixture, spelled out for a failure message.
fn hex_head(bytes: &[u8]) -> String {
    let head: Vec<String> = bytes
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02X}"))
        .collect();
    format!("{}-byte fixture ({})", bytes.len(), head.join(" "))
}

/// Non-image bytes under an `image/png` part header: the claim is what the
/// client says, the answer is what the bytes are, so this is the same 422 as
/// any other non-image — and an empty part with it.
#[tokio::test]
async fn non_image_bytes_with_an_image_content_type_are_the_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, _) = authed_app_with_uploads(&pool, "alice", &dir).await;
    for bytes in [TEXT, PDF, b""] {
        // Every fixture from [`file_part`] arrives as `image/png`, named
        // `.png`: neither the claim nor the name carries the decision.
        let response = upload(&app, &token, "avatar", bytes).await;
        assert_422(
            &response,
            "Unsupported file type",
            &format!("{} bytes claimed image/png", bytes.len()),
        );
    }
    assert_eq!(stored_files(&dir), 0, "nothing reached storage");
    let response = upload(&app, &token, "header", TEXT).await;
    assert_422(&response, "Unsupported file type", "the banner checks too");
}

/// One bad image in a body with a good one refuses the whole request: every
/// slot is checked before any file is stored, so the good avatar never
/// reaches disk and the row never moves.
#[tokio::test]
async fn a_body_with_one_bad_image_stores_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let response = patch_multipart(
        &app,
        &token,
        &[
            part("display_name", "Ada"),
            file_part("avatar", PNG),
            file_part("header", PDF),
        ],
    )
    .await;
    assert_422(
        &response,
        "Unsupported file type",
        "the bad header sinks the body",
    );
    assert_eq!(
        stored_files(&dir),
        0,
        "and the good avatar was never stored"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.name, "", "no column of the same body was written");
    assert!(row.avatar_url.is_none(), "and no image either");
}

/// The image slots are absent from a text-only body, so an account with a
/// stored avatar keeps it (and its file) through a name or note update.
#[tokio::test]
async fn a_text_update_leaves_a_stored_image_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let avatar = url_of(&ok_body(&upload(&app, &token, "avatar", PNG).await)["avatar"]);
    let body = ok_body(&patch_json(&app, &token, &json!({"display_name": "Grace"})).await);
    assert_eq!(
        body["avatar"], avatar,
        "an absent avatar parameter keeps the stored image"
    );
    assert_eq!(served(&dir, &avatar).as_deref(), Some(PNG), "and its file");
    assert_eq!(
        stored(&pool, alice.id).await.avatar_url.as_deref(),
        Some(avatar.as_str()),
        "stored through the unrelated write"
    );
}
