//! The default avatar, banner, thumbnail and mascot the API advertises
//! (`src/default_assets.rs`): each is served by `GET /media/default/{name}`
//! with its image type, and the URLs the entities carry resolve to them.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::Value;
use wired_be::config::TEST_BASE_URL;

use crate::common;

/// GETs `path` through `app` and returns the response.
async fn get(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).send(app).await
}

/// The path of an advertised URL on the test instance's origin.
fn path_of(url: &Value) -> String {
    url.as_str()
        .and_then(|text| text.strip_prefix(TEST_BASE_URL))
        .unwrap_or_else(|| panic!("{url} is not a URL on {TEST_BASE_URL}"))
        .to_owned()
}

/// Every default asset answers 200 with its image type, cached and inline,
/// and the bytes are an image of that type.
#[tokio::test]
async fn every_default_asset_is_served_as_an_image() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    for (name, content_type, magic) in [
        ("avi.png", "image/png", &b"\x89PNG\r\n\x1a\n"[..]),
        ("banner.png", "image/png", &b"\x89PNG\r\n\x1a\n"[..]),
        ("mascot.png", "image/png", &b"\x89PNG\r\n\x1a\n"[..]),
        ("thumbnail.jpg", "image/jpeg", &b"\xFF\xD8\xFF"[..]),
    ] {
        let response = get(&app, &format!("/media/default/{name}")).await;
        assert_eq!(response.status, StatusCode::OK, "{name} is served");
        assert_eq!(
            response.header("content-type"),
            Some(content_type),
            "{name} has its image type"
        );
        assert_eq!(
            response.header("content-disposition"),
            Some("inline"),
            "{name} renders in the browser"
        );
        assert!(
            response.body.starts_with(magic),
            "{name} is an image of that type"
        );
    }
}

/// A name that is not one of the four falls through to the uploads
/// directory and answers the ordinary 404.
#[tokio::test]
async fn an_unknown_default_name_is_not_found() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    let response = get(&app, "/media/default/other.png").await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "only the four assets live under default/"
    );
}

/// A fresh account's `avatar` and `header` end in the default paths, and
/// following those URLs returns an image.
#[tokio::test]
async fn a_fresh_accounts_default_images_resolve() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);
    let account = get(&app, &format!("/api/v1/accounts/{}", alice.id))
        .await
        .json_value();
    for (key, suffix) in [
        ("avatar", "/media/default/avi.png"),
        ("avatar_static", "/media/default/avi.png"),
        ("header", "/media/default/banner.png"),
        ("header_static", "/media/default/banner.png"),
    ] {
        let url = account[key].as_str().unwrap_or_default();
        assert!(url.ends_with(suffix), "{key} ends in {suffix}: {url}");
        let image = get(&app, &path_of(&account[key])).await;
        assert_eq!(image.status, StatusCode::OK, "{key} resolves");
        assert_eq!(
            image.header("content-type"),
            Some("image/png"),
            "{key} is a PNG"
        );
    }
}

/// The instance thumbnail on both documents resolves to the JPEG.
#[tokio::test]
async fn the_instance_thumbnail_resolves() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    let v1 = get(&app, "/api/v1/instance").await.json_value();
    let v2 = get(&app, "/api/v2/instance").await.json_value();
    for thumbnail in [&v1["thumbnail"], &v2["thumbnail"]["url"]] {
        let image = get(&app, &path_of(thumbnail)).await;
        assert_eq!(image.status, StatusCode::OK, "the thumbnail resolves");
        assert_eq!(
            image.header("content-type"),
            Some("image/jpeg"),
            "the thumbnail is a JPEG"
        );
    }
}
