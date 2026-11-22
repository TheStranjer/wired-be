//! Who a limited request is counted against: the user behind a valid
//! bearer token, else the client address.

use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, Request},
    http::HeaderName,
};
use tracing::warn;

use crate::state::AppState;

/// The `X-Forwarded-For` request header, read only when
/// `TRUST_PROXY_HEADERS` is on.
const FORWARDED_FOR_HEADER: HeaderName = HeaderName::from_static("x-forwarded-for");
/// The subject of a limited request that names neither a user nor a client
/// IP: an in-process test request carrying no `ConnectInfo`.
const UNKNOWN_SUBJECT: &str = "unknown";

/// What a limited request is counted against: `user:<id>` when it presents
/// a valid user-bearing bearer token (Pleroma's `:oauth_token` subject,
/// resolved to the account here), `ip:<client>` for everything else. An
/// unresolvable token — app-only, expired, unknown — and a failed lookup
/// both count by client, never against the token's owner-to-be.
pub(super) async fn authenticated_subject(
    app: &AppState,
    token: Option<&str>,
    client_key: String,
) -> String {
    match token {
        Some(raw) => match crate::auth::credential(app, raw).await {
            Ok(Some(crate::auth::AuthContext {
                user: Some(user), ..
            })) => format!("user:{}", user.id),
            Ok(_) => client_key,
            Err(err) => {
                warn!(%err, "rate limit token lookup failed; counting the request by client");
                client_key
            }
        },
        None => client_key,
    }
}

/// The client an anonymous request is counted against: the leftmost
/// `X-Forwarded-For` entry while `TRUST_PROXY_HEADERS` is on, else (or
/// when the header says nothing usable) the socket peer; [`UNKNOWN_SUBJECT`]
/// when even that is absent.
pub(super) fn client(app: &AppState, request: &Request) -> String {
    if app.config.rate_limit.trust_proxy_headers.get()
        && let Some(forwarded) = forwarded_for(request)
    {
        return forwarded;
    }
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or_else(
            || UNKNOWN_SUBJECT.to_owned(),
            |ConnectInfo(addr)| addr.to_string(),
        )
}

/// The leftmost, trimmed, non-empty `X-Forwarded-For` entry — the first hop
/// the trusted proxy appended; `None` without one.
fn forwarded_for(request: &Request) -> Option<String> {
    let value = request
        .headers()
        .get(&FORWARDED_FOR_HEADER)?
        .to_str()
        .ok()?;
    let first = value.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{body::Body, http::StatusCode};
    use tower::ServiceExt as _;

    use super::super::{
        clock::ManualClock,
        test_support::{
            FLAGS_ON, Flags, app_on_pool, auth_app, auth_limits, forwarded_post, limited_post,
            limited_request, login,
        },
    };
    use super::*;

    /// Distinct socket peers keep distinct windows, and the same peer is
    /// spent even on the sibling route of the same bucket.
    #[tokio::test]
    async fn separate_keys_for_separate_clients() {
        let clock = Arc::new(ManualClock::new());
        let app = auth_app(&clock, 1, FLAGS_ON);
        let first = app
            .clone()
            .oneshot(limited_post("203.0.113.1:1"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK, "first client passes");
        let twin = app
            .clone()
            .oneshot(limited_post("203.0.113.2:2"))
            .await
            .unwrap();
        assert_eq!(
            twin.status(),
            StatusCode::OK,
            "a second client has its own window"
        );
        let sibling = app
            .clone()
            .oneshot(
                Request::post("/also-limited")
                    .extension(ConnectInfo("203.0.113.1:1".parse::<SocketAddr>().unwrap()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            sibling.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the first client is spent even on the sibling route of the same bucket"
        );
    }

    /// `X-Forwarded-For` names the client only while
    /// `TRUST_PROXY_HEADERS` is on; while off, every request from one
    /// socket shares one window whatever the header claims.
    #[tokio::test]
    async fn forwarded_for_is_honored_only_when_trusted() {
        let trusted_clock = Arc::new(ManualClock::new());
        let trusted_app = auth_app(
            &trusted_clock,
            1,
            Flags {
                enabled: true,
                trust: true,
            },
        );
        let alice = trusted_app
            .clone()
            .oneshot(forwarded_post("10.0.0.1:1", "203.0.113.1, 70.41.3.18"))
            .await
            .unwrap();
        assert_eq!(alice.status(), StatusCode::OK, "the first hop passes");
        let bob = trusted_app
            .oneshot(forwarded_post("10.0.0.1:1", "203.0.113.2, 70.41.3.18"))
            .await
            .unwrap();
        assert_eq!(
            bob.status(),
            StatusCode::OK,
            "another forwarded client behind the same proxy keeps its own window"
        );

        let plain_clock = Arc::new(ManualClock::new());
        let plain_app = auth_app(&plain_clock, 1, FLAGS_ON);
        let one = plain_app
            .clone()
            .oneshot(forwarded_post("10.0.0.1:1", "203.0.113.1"))
            .await
            .unwrap();
        assert_eq!(one.status(), StatusCode::OK, "the first passes");
        let two = plain_app
            .oneshot(forwarded_post("10.0.0.1:1", "203.0.113.2"))
            .await
            .unwrap();
        assert_eq!(
            two.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "while untrusted the header is noise: one socket, one window"
        );
    }

    /// A valid user bearer token keys the window on the user id, not the
    /// socket: the user is spent through another IP, while another user and
    /// an anonymous request keep their own budgets.
    #[tokio::test]
    async fn bearer_token_keys_on_the_user_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let clock = Arc::new(ManualClock::new());
        let app = app_on_pool(&auth_limits(2), &clock, FLAGS_ON, pool.clone());
        let first = login(&pool, "ratelimited-first").await;
        let second = login(&pool, "ratelimited-second").await;
        let opener = app
            .clone()
            .oneshot(limited_request(&first, Some("203.0.113.1:1")))
            .await
            .unwrap();
        assert_eq!(
            opener.status(),
            StatusCode::OK,
            "the user's first request passes"
        );
        let elsewhere = app
            .clone()
            .oneshot(limited_request(&first, Some("203.0.113.2:2")))
            .await
            .unwrap();
        assert_eq!(
            elsewhere.status(),
            StatusCode::OK,
            "one user's two sockets are one window"
        );
        let refused = app
            .clone()
            .oneshot(limited_request(&first, Some("203.0.113.1:1")))
            .await
            .unwrap();
        assert_eq!(
            refused.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the user's window is spent regardless of the socket"
        );
        let twin = app
            .clone()
            .oneshot(limited_request(&second, Some("203.0.113.1:1")))
            .await
            .unwrap();
        assert_eq!(
            twin.status(),
            StatusCode::OK,
            "another user brings their own window"
        );
        let anonymous = app.oneshot(limited_request("", None)).await.unwrap();
        assert_eq!(
            anonymous.status(),
            StatusCode::OK,
            "an anonymous request counts by IP"
        );
    }

    /// A bearer token that resolves to nothing (no such row, or a database
    /// the limiter cannot reach) never blocks: the request counts by client
    /// and the route keeps answering.
    #[tokio::test]
    async fn unresolvable_token_falls_back_to_the_client() {
        let clock = Arc::new(ManualClock::new());
        let app = auth_app(&clock, 1, FLAGS_ON);
        let pass = app
            .clone()
            .oneshot(limited_request("no-such-token", Some("203.0.113.1:1")))
            .await
            .unwrap();
        assert_eq!(
            pass.status(),
            StatusCode::OK,
            "the lazy pool's failed lookup is not fatal"
        );
        let reject = app
            .oneshot(limited_request("no-such-token", Some("203.0.113.1:1")))
            .await
            .unwrap();
        assert_eq!(
            reject.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the same client is spent by IP"
        );
    }
}
