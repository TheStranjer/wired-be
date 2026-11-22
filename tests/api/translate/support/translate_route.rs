//! `POST /api/v1/statuses/{id}/translate`: the providers, the target
//! language, sanitizing, reblogs and the route's keying.

use super::*;

/// `POST …/translate` on a `LibreTranslate` instance: the provider is sent the
/// request Pleroma's provider sends and its answer comes back as the entity,
/// with the status's own language reported as the detected one.
#[tokio::test]
async fn libretranslate_translates_a_status() {
    let db = common::test_db().await;
    let (app, mock, status, token) = libre_scenario(&db, "<p>Hallo</p>").await;

    let response = translate(&app, &token, &status.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value(),
        json!({
            "content": "<p>Hallo</p>",
            "spoiler_text": "",
            "poll": null,
            "media_attachments": [],
            "detected_source_language": "en",
            "provider": "LibreTranslate",
        }),
        "the ticket's document, with exactly the ticket's keys"
    );
    let seen = mock.requests().await;
    assert_eq!(seen.len(), 1, "one call per translation");
    assert_eq!(seen[0].method, "POST", "the translate call is a POST");
    assert_eq!(seen[0].path, "/translate", "at the documented path");
    assert_eq!(
        sent(&seen[0]),
        json!({
            "q": status.content,
            "source": "en",
            "target": "de",
            "format": "html",
            "api_key": "",
        }),
        "the status's own HTML content, with a keyless server sent an empty key"
    );
}

/// `POST …/translate` on a `DeepL` instance: `DeepL`'s own request shape — the
/// text in an array, `tag_handling=html`, the key in `Authorization` — and the
/// language `DeepL` detected is what the answer reports.
#[tokio::test]
async fn deepl_translates_a_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::deepl("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let token = reader(&pool, "bob", SCOPES).await;
    let app = common::test_translation_app(pool, "deepl", &url);

    let response = translate(&app, &token, &status.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value(),
        json!({
            "content": "<p>Hallo</p>",
            "spoiler_text": "",
            "poll": null,
            "media_attachments": [],
            "detected_source_language": "EN",
            "provider": "DeepL",
        }),
        "the provider's own name and detection"
    );
    let seen = mock.requests().await;
    assert_eq!(seen[0].path, "/v2/translate", "the v2 endpoint");
    assert_eq!(
        seen[0].authorization,
        format!("DeepL-Auth-Key {TEST_KEY}"),
        "the configured key authenticates the call"
    );
    assert_eq!(
        sent(&seen[0]),
        json!({
            "text": [status.content],
            "source_lang": "en",
            "target_lang": "de",
            "tag_handling": "html",
        }),
        "the request Pleroma's DeepL provider sends"
    );
}

/// A status with no language of its own is sent to the provider without one:
/// `auto` for `LibreTranslate`, and no `source_lang` at all for `DeepL`.
#[tokio::test]
async fn a_status_that_names_no_language_is_left_to_the_provider() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello").untranslated()).await;
    let token = reader(&pool, "bob", SCOPES).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let response = translate(&app, &token, &status.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        sent(&mock.requests().await[0])["source"],
        "auto",
        "the provider is asked to detect it"
    );
    assert_eq!(
        response.json_value()["detected_source_language"],
        Value::Null,
        "and a server that says nothing back detects nothing we can report"
    );
}

/// With no `lang` in the body the endpoint translates into the caller's own
/// default language, and into English when their account never named one.
#[tokio::test]
async fn the_target_defaults_to_the_callers_language_then_english() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Saluton</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let esperanto = reader_with_language(&pool, "carol", "eo").await;
    let unspoken = reader(&pool, "dave", SCOPES).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let first = translate(&app, &esperanto, &status.id.to_string(), json!({})).await;
    let second = translate(
        &app,
        &unspoken,
        &status.id.to_string(),
        json!({"lang": " fr "}),
    )
    .await;
    let third = translate(&app, &unspoken, &status.id.to_string(), json!({})).await;

    for response in [&first, &second, &third] {
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a body without a usable `lang` still translates: {}",
            response.text()
        );
    }
    let targets: Vec<String> = mock
        .requests()
        .await
        .iter()
        .map(|entry| {
            sent(entry)["target"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        targets,
        vec!["eo".to_owned(), "fr".to_owned(), "en".to_owned()],
        "the account default, then the padded spelling trimmed, then English"
    );
}

/// Whatever markup the provider answers with is sanitized before it is served:
/// an injected script and a `javascript:` link do not survive the trip.
#[tokio::test]
async fn the_providers_html_is_sanitized() {
    let db = common::test_db().await;
    let (app, _mock, status, token) = libre_scenario(
        &db,
        "<p>Hallo</p><script>alert(1)</script><a href=\"javascript:x\">link</a><em>Welt</em>",
    )
    .await;

    let response = translate(&app, &token, &status.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    let content = response.json_value()["content"]
        .as_str()
        .expect("content is a string")
        .to_owned();
    assert!(
        content.starts_with("<p>Hallo</p>") && content.contains("<em>Welt</em>"),
        "the markup this instance serves anyway survives: {content}"
    );
    assert!(
        !content.contains("script") && !content.contains("javascript:"),
        "the script element is gone with its contents and the bad link \
         scheme keeps no target: {content}"
    );
}

/// The content warning is the status's own, untranslated: the endpoint sends
/// the provider only the status content.
#[tokio::test]
async fn the_spoiler_rides_along_as_written() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(
        &pool,
        &author,
        &Fixture::new("Hello").warning("spoilers ahead"),
    )
    .await;
    let token = reader(&pool, "bob", SCOPES).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let response = translate(&app, &token, &status.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value()["spoiler_text"],
        "spoilers ahead",
        "the warning is served as its author wrote it"
    );
    let seen = mock.requests().await;
    assert_eq!(seen.len(), 1, "one call, not one per translatable field");
    assert_eq!(
        sent(&seen[0])["q"],
        status.content,
        "and it carries only the status content"
    );
}

/// A reblog row is translated as itself — the row the client named, whose own
/// content is empty — exactly as the other status routes answer it.
#[tokio::test]
async fn a_reblog_is_translated_as_its_own_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let original = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let booster = common::create_local_user(&pool, "carol").await;
    let boost = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            user_id: booster.id,
            reblog_of_id: Some(original.id),
            language: Some("en".to_owned()),
            local: true,
            ..NewStatus::default()
        },
    )
    .await
    .expect("reblog fixture inserts");
    let token = reader(&pool, "bob", SCOPES).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let response = translate(&app, &token, &boost.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value()["content"],
        "<p>Hallo</p>",
        "the answer is what the provider said"
    );
    assert_eq!(
        sent(&mock.requests().await[0])["q"],
        boost.content,
        "and the provider was sent the reblog row's own (empty) content"
    );
}

/// The route is mounted at the status id and nowhere else: the trailing-slash
/// spelling is the app-wide fallback, not a second mount.
#[tokio::test]
async fn the_route_is_keyed_on_the_status_id() {
    let db = common::test_db().await;
    let (app, _mock, status, token) = libre_scenario(&db, "<p>Hallo</p>").await;

    let slashed = common::TestRequest::post(&format!("/api/v1/statuses/{}/translate/", status.id))
        .bearer(&token)
        .json(&json!({"lang": "de"}))
        .send(&app)
        .await;

    assert_eq!(
        slashed.status,
        StatusCode::NOT_FOUND,
        "the slash spelling is not a second mount"
    );
    assert_eq!(
        slashed.json_value(),
        json!({"error": "Not implemented"}),
        "Pleroma's api_not_implemented fallback"
    );
}
