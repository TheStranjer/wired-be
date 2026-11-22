//! The gates in front of the route: credentials, scopes, JSON answers and
//! the rate limit bucket.

use axum::http::StatusCode;

use super::support::{ROOT, about, dormant_user, file, filings, fixture, reporter};
use crate::common;

/// The route is user-token-only: no token, an unknown token, an app-only
/// credential and a deactivated owner are refused before anything else is
/// looked at, and none of them files anything.
#[tokio::test]
async fn the_credential_gates_refuse_the_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let bob = common::create_local_user(&pool, "bob").await;
    let app_only = common::create_app_only_token(&pool, "write write:reports").await;
    let dormant = dormant_user(&pool, "dormant").await;
    let dormant_token = common::create_app_and_token(&pool, &dormant, "write:reports").await;

    let missing = common::TestRequest::post(ROOT)
        .json(&about(&bob))
        .send(&app)
        .await;
    common::assert_answer(&missing, StatusCode::UNAUTHORIZED, "no token at all");
    common::assert_error(&missing, "Invalid credentials", "no token at all");

    let unknown = common::TestRequest::post(ROOT)
        .bearer("no-such-token")
        .json(&about(&bob))
        .send(&app)
        .await;
    common::assert_answer(
        &unknown,
        StatusCode::UNAUTHORIZED,
        "a token that does not resolve",
    );
    common::assert_error(
        &unknown,
        "The access token is invalid",
        "a token that does not resolve",
    );

    let credential = common::TestRequest::post(ROOT)
        .bearer(&app_only)
        .json(&about(&bob))
        .send(&app)
        .await;
    common::assert_answer(&credential, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(
        &credential,
        "This API requires an authenticated user",
        "an app-only token",
    );

    let disabled = common::TestRequest::post(ROOT)
        .bearer(&dormant_token)
        .json(&about(&bob))
        .send(&app)
        .await;
    common::assert_answer(&disabled, StatusCode::FORBIDDEN, "a deactivated owner");
    common::assert_error(
        &disabled,
        "Your account is currently disabled",
        "a deactivated owner",
    );

    assert!(filings(&pool).await.is_empty(), "and no refusal filed");
}

/// The scope half of the pipeline: `write:reports` and the broad `write` root
/// file, while any other grant is the 403 naming the missing one.
#[tokio::test]
async fn a_token_without_the_route_scope_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_narrow, narrow) = fixture(&pool, "narrow", "read:statuses write:statuses").await;
    let (_beside, beside) = fixture(&pool, "beside", "write:accounts").await;
    let (_root, root) = fixture(&pool, "root", "write").await;
    let (_follow, follow) = fixture(&pool, "follow", "follow").await;
    let bob = common::create_local_user(&pool, "bob").await;

    for token in [&narrow, &beside, &follow] {
        let response = file(&app, token, about(&bob)).await;
        common::assert_answer(
            &response,
            StatusCode::FORBIDDEN,
            "a token without the scope",
        );
        common::assert_error(
            &response,
            "Insufficient permissions: write:reports.",
            "a token without the scope",
        );
    }

    let response = file(&app, &root, about(&bob)).await;
    common::assert_answer(&response, StatusCode::OK, "the broad write root files");
    assert_eq!(
        filings(&pool).await.len(),
        1,
        "and only the granted token did"
    );
}

/// A report is answered 200 with `application/json`, and a refusal's body is
/// the one-key `{"error": …}` shape of decision 8.
#[tokio::test]
async fn both_answers_are_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;

    let refused = file(&app, &token, about(&alice)).await;
    common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, "a self-report");
    assert_eq!(
        refused.header("content-type"),
        Some("application/json"),
        "the refusal is JSON"
    );
    assert_eq!(
        refused
            .json_value()
            .as_object()
            .expect("an error object")
            .keys()
            .len(),
        1,
        "and carries nothing but its `error`"
    );

    let ok = file(&app, &token, about(&bob)).await;
    common::assert_answer(&ok, StatusCode::OK, "a filing");
    assert_eq!(
        ok.header("content-type"),
        Some("application/json"),
        "so is the success"
    );
}

/// The route rides the `statuses_actions` bucket: the third request of the
/// window is refused 429 `{"error":"Throttled"}` and never reaches the
/// handler, so nothing is filed for it.
#[tokio::test]
async fn the_route_spends_its_rate_limit_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("statuses_actions=2:600"),
    );
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;

    for spent in 0..2 {
        let response = file(&app, &token, about(&bob)).await;
        common::assert_answer(&response, StatusCode::OK, "a request inside the budget");
        assert_eq!(
            response.header("x-ratelimit-limit"),
            Some("2"),
            "the bucket's budget"
        );
        assert_eq!(
            response.header("x-ratelimit-remaining"),
            Some(if spent == 0 { "1" } else { "0" }),
            "the window counts down"
        );
    }

    let throttled = file(&app, &token, about(&bob)).await;
    common::assert_answer(&throttled, StatusCode::TOO_MANY_REQUESTS, "over the budget");
    common::assert_error(&throttled, "Throttled", "over the budget");
    assert_eq!(
        filings(&pool).await.len(),
        2,
        "the third request never reached the handler"
    );
}
