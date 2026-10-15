//! Integration tests for `GET /api/v1/custom_emojis`: the endpoint needs
//! no authentication, returns only the local, enabled emojis ordered by
//! category then shortcode, renders the `CustomEmoji` entity with its
//! exact JSON keys (Pleroma's `tags` included), and answers `[]` when
//! nothing is stored.

#![cfg(test)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::custom_emoji::{self, NewCustomEmoji};

use crate::common;

/// The instance origin fixture image URLs point at.
const BASE: &str = "https://test.example";

/// Every JSON key one listed emoji must carry, sorted.
const EXPECTED_KEYS: [&str; 6] = [
    "category",
    "shortcode",
    "static_url",
    "tags",
    "url",
    "visible_in_picker",
];

/// Upserts a local emoji named `shortcode` in pack `category`
/// (`None` = ungrouped), enabled, with its still url stored.
async fn seed_local(pool: &PgPool, shortcode: &str, category: Option<&str>) {
    let _stored: custom_emoji::CustomEmoji = custom_emoji::upsert(
        pool,
        &NewCustomEmoji {
            category: category.map(str::to_owned),
            static_url: Some(format!("{BASE}/emoji/custom/{shortcode}.png")),
            tags: vec![shortcode.to_owned(), "test".to_owned()],
            ..NewCustomEmoji::local(shortcode, &format!("{BASE}/emoji/custom/{shortcode}.gif"))
        },
    )
    .await
    .expect("fixture emoji upserts");
}

/// `GET /api/v1/custom_emojis` without any credentials.
async fn get_emojis(app: &axum::Router) -> common::TestResponse {
    common::TestRequest::get("/api/v1/custom_emojis")
        .send(app)
        .await
}

/// The `shortcode` of every element of `body`, in order.
fn shortcodes(body: &Value) -> Vec<&str> {
    body.as_array()
        .expect("the endpoint answers with a JSON array")
        .iter()
        .map(|emoji| {
            emoji["shortcode"]
                .as_str()
                .expect("every emoji carries its shortcode")
        })
        .collect()
}

/// An empty table answers 200 with the empty list, JSON content type and
/// no auth required.
#[tokio::test]
async fn empty_database_returns_an_empty_json_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = get_emojis(&app).await;
    assert_eq!(response.status, StatusCode::OK, "200 without auth");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the list is JSON"
    );
    assert_eq!(
        response.text(),
        "[]",
        "an empty table serves the empty list verbatim"
    );
}

/// Local enabled emojis come back ordered by category then shortcode
/// (ungrouped last), with exact keys and values; disabled and remote
/// rows are absent, and a bearer token changes nothing.
#[tokio::test]
async fn lists_local_enabled_emojis_ordered_by_category_then_shortcode() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    seed_local(&pool, "uncategorised", None).await;
    seed_local(&pool, "zebra", Some("animals")).await;
    seed_local(&pool, "aardvark", Some("animals")).await;
    // A disabled local emoji and a remote emoji share both names with
    // listed rows: neither may show up.
    let _hidden: custom_emoji::CustomEmoji = custom_emoji::upsert(
        &pool,
        &NewCustomEmoji {
            disabled: true,
            ..NewCustomEmoji::local("hidden", &format!("{BASE}/emoji/custom/hidden.gif"))
        },
    )
    .await
    .expect("disabled fixture emoji upserts");
    let _remote: custom_emoji::CustomEmoji = custom_emoji::upsert(
        &pool,
        &NewCustomEmoji {
            domain: Some("remote.example".to_owned()),
            ..NewCustomEmoji::local("aardvark", "https://remote.example/emoji/aardvark.gif")
        },
    )
    .await
    .expect("remote fixture emoji upserts");

    let app = common::test_app(pool);
    let response = common::TestRequest::get("/api/v1/custom_emojis")
        .bearer("not-a-real-token")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "auth is not required");
    let body = response.json_value();
    assert_eq!(
        shortcodes(&body),
        ["aardvark", "zebra", "uncategorised"],
        "category order, then shortcode, ungrouped last; disabled and remote are out"
    );

    let first = &body[0];
    assert_eq!(
        first,
        &json!({
            "shortcode": "aardvark",
            "url": "https://test.example/emoji/custom/aardvark.gif",
            "static_url": "https://test.example/emoji/custom/aardvark.png",
            "visible_in_picker": true,
            "category": "animals",
            "tags": ["aardvark", "test"],
        }),
        "the first listed emoji in full"
    );
    for emoji in body.as_array().expect("a JSON array of emojis") {
        let mut keys: Vec<&str> = emoji
            .as_object()
            .expect("each element is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, EXPECTED_KEYS, "exact key set per emoji");
    }
    assert!(
        body[2]["static_url"].is_string(),
        "the ungrouped emoji's still url is served"
    );
}

/// `visible_in_picker` is data the client honours, not a filter: an
/// invisible-but-enabled emoji is listed with the flag false, and a row
/// with no still url or pack serves those keys as `null`.
#[tokio::test]
async fn picker_visibility_and_absent_fields_pass_through() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _secret: custom_emoji::CustomEmoji = custom_emoji::upsert(
        &pool,
        &NewCustomEmoji {
            visible_in_picker: false,
            ..NewCustomEmoji::local("secret", &format!("{BASE}/emoji/custom/secret.png"))
        },
    )
    .await
    .expect("invisible fixture emoji upserts");

    let app = common::test_app(pool);
    let response = get_emojis(&app).await;
    assert_eq!(response.status, StatusCode::OK, "200 on success");
    let body = response.json_value();
    assert_eq!(
        body,
        json!([{
            "shortcode": "secret",
            "url": "https://test.example/emoji/custom/secret.png",
            "static_url": null,
            "visible_in_picker": false,
            "category": null,
            "tags": [],
        }]),
        "the invisible emoji is listed, flags and absent fields included"
    );
}

/// `POST` to the collection is not routed: the endpoint is a read-only
/// list (Pleroma's controller exposes `index` only).
#[tokio::test]
async fn other_methods_are_not_routed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::post("/api/v1/custom_emojis")
        .json(&json!({"shortcode": "cat"}))
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "a known path with the wrong verb keeps axum's 405"
    );
}
