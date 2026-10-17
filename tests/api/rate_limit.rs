#![cfg(test)]

//! The rate limiter over HTTP (`src/rate_limit.rs`): the mounted routes
//! answer the `x-ratelimit-*` headers, share a bucket across their paths,
//! and refuse over the budget with 429 `{"error":"Throttled"}`.
//!
//! The suite's own config runs with `RATE_LIMITS_ENABLED=false` (see
//! `common::test_config`); these tests mount the limiter with
//! `common::test_config_with_rate_limits` and drive it through
//! `TestRequest::peer`, the harness' stand-in for the socket peer
//! `serve` wires as `ConnectInfo`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::http::StatusCode;
use serde_json::json;
use wired_be::config::Toggle;

use crate::common;

/// The socket peer number `n` of the documentation-test range: one
/// distinct client per call.
fn peer(n: u8) -> SocketAddr {
    SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, n)),
        40_000 + u16::from(n),
    )
}

/// `POST /oauth/token` with a body the endpoint refuses for its own
/// reasons: the limiter counts it, the answer is a plain 400.
fn token_request() -> common::TestRequest {
    common::TestRequest::post("/oauth/token").json(&json!({"grant_type": "bogus"}))
}

/// A mounted route answers the three `x-ratelimit-*` headers, remaining
/// decrements request by request, and the request over the budget is
/// refused 429 `{"error":"Throttled"}` with a `Retry-After` and the same
/// headers.
#[tokio::test]
async fn limited_route_counts_down_to_a_throttled_refusal() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let config = common::test_config_with_rate_limits("authentication=2:600");
    let app = common::test_app_with_config(pool, config);
    let first = token_request().peer(peer(1)).send(&app).await;
    common::assert_answer(&first, StatusCode::BAD_REQUEST, "token grant refused");
    assert_eq!(first.header("x-ratelimit-limit"), Some("2"), "the budget");
    assert_eq!(
        first.header("x-ratelimit-remaining"),
        Some("1"),
        "one request spent of two"
    );
    assert_eq!(
        first.header("x-ratelimit-reset"),
        Some("600"),
        "the window is untouched"
    );
    let second = token_request().peer(peer(1)).send(&app).await;
    assert_eq!(
        second.header("x-ratelimit-remaining"),
        Some("0"),
        "the budget is spent"
    );
    let third = token_request().peer(peer(1)).send(&app).await;
    common::assert_answer(&third, StatusCode::TOO_MANY_REQUESTS, "over the limit");
    common::assert_error(&third, "Throttled", "the refusal body");
    assert_eq!(third.header("retry-after"), Some("600"), "one window left");
    assert_eq!(third.header("x-ratelimit-limit"), Some("2"), "and headers");
    assert_eq!(
        third.header("x-ratelimit-remaining"),
        Some("0"),
        "none left"
    );
}

/// `/oauth/token` and `/oauth/authorize` share the `authentication`
/// bucket: the authorize page spends the token endpoint's window too.
#[tokio::test]
async fn the_two_oauth_paths_share_one_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let config = common::test_config_with_rate_limits("authentication=2:600");
    let app = common::test_app_with_config(pool, config);
    let consent = common::TestRequest::get("/oauth/authorize?response_type=token")
        .peer(peer(2))
        .send(&app)
        .await;
    common::assert_answer(&consent, StatusCode::BAD_REQUEST, "the flow check answers");
    assert_eq!(
        consent.header("x-ratelimit-remaining"),
        Some("1"),
        "the consent page is counted"
    );
    let token = token_request().peer(peer(2)).send(&app).await;
    assert_eq!(
        token.header("x-ratelimit-remaining"),
        Some("0"),
        "and shares the window with the token grant"
    );
    let refused = token_request().peer(peer(2)).send(&app).await;
    common::assert_answer(
        &refused,
        StatusCode::TOO_MANY_REQUESTS,
        "the shared window is spent",
    );
}

/// `POST /api/v1/apps` and `POST /api/v1/accounts` share the
/// `app_account_creation` bucket across their routers.
#[tokio::test]
async fn app_and_account_registration_share_their_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let config = common::test_config_with_rate_limits("app_account_creation=2:1800");
    let app = common::test_app_with_config(pool, config);
    let registered = common::TestRequest::post("/api/v1/apps")
        .json(&json!({
            "client_name": "Throttle Client",
            "redirect_uris": "https://client.example/callback",
            "scopes": "read",
        }))
        .peer(peer(3))
        .send(&app)
        .await;
    common::assert_answer(&registered, StatusCode::OK, "app registration");
    assert_eq!(
        registered.header("x-ratelimit-remaining"),
        Some("1"),
        "the registration is counted"
    );
    let attempted = common::TestRequest::post("/api/v1/accounts")
        .json(&json!({
            "username": "throttled",
            "email": "throttled@example.org",
            "password": "a-throttling-password",
            "agreement": true,
        }))
        .peer(peer(3))
        .send(&app)
        .await;
    assert_eq!(
        attempted.header("x-ratelimit-remaining"),
        Some("0"),
        "registration spends the app's window too"
    );
    let refused = common::TestRequest::post("/api/v1/apps")
        .json(&json!({
            "client_name": "Too Late",
            "redirect_uris": "https://client.example/callback",
        }))
        .peer(peer(3))
        .send(&app)
        .await;
    common::assert_answer(
        &refused,
        StatusCode::TOO_MANY_REQUESTS,
        "the shared budget is spent",
    );
}

/// A route no bucket is mounted on answers with no headers and no
/// refusals, and the suite's default config (`RATE_LIMITS_ENABLED=false`)
/// passes the mounted routes through unthrottled.
#[tokio::test]
async fn unmounted_routes_and_the_disabled_limiter_answer_plain() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let limited_app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("search=1:1"),
    );
    let health = common::TestRequest::get("/health")
        .peer(peer(4))
        .send(&limited_app)
        .await;
    common::assert_answer(&health, StatusCode::NO_CONTENT, "health");
    assert_eq!(
        health.header("x-ratelimit-limit"),
        None,
        "a route outside any bucket carries no headers"
    );
    let default_app = common::test_app(pool);
    for _ in 0..3 {
        let response = token_request().peer(peer(4)).send(&default_app).await;
        common::assert_answer(
            &response,
            StatusCode::BAD_REQUEST,
            "the grant still answers",
        );
        assert_eq!(
            response.header("x-ratelimit-limit"),
            None,
            "and the suite's default config never throttles"
        );
    }
}

/// A distinct socket peer is a distinct window; while
/// `TRUST_PROXY_HEADERS` is off — the default — an `X-Forwarded-For`
/// header names nobody: the socket's window is shared however the header
/// lies.
#[tokio::test]
async fn distinct_peers_are_distinct_windows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let config = common::test_config_with_rate_limits("authentication=1:600");
    let app = common::test_app_with_config(pool, config);
    let first = token_request().peer(peer(5)).send(&app).await;
    common::assert_answer(&first, StatusCode::BAD_REQUEST, "the first client passes");
    let second = token_request().peer(peer(6)).send(&app).await;
    common::assert_answer(&second, StatusCode::BAD_REQUEST, "the second client passes");
    let repeated = token_request().peer(peer(5)).send(&app).await;
    common::assert_answer(
        &repeated,
        StatusCode::TOO_MANY_REQUESTS,
        "the first client is spent",
    );
}

/// `TRUST_PROXY_HEADERS=true` makes the leftmost `X-Forwarded-For` entry
/// the subject: two forwarded clients behind one proxy keep two windows.
/// While the flag is off — the default — the same pair shares the proxy's
/// socket window however the header claims otherwise.
#[tokio::test]
async fn forwarded_for_names_the_client_only_when_trusted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let plain_app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("authentication=1:600"),
    );
    let one = token_request()
        .peer(peer(7))
        .header("x-forwarded-for", "198.51.100.1")
        .send(&plain_app)
        .await;
    common::assert_answer(&one, StatusCode::BAD_REQUEST, "the first hop passes");
    let two = token_request()
        .peer(peer(7))
        .header("x-forwarded-for", "198.51.100.2")
        .send(&plain_app)
        .await;
    common::assert_answer(
        &two,
        StatusCode::TOO_MANY_REQUESTS,
        "untrusted, the header names nobody: one socket, one window",
    );

    let mut config = common::test_config_with_rate_limits("authentication=1:600");
    config.rate_limit.trust_proxy_headers = Toggle::new(true);
    let trusted_app = common::test_app_with_config(pool, config);
    let alice = token_request()
        .peer(peer(7))
        .header("x-forwarded-for", "198.51.100.1, 70.41.3.18")
        .send(&trusted_app)
        .await;
    common::assert_answer(&alice, StatusCode::BAD_REQUEST, "alice passes");
    let bob = token_request()
        .peer(peer(7))
        .header("x-forwarded-for", "198.51.100.2, 70.41.3.18")
        .send(&trusted_app)
        .await;
    common::assert_answer(
        &bob,
        StatusCode::BAD_REQUEST,
        "bob keeps his own window through the same proxy",
    );
    let alice_again = token_request()
        .peer(peer(7))
        .header("x-forwarded-for", "198.51.100.1")
        .send(&trusted_app)
        .await;
    common::assert_answer(
        &alice_again,
        StatusCode::TOO_MANY_REQUESTS,
        "alice's window is spent while bob's survives",
    );
}

/// A valid user bearer token keys the window on the user, not the
/// socket: the account is spent through another IP, while another account
/// and an anonymous request keep their own budgets.
#[tokio::test]
async fn a_bearer_token_keys_the_window_on_its_user() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let owner = common::create_local_user(&pool, "ratelimited-owner").await;
    let token = common::create_app_and_token(&pool, &owner, "read").await;
    let config = common::test_config_with_rate_limits("authentication=1:600");
    let app = common::test_app_with_config(pool.clone(), config);
    let first = token_request()
        .bearer(&token)
        .peer(peer(8))
        .send(&app)
        .await;
    common::assert_answer(
        &first,
        StatusCode::BAD_REQUEST,
        "the user's first request passes",
    );
    let elsewhere = token_request()
        .bearer(&token)
        .peer(peer(9))
        .send(&app)
        .await;
    common::assert_answer(
        &elsewhere,
        StatusCode::TOO_MANY_REQUESTS,
        "the same user is spent from another socket",
    );
    let anonymous = token_request().peer(peer(8)).send(&app).await;
    common::assert_answer(
        &anonymous,
        StatusCode::BAD_REQUEST,
        "the user's window does not bind the socket's own requests",
    );
}
