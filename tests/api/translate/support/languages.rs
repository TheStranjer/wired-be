//! The language matrix endpoints and the `v2` instance document.

use super::*;

/// `GET /api/v1/instance/translation_languages` answers the provider's matrix —
/// and answers it from the cache, so the second request costs no provider call.
#[tokio::test]
async fn the_language_matrix_is_served_and_cached() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let first = languages(&app).await;
    let second = languages(&app).await;

    assert_eq!(first.status, StatusCode::OK, "the document is served");
    assert_eq!(
        first.json_value(),
        json!({"de": ["en"], "en": ["de"]}),
        "every source with every other language as its target"
    );
    assert_eq!(
        second.json_value(),
        first.json_value(),
        "the cached document is the same one"
    );
    let seen = mock.requests().await;
    assert_eq!(
        seen.iter()
            .filter(|entry| entry.path == "/languages")
            .count(),
        1,
        "one fetch per cache window, not one per request"
    );
    assert_eq!(seen[0].method, "GET", "the list is a `GET` call");
}

/// `DeepL` builds its matrix from the two lists its API documents, and
/// authenticates those calls exactly as it authenticates a translation.
#[tokio::test]
async fn deepl_lists_its_languages_from_both_queries() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::deepl("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let app = common::test_translation_app(pool, "deepl", &url);

    let response = languages(&app).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value(),
        json!({
            "en": ["pt", "pt-br"],
            "pt": ["en", "pt-br"],
            "pt-br": ["en", "pt"],
        }),
        "a regional spelling is listed beside its base tag"
    );
    let seen = mock.requests().await;
    assert_eq!(
        seen.iter()
            .map(|entry| entry.target.as_str())
            .collect::<Vec<&str>>(),
        vec!["/v2/languages?type=source", "/v2/languages?type=target"],
        "two queries, sources first"
    );
    assert!(
        seen.iter()
            .all(|entry| entry.authorization == format!("DeepL-Auth-Key {TEST_KEY}")),
        "the list is as authenticated as the translation"
    );
}

/// The language document is the empty object when the configured provider
/// refuses its list, and a refusal is not cached: the provider is asked again
/// rather than buying an hour of silence. An instance with no provider at all
/// answers the same `{}` — `instance::translation_languages_is_the_empty_object`
/// is that test.
#[tokio::test]
async fn a_provider_without_a_list_answers_no_pairs() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        ..Mock::libretranslate("<p>Hallo</p>")
    });
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let refused = languages(&app).await;
    let again = languages(&app).await;

    assert_eq!(
        refused.status,
        StatusCode::OK,
        "the endpoint answers even a failing provider"
    );
    assert_eq!(
        refused.json_value(),
        json!({}),
        "a provider that refuses its list has no language pairs: {}",
        refused.text()
    );
    assert_eq!(again.json_value(), json!({}), "and the second answer too");
    assert_eq!(
        mock.requests().await.len(),
        2,
        "a refusal is not cached, so the provider gets asked again"
    );
}

/// `GET /api/v2/instance` reports `configuration.translation.enabled` from the
/// same settings the endpoints act on: true for a usable provider, false while
/// none is configured and false for one missing what it needs.
#[tokio::test]
async fn the_v2_document_reports_whether_translation_is_enabled() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let configured = common::test_app_with_config(
        pool.clone(),
        common::test_translation_config("deepl", Some("http://127.0.0.1:9")),
    );
    let urlless = common::test_app_with_config(
        pool.clone(),
        common::test_translation_config("libretranslate", None),
    );
    let unconfigured = common::test_app(pool);

    for (app, enabled) in [
        (&configured, true),
        (&urlless, false),
        (&unconfigured, false),
    ] {
        let response = common::TestRequest::get("/api/v2/instance").send(app).await;
        assert_eq!(response.status, StatusCode::OK, "the document is served");
        assert_eq!(
            response.json_value()["configuration"]["translation"],
            json!({ "enabled": enabled }),
            "the translation group of a provider that is usable: {}",
            response.text()
        );
    }
}
