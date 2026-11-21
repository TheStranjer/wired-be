//! Upload validation: stored descriptions and focus points, missing and
//! empty files, non-media files and the type the bytes decide.

use axum::{body::Bytes, http::StatusCode};
use serde_json::json;

use super::support::{
    PDF, SCOPE, TEXT, V1, V2, entity_id, entries_in, error_body, file_part, key_extension, mime_of,
    original_meta, png, row_count, storage_key, stored_row, upload, upload_file, uploader,
    uploads_dir, url_of,
};
use crate::common::{self, MultipartPart};
/// `description` and `focus` reach the row and the entity reports them
/// beside the detected dimensions.
#[tokio::test]
async fn description_and_focus_are_stored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "dave", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let parts = [
        file_part("mat.png", "image/png", png(80, 60)),
        MultipartPart::text("description", "a cat on a mat"),
        MultipartPart::text("focus", "0.25,-0.5"),
    ];
    let entity = upload(&app, V1, &token, &parts).await.json_value();
    assert_eq!(
        entity["description"], "a cat on a mat",
        "the alt text comes back"
    );
    assert_eq!(
        entity["meta"]["focus"],
        json!({ "x": 0.25, "y": -0.5 }),
        "the focal point is reported"
    );
    assert_eq!(
        entity["meta"]["original"],
        original_meta(80, 60),
        "and so are the detected dimensions"
    );

    let id = entity_id(&entity);
    let row = stored_row(&pool, &id).await;
    assert_eq!(
        row.description.as_deref(),
        Some("a cat on a mat"),
        "the row keeps the alt text"
    );
    assert_eq!(row.focus_x, Some(0.25), "the row keeps focus x");
    assert_eq!(row.focus_y, Some(-0.5), "the row keeps focus y");
    assert_eq!(row.user_id, Some(user.id), "for the uploader");

    let both_ends = [
        file_part("edge.png", "image/png", png(80, 60)),
        MultipartPart::text("focus", "-1,1"),
    ];
    let edged = upload(&app, V1, &token, &both_ends).await.json_value();
    assert_eq!(
        edged["meta"]["focus"],
        json!({ "x": -1.0, "y": 1.0 }),
        "both ends of the range are inclusive"
    );
}

/// A `focus` that is not two floats in `[-1.0, 1.0]` is a 422, and nothing is
/// stored for it.
#[tokio::test]
async fn an_invalid_focus_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "erin", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

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
        let parts = [
            file_part("mat.png", "image/png", png(32, 32)),
            MultipartPart::text("focus", focus),
        ];
        let response = upload(&app, V1, &token, &parts).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{focus}` is not a focal point"
        );
        assert_eq!(
            response.text(),
            error_body("Validation failed: Focus is invalid"),
            "the 422 says what was wrong with `{focus}`"
        );
    }
    assert_eq!(
        row_count(&pool, &user).await,
        0,
        "a rejected focus stores no media"
    );
}

/// An absent `focus`, and one sent empty, store no focal point instead of
/// failing: clearing the value is not a validation error.
#[tokio::test]
async fn an_empty_focus_stores_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "fred", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let blank = [
        file_part("mat.png", "image/png", png(32, 32)),
        MultipartPart::text("focus", ""),
    ];
    let entity = upload(&app, V1, &token, &blank).await.json_value();
    assert_eq!(
        entity["meta"].get("focus"),
        None,
        "an empty focus is no focus"
    );
    assert_eq!(
        entity["meta"]["original"],
        original_meta(32, 32),
        "while the detected dimensions stay reported"
    );
    let id = entity_id(&entity);
    let row = stored_row(&pool, &id).await;
    assert_eq!(row.focus_x, None, "and stores no x");
    assert_eq!(row.focus_y, None, "nor y");
    assert_eq!(
        row.user_id,
        Some(user.id),
        "the upload is still the caller's"
    );

    let absent = upload_file(
        &app,
        V1,
        &token,
        file_part("mat.png", "image/png", png(32, 32)),
    )
    .await;
    assert_eq!(absent["meta"].get("focus"), None, "and so is an absent one");
}

/// No `file` part at all is the ticket's 422, whatever else the body holds.
#[tokio::test]
async fn a_body_without_a_file_is_a_missing_parameter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "gina", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let cases = [
        // Only a text field.
        upload(
            &app,
            V1,
            &token,
            &[MultipartPart::text("description", "no file here")],
        )
        .await,
        // No parts at all.
        upload(&app, V1, &token, &[]).await,
        // The file under another field name.
        upload(
            &app,
            V1,
            &token,
            &[MultipartPart::file(
                "upload",
                "cat.png",
                "image/png",
                Bytes::from(png(16, 16)),
            )],
        )
        .await,
        // A `POST` to the second version's path, the same body.
        upload(
            &app,
            V2,
            &token,
            &[MultipartPart::text("description", "no file here")],
        )
        .await,
        // A JSON body, which carries no files.
        common::TestRequest::post(V1)
            .bearer(&token)
            .json(&json!({"description": "no file"}))
            .send(&app)
            .await,
    ];
    for response in cases {
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a body without a `file` part is a 422, got: {}",
            response.text()
        );
        assert_eq!(
            response.text(),
            error_body("Missing parameter: file"),
            "the 422 names the parameter that was missing"
        );
    }
    assert_eq!(
        row_count(&pool, &user).await,
        0,
        "and a rejected body stores no media"
    );
}

/// A `file` part that carries no bytes is rejected as blank: an empty file has
/// no magic bytes to detect and would store nothing.
#[tokio::test]
async fn an_empty_file_is_rejected() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "heidi", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for path in [V1, V2] {
        let response = upload(
            &app,
            path,
            &token,
            &[file_part("empty.png", "image/png", "")],
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path} refuses a zero-length file"
        );
        assert_eq!(
            response.text(),
            error_body("Validation failed: File can't be blank"),
            "the 422 says the file was blank"
        );
    }
    assert_eq!(row_count(&pool, &user).await, 0, "nothing was stored");
    assert_eq!(
        entries_in(&dir),
        0,
        "and nothing was written to the uploads directory"
    );
}

/// Bytes that are not image, video or audio never enter the store, whether the
/// detector names their type or not.
#[tokio::test]
async fn a_file_that_is_not_media_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "ivan", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for (bytes, filename) in [(PDF, "report.pdf"), (TEXT, "notes.txt")] {
        let response = upload(&app, V1, &token, &[file_part(filename, "image/png", bytes)]).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{filename} is not media, whatever its part claimed"
        );
        assert_eq!(
            response.text(),
            error_body("Unsupported file type"),
            "the 422 is about the type"
        );
    }
    assert_eq!(
        row_count(&pool, &user).await,
        0,
        "no row for a refused file"
    );
    assert_eq!(entries_in(&dir), 0, "and no file on disk");
}

/// The bytes decide the type, never the part header: a `PNG` sent as HTML is
/// stored, named and served as the image it is.
#[tokio::test]
async fn the_client_header_never_decides_the_type() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "judy", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let entity = upload_file(
        &app,
        V1,
        &token,
        file_part("cat.png", "text/html", png(120, 90)),
    )
    .await;
    assert_eq!(entity["type"], "image", "the detected kind wins");
    assert_eq!(mime_of(&entity), "image/png", "and so does the mime type");

    let key = storage_key(&url_of(&entity));
    assert_eq!(
        key_extension(&key),
        "png",
        "the key follows the bytes, not the header"
    );
    let served = common::TestRequest::get(&format!("/media/{key}"))
        .send(&app)
        .await;
    assert_eq!(
        served.header("content-type"),
        Some("image/png"),
        "the file serves as what it is, not as what was claimed"
    );
    assert_eq!(
        served.header("content-disposition"),
        Some("inline"),
        "and it is the media the extension says"
    );
}
