//! Upload limits and access: the size caps, anonymous callers and the
//! scopes that may upload.

use axum::{body::Bytes, http::StatusCode};

use super::support::{
    BOUNDARY, MP4, SCOPE, V1, V2, entries_in, error_body, file_part, long_png_header, png,
    row_count, storage_key, upload, upload_file, uploader, uploads_dir, url_of,
};
use crate::common;
/// A file over `UPLOAD_LIMIT_BYTES` is a 413 naming the limit it passed.
#[tokio::test]
async fn an_oversize_file_is_payload_too_large() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "ken", SCOPE).await;
    let app = common::test_app_with_upload_limits(pool.clone(), dir.path(), 64);

    for path in [V1, V2] {
        let response = upload(
            &app,
            path,
            &token,
            &[file_part("big.png", "image/png", long_png_header())],
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{path} refuses a file over the per-file limit"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the 413 is the standard JSON error"
        );
        let body = response.json_value();
        let message = body["error"].as_str().expect("an error message");
        assert!(
            message.contains("64 bytes"),
            "the 413 names the limit it enforced: {message}"
        );
    }
    assert_eq!(
        row_count(&pool, &user).await,
        0,
        "an oversize file stores nothing"
    );
}

/// A file larger than axum's 2 MiB extractor default but within
/// `UPLOAD_LIMIT_BYTES` uploads whole: the media route carries
/// `upload_body_limit`, which is what makes the configured limit the one that
/// applies. A video stands in here: its bytes only need the magic header the
/// detector reads, since nothing decodes them.
#[tokio::test]
async fn a_file_over_the_extractor_default_uploads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "lana", SCOPE).await;
    let app = common::test_app_with_upload_limits(pool.clone(), dir.path(), 4_000_000);

    // The `MP4` signature, padded just past the 2 MiB axum defaults at.
    let mut big: Vec<u8> = MP4.to_vec();
    big.resize(2 * 1024 * 1024 + 1024, b'x');
    let expected = Bytes::from(big.clone());

    let entity = upload_file(&app, V1, &token, file_part("big.mp4", "video/mp4", big)).await;
    let key = storage_key(&url_of(&entity));
    let served = common::TestRequest::get(&format!("/media/{key}"))
        .send(&app)
        .await;
    assert_eq!(served.status, StatusCode::OK, "the file serves back");
    assert_eq!(
        served.body.len(),
        expected.len(),
        "the whole file arrived, not a truncated head"
    );
    assert_eq!(served.body, expected, "and its bytes are intact");
    assert_eq!(
        entity["pleroma"]["mime_type"], "video/mp4",
        "the type still came from the signature"
    );
}

/// Without a token the endpoint answers 401 and the body is never stored.
#[tokio::test]
async fn an_anonymous_upload_is_unauthorized() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for path in [V1, V2] {
        let response = common::TestRequest::post(path)
            .multipart(BOUNDARY, &[file_part("cat.png", "image/png", png(16, 16))])
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{path} needs a token"
        );
        assert_eq!(
            response.text(),
            error_body("Invalid credentials"),
            "the 401 is Pleroma's wording"
        );
        assert_eq!(
            response.header("www-authenticate"),
            Some("Bearer"),
            "and it names the scheme"
        );
    }
    assert_eq!(entries_in(&dir), 0, "no file was written");
}

/// A token that does not grant `write:media` is a 403 naming the scope, and an
/// app-only token cannot upload at all.
#[tokio::test]
async fn a_token_without_the_scope_is_forbidden() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, read_media) = uploader(&pool, "mona", "read:media").await;
    let (_other, write_statuses) = uploader(&pool, "nora", "write:statuses").await;
    let (_third, read) = uploader(&pool, "olly", "read").await;
    let app_only = common::create_app_only_token(&pool, "write").await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let denials = [
        ("read:media", &read_media),
        ("write:statuses", &write_statuses),
        ("read", &read),
    ];
    for (label, token) in denials {
        let response = upload(
            &app,
            V1,
            token,
            &[file_part("cat.png", "image/png", png(16, 16))],
        )
        .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{label} is a 403");
        assert_eq!(
            response.text(),
            error_body("Insufficient permissions: write:media."),
            "the 403 names the missing scope for {label}"
        );
    }

    let anonymous = upload(
        &app,
        V1,
        &app_only,
        &[file_part("cat.png", "image/png", png(16, 16))],
    )
    .await;
    assert_eq!(
        anonymous.status,
        StatusCode::FORBIDDEN,
        "an app-only token is not a user"
    );
    assert_eq!(
        anonymous.text(),
        error_body("This API requires an authenticated user"),
        "and the 403 says so"
    );
    assert_eq!(
        row_count(&pool, &user).await,
        0,
        "a forbidden upload stores nothing"
    );
}

/// The broader `write` scope uploads too: `write:media` is satisfied by its
/// top-level scope as well as by itself.
#[tokio::test]
async fn the_broader_write_scope_uploads_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "peggy", "write").await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for path in [V1, V2] {
        let response = upload(
            &app,
            path,
            &token,
            &[file_part("cat.png", "image/png", png(16, 16))],
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "`write` covers `write:media` on {path}: {}",
            response.text()
        );
    }
}
