//! The translate route's refusals: provider failures, invisible statuses,
//! the token gate and the body checks.

use super::*;

/// A provider that refuses, cannot be reached, or answers unreadably is the
/// 503 the ticket pins, with the real reason kept out of the body.
#[tokio::test]
async fn a_failing_provider_is_a_service_unavailable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let token = reader(&pool, "bob", SCOPES).await;

    let refused_app = {
        let mock = Arc::new(Mock {
            status: StatusCode::TOO_MANY_REQUESTS,
            ..Mock::libretranslate("<p>Hallo</p>")
        });
        let (url, _server) = spawn(mock).await;
        common::test_translation_app(pool.clone(), "libretranslate", &url)
    };
    let unreadable_app = {
        let mock = Arc::new(Mock {
            libre_answer: "<h1>not json</h1>".to_owned(),
            ..Mock::libretranslate("<p>Hallo</p>")
        });
        let (url, _server) = spawn(mock).await;
        common::test_translation_app(pool.clone(), "libretranslate", &url)
    };
    // Port 9 (discard) answers nothing at all.
    let dead_app =
        common::test_translation_app(pool, "libretranslate", "http://127.0.0.1:9/nowhere");

    let refused = translate(
        &refused_app,
        &token,
        &status.id.to_string(),
        json!({"lang": "de"}),
    )
    .await;
    let unreadable = translate(
        &unreadable_app,
        &token,
        &status.id.to_string(),
        json!({"lang": "de"}),
    )
    .await;
    let unreachable = translate(
        &dead_app,
        &token,
        &status.id.to_string(),
        json!({"lang": "de"}),
    )
    .await;

    for response in [&refused, &unreadable, &unreachable] {
        assert_eq!(
            response.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "the provider did not deliver: {}",
            response.text()
        );
        common::assert_error(response, UNAVAILABLE, "a failing provider");
    }
    assert!(
        !refused.text().contains("429") && !unreachable.text().contains("127.0.0.1"),
        "the reason stays server-side: {} / {}",
        refused.text(),
        unreachable.text()
    );
}

/// With `TRANSLATION_PROVIDER` at its `none` default the route answers the 503
/// the ticket pins, and the status gate still comes first.
#[tokio::test]
async fn no_provider_is_a_service_unavailable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let secret = post_status(
        &pool,
        &author,
        &Fixture::new("Secret").seen_by(Visibility::Private),
    )
    .await;
    let token = reader(&pool, "bob", SCOPES).await;
    let app = common::test_app(pool);

    let refused = translate(&app, &token, &status.id.to_string(), json!({"lang": "de"})).await;
    let invisible = translate(&app, &token, &secret.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(
        refused.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "503 status"
    );
    common::assert_error(&refused, NOT_ENABLED, "no provider configured");
    assert_eq!(
        refused.header("content-type"),
        Some("application/json"),
        "the refusal is JSON like every error"
    );
    assert_eq!(
        invisible.status,
        StatusCode::NOT_FOUND,
        "an unreadable post is still the shared 404"
    );
    common::assert_error(&invisible, NOT_FOUND, "someone else's private post");
}

/// A status this caller may not see — a private or direct post of someone
/// else's, a tombstone, an unknown id, a malformed id — is the shared 404, and
/// none of them wakes the provider.
#[tokio::test]
async fn an_invisible_status_is_not_found_without_calling_the_provider() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let secret = post_status(
        &pool,
        &author,
        &Fixture::new("Secret").seen_by(Visibility::Private),
    )
    .await;
    let direct = post_status(
        &pool,
        &author,
        &Fixture::new("Just us").seen_by(Visibility::Direct),
    )
    .await;
    let deleted = post_status(&pool, &author, &Fixture::new("Gone")).await;
    let token = reader(&pool, "bob", SCOPES).await;
    let _tombstone = status::soft_delete(&pool, deleted.id)
        .await
        .expect("the tombstone write runs");
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let probes = [
        translate(&app, &token, &secret.id.to_string(), json!({"lang": "de"})).await,
        translate(&app, &token, &direct.id.to_string(), json!({"lang": "de"})).await,
        translate(&app, &token, &deleted.id.to_string(), json!({"lang": "de"})).await,
        translate(&app, &token, UNKNOWN_ID, json!({"lang": "de"})).await,
        translate(&app, &token, "not-an-id", json!({"lang": "de"})).await,
    ];

    common::assert_all_refused(
        &probes,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "an unreadable id",
    );
    assert!(
        mock.requests().await.is_empty(),
        "nothing unreadable is ever sent to a provider"
    );
}

/// The route needs a user token with `read:statuses`: no credentials are the
/// 401, a token without the scope the 403, an app-only token the 403 that says
/// a user is required — and none of them reaches the provider.
#[tokio::test]
async fn the_route_needs_a_reader_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let wrong_scope = reader(&pool, "mallory", "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, SCOPES).await;
    let path = format!("/api/v1/statuses/{}/translate", status.id);
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let anonymous = common::TestRequest::post(&path)
        .json(&json!({"lang": "de"}))
        .send(&app)
        .await;
    let refused = translate(
        &app,
        &wrong_scope,
        &status.id.to_string(),
        json!({"lang": "de"}),
    )
    .await;
    let unbound = translate(
        &app,
        &app_only,
        &status.id.to_string(),
        json!({"lang": "de"}),
    )
    .await;

    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, NO_TOKEN, "an anonymous caller");
    assert_eq!(
        anonymous.header("www-authenticate"),
        Some("Bearer"),
        "a 401 challenges for the scheme"
    );
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "the wrong scope");
    common::assert_error(&refused, NO_SCOPE, "a token without read:statuses");
    assert_eq!(
        unbound.status,
        StatusCode::FORBIDDEN,
        "an app-only token is no user"
    );
    assert!(
        mock.requests().await.is_empty(),
        "a refused request never reaches the provider"
    );
}

/// A `lang` of the wrong type is the body extractor's 422, before the provider
/// is ever asked.
#[tokio::test]
async fn a_non_string_lang_is_unprocessable() {
    let db = common::test_db().await;
    let (app, mock, status, token) = libre_scenario(&db, "<p>Hallo</p>").await;

    let response = translate(&app, &token, &status.id.to_string(), json!({"lang": 7})).await;

    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        response.text()
    );
    assert!(
        response.json_value()["error"]
            .as_str()
            .is_some_and(|message| message.starts_with("Invalid parameters:")),
        "the extractor names its own failure: {}",
        response.text()
    );
    assert!(mock.requests().await.is_empty(), "and nothing was asked");
}

/// The translated content round-trips as form parameters too (decision 10):
/// `lang=de` in an urlencoded body is the same request.
#[tokio::test]
async fn the_body_may_be_a_form() {
    let db = common::test_db().await;
    let (app, _mock, status, token) = libre_scenario(&db, "<p>Hallo</p>").await;

    let response = common::TestRequest::post(&format!("/api/v1/statuses/{}/translate", status.id))
        .bearer(&token)
        .form(&[("lang", "de")])
        .send(&app)
        .await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value()["content"],
        "<p>Hallo</p>",
        "the form's `lang` won"
    );
}
