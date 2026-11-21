//! The two endpoint versions, client thumbnails, key uniqueness and the
//! upload-only method gate.

use axum::{body::Bytes, http::StatusCode};
use serde_json::Value;

use super::support::{
    SCOPE, TEXT, V1, V2, entity_id, entries_in, file_part, jpeg, mime_of, png, row_count,
    storage_key, stored_row, upload, upload_file, uploader, uploads_dir, url_of,
};
use crate::common::{self, MultipartPart};
/// The two versions are one endpoint: same status, same entity keys, same
/// fields, and two separate uploads.
#[tokio::test]
async fn v1_and_v2_answer_the_same() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "quinn", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let fixture = png(200, 100);

    let mut entities: Vec<(String, Value)> = Vec::new();
    for path in [V1, V2] {
        let parts = [
            file_part("cat.png", "image/png", fixture.clone()),
            MultipartPart::text("description", "the same cat"),
            MultipartPart::text("focus", "0,0"),
        ];
        let response = upload(&app, path, &token, &parts).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{path} answers 200, not Mastodon's 202: uploads are synchronous"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "{path} answers JSON"
        );
        entities.push((path.to_owned(), response.json_value()));
    }

    let (_, first) = &entities[0];
    let (second_path, second) = &entities[1];
    assert_eq!(
        sorted_keys(first),
        sorted_keys(second),
        "{second_path} reports the same keys as v1"
    );
    for field in ["type", "description", "blurhash", "remote_url", "meta"] {
        assert_eq!(
            first[field], second[field],
            "the {field} of the two responses agrees"
        );
    }
    assert_ne!(
        first["id"], second["id"],
        "but they are two separate uploads"
    );
    assert_ne!(
        url_of(first),
        url_of(second),
        "each with its own unguessable url"
    );
    assert_ne!(
        first["preview_url"], second["preview_url"],
        "and its own generated preview"
    );

    let ids: Vec<String> = entities
        .iter()
        .map(|(_, entity)| entity_id(entity))
        .collect();
    for id in &ids {
        let row = stored_row(&pool, id).await;
        assert_eq!(row.user_id, Some(user.id), "both rows belong to the caller");
        assert!(row.status_id.is_none(), "and both are unattached");
        let key = row.storage_key.clone().expect("a local upload has a key");
        let served = common::TestRequest::get(&format!("/media/{key}"))
            .send(&app)
            .await;
        assert_eq!(
            served.body,
            Bytes::from(fixture.clone()),
            "and its file serves from the announced url"
        );
    }
}

/// A client-sent `thumbnail` part is accepted and ignored, whether sent as a
/// file part or as a text field: the preview the row records is the generated
/// one, keyed off the `file` part's stem, not the posted thumbnail's bytes.
#[tokio::test]
async fn a_client_thumbnail_is_ignored_for_the_generated_preview() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "rosa", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let fixture = png(64, 64);

    let parts = [
        file_part("cat.png", "image/png", fixture.clone()),
        MultipartPart::text("description", "a cat"),
        MultipartPart::file(
            "thumbnail",
            "thumb.png",
            "image/png",
            Bytes::from(jpeg(64, 64)),
        ),
        MultipartPart::text("thumbnail", "ignored"),
    ];
    let entity = upload(&app, V1, &token, &parts).await.json_value();
    assert_eq!(entity["type"], "image", "the upload went through");
    assert_eq!(
        entity["description"], "a cat",
        "the description belongs to the file"
    );
    assert_eq!(mime_of(&entity), "image/png", "and one type for one file");

    let id = entity_id(&entity);
    let row = stored_row(&pool, &id).await;
    let key = row.storage_key.clone().expect("a storage key");
    assert_eq!(
        row.content_type, "image/png",
        "the client thumbnail did not change the stored type"
    );
    let stem = key
        .strip_suffix(".png")
        .expect("the key names its extension");
    assert_eq!(
        storage_key(row.preview_url.as_deref().expect("a preview url")),
        format!("{stem}_small.jpg"),
        "the preview is generated from the file, not taken from the part"
    );
    let served = common::TestRequest::get(&format!("/media/{key}"))
        .send(&app)
        .await;
    assert_eq!(
        served.body,
        Bytes::from(fixture),
        "and what got stored is the `file` part"
    );
    assert_eq!(row_count(&pool, &user).await, 1, "one file in, one row");
    assert_eq!(
        entries_in(&dir),
        1,
        "one namespace directory holding the file and its preview"
    );
}

/// Rejected uploads leave nothing behind, and repeated uploads of the same
/// bytes never share a key.
#[tokio::test]
async fn repeated_uploads_do_not_share_a_key() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "sue", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let fixture = png(16, 16);

    let mut keys: Vec<String> = Vec::new();
    for _ in 0..3 {
        let entity = upload_file(
            &app,
            V1,
            &token,
            file_part("cat.png", "image/png", fixture.clone()),
        )
        .await;
        keys.push(storage_key(&url_of(&entity)));
        let rejected = upload(&app, V1, &token, &[file_part("x.txt", "image/png", TEXT)]).await;
        assert_eq!(
            rejected.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "the unsupported file between them is refused"
        );
    }
    let unique: std::collections::HashSet<String> = keys.iter().cloned().collect();
    assert_eq!(unique.len(), 3, "every upload minted its own key");
    for key in &keys {
        assert!(
            key.starts_with(&format!("{}/", user.id)),
            "and every key is in the uploader's namespace: {key}"
        );
    }
    assert_eq!(
        row_count(&pool, &user).await,
        3,
        "one row per accepted upload"
    );
    assert_eq!(
        entries_in(&dir),
        1,
        "one directory, shared by the uploader's three files"
    );
}

/// The upload routes are `POST`-only: a `GET` there is a method mismatch, not
/// a path the fallback swallows.
#[tokio::test]
async fn only_post_uploads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for path in [V1, V2] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(
            response.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{path} is not readable"
        );
        assert!(response.body.is_empty(), "axum's 405 body is empty");
    }
}

/// The keys of an entity object, sorted, for comparing two responses exactly.
///
/// # Panics
///
/// If the value is not an object.
fn sorted_keys(entity: &Value) -> Vec<&str> {
    let mut names: Vec<&str> = entity
        .as_object()
        .expect("an entity object")
        .keys()
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    names
}
