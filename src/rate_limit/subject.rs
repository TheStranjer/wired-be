//! Who a limited request is counted against: the user behind a valid
//! bearer token, else the client address.

use std::net::{IpAddr, SocketAddr};

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

/// The client an anonymous request is counted against: while
/// `TRUST_PROXY_HEADERS` is on, the `X-Forwarded-For` entry
/// `TRUSTED_PROXY_HOPS` places from the right (the address the outermost
/// trusted proxy saw, never the client-controlled left), else (or when the
/// header says nothing usable: absent, too short, not an IP) the socket
/// peer's address; [`UNKNOWN_SUBJECT`] when even that is absent. The
/// source port is never part of the key (every connection draws a fresh
/// one), and an IPv6 address is keyed by its /64 prefix (see [`ip_key`]).
pub(super) fn client(app: &AppState, request: &Request) -> String {
    let settings = &app.config.rate_limit;
    if settings.trust_proxy_headers.get()
        && let Some(forwarded) = forwarded_for(request, settings.trusted_proxy_hops)
    {
        return ip_key(forwarded);
    }
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or_else(
            || UNKNOWN_SUBJECT.to_owned(),
            |ConnectInfo(addr)| ip_key(addr.ip()),
        )
}

/// The window key of an address: IPv4 (and IPv4-mapped IPv6) as the bare
/// address, any other IPv6 as its /64 prefix, so a host cannot rotate
/// through the addresses of its own /64 to dodge a window.
fn ip_key(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            let [a, b, c, d, ..] = v6.segments();
            format!("{a:x}:{b:x}:{c:x}:{d:x}::/64")
        }
    }
}

/// The client address the outermost trusted proxy saw: the trimmed,
/// non-empty `X-Forwarded-For` entry `hops` places from the right (`hops`
/// = 1 is the rightmost entry, the one the proxy appended). Entries left of
/// it are client-controlled and never read. `None` when the header is
/// absent, holds fewer entries than `hops`, `hops` is zero, or the chosen
/// entry is not an IP address.
fn forwarded_for(request: &Request, hops: usize) -> Option<IpAddr> {
    let value = request
        .headers()
        .get(&FORWARDED_FOR_HEADER)?
        .to_str()
        .ok()?;
    let entries: Vec<&str> = value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    let index = entries.len().checked_sub(hops)?;
    entries.get(index)?.parse::<IpAddr>().ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{body::Body, http::StatusCode};
    use tower::ServiceExt as _;

    use super::super::{
        clock::ManualClock,
        test_support::{
            FLAGS_ON, app_on_pool, auth_app, auth_limits, forwarded_post, limited_post,
            limited_request, login, trusting,
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

    /// Two requests from one address over different source ports are one
    /// client; a different address is another.
    #[tokio::test]
    async fn source_port_is_not_part_of_the_key() {
        let clock = Arc::new(ManualClock::new());
        let app = auth_app(&clock, 1, FLAGS_ON);
        let first = app
            .clone()
            .oneshot(limited_post("203.0.113.1:1"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK, "first connection passes");
        let reconnect = app
            .clone()
            .oneshot(limited_post("203.0.113.1:2"))
            .await
            .unwrap();
        assert_eq!(
            reconnect.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "a new source port from the same address shares the window"
        );
        let other = app.oneshot(limited_post("203.0.113.2:1")).await.unwrap();
        assert_eq!(
            other.status(),
            StatusCode::OK,
            "a different address has its own window"
        );
    }

    /// An IPv6 client is keyed by its /64: addresses inside one prefix share
    /// a window, another prefix does not.
    #[tokio::test]
    async fn ipv6_clients_share_a_window_per_64_prefix() {
        let clock = Arc::new(ManualClock::new());
        let app = auth_app(&clock, 1, FLAGS_ON);
        let first = app
            .clone()
            .oneshot(limited_post("[2001:db8::1]:1"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK, "first address passes");
        let same_prefix = app
            .clone()
            .oneshot(limited_post("[2001:db8::2]:2"))
            .await
            .unwrap();
        assert_eq!(
            same_prefix.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "another address in the same /64 shares the window"
        );
        let other_prefix = app
            .oneshot(limited_post("[2001:db8:0:1::1]:1"))
            .await
            .unwrap();
        assert_eq!(
            other_prefix.status(),
            StatusCode::OK,
            "another /64 has its own window"
        );
    }

    /// The key is the bare address for IPv4 and IPv4-mapped IPv6, the /64
    /// prefix for other IPv6.
    #[test]
    fn ip_key_shapes() {
        let key = |text: &str| ip_key(text.parse::<IpAddr>().unwrap());
        assert_eq!(key("203.0.113.1"), "203.0.113.1", "IPv4 is bare");
        assert_eq!(key("::ffff:203.0.113.1"), "203.0.113.1", "mapped IPv4");
        assert_eq!(key("2001:db8::1"), "2001:db8:0:0::/64", "IPv6 prefix");
        assert_eq!(
            key("2001:db8:0:1:ffff::1"),
            "2001:db8:0:1::/64",
            "IPv6 low bits dropped"
        );
    }

    /// A trusted `X-Forwarded-For` address is keyed the same way, so a
    /// forwarded IPv6 client cannot rotate through its /64 either.
    #[tokio::test]
    async fn forwarded_ipv6_is_keyed_by_prefix() {
        let clock = Arc::new(ManualClock::new());
        let app = auth_app(&clock, 1, trusting(1));
        let first = app
            .clone()
            .oneshot(forwarded_post("10.0.0.1:1", "2001:db8::1"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK, "first passes");
        let second = app
            .oneshot(forwarded_post("10.0.0.1:2", "2001:db8::2"))
            .await
            .unwrap();
        assert_eq!(
            second.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "same forwarded /64, same window"
        );
    }

    /// `X-Forwarded-For` names the client only while
    /// `TRUST_PROXY_HEADERS` is on; while off, every request from one
    /// socket shares one window whatever the header claims.
    #[tokio::test]
    async fn forwarded_for_is_honored_only_when_trusted() {
        let trusted_clock = Arc::new(ManualClock::new());
        let trusted_app = auth_app(&trusted_clock, 1, trusting(1));
        let alice = trusted_app
            .clone()
            .oneshot(forwarded_post("10.0.0.1:1", "70.41.3.18"))
            .await
            .unwrap();
        assert_eq!(alice.status(), StatusCode::OK, "the first hop passes");
        let bob = trusted_app
            .oneshot(forwarded_post("10.0.0.1:1", "70.41.3.19"))
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

    /// The parsed address of `header` at `hops`, as `client` would read it.
    fn forwarded(header: &str, hops: usize) -> Option<String> {
        forwarded_for(&forwarded_post("10.0.0.1:1", header), hops).map(|ip| ip.to_string())
    }

    /// The entry `hops` places from the right is the client; entries left of
    /// it are never read, and a header too short, empty-entried or
    /// unparseable names nobody.
    #[test]
    fn forwarded_for_picks_the_entry_hops_from_the_right() {
        assert_eq!(
            forwarded("1.1.1.1, 2.2.2.2", 1).as_deref(),
            Some("2.2.2.2"),
            "one hop: the rightmost entry"
        );
        assert_eq!(
            forwarded("9.9.9.9, 1.1.1.1, 2.2.2.2", 2).as_deref(),
            Some("1.1.1.1"),
            "two hops: the one before the rightmost"
        );
        assert_eq!(
            forwarded("1.1.1.1, 2.2.2.2, ", 1).as_deref(),
            Some("2.2.2.2"),
            "empty entries are dropped"
        );
        assert_eq!(forwarded("2.2.2.2", 2), None, "fewer entries than hops");
        assert_eq!(forwarded("1.1.1.1, foo", 1), None, "a non-IP entry");
        assert_eq!(forwarded("foo, 2.2.2.2", 1).as_deref(), Some("2.2.2.2"));
        assert_eq!(forwarded("2.2.2.2", 0), None, "zero hops trusts nothing");
        assert_eq!(forwarded(" ,, ", 1), None, "no entries at all");
    }

    /// Behind one trusted proxy, requests whose spoofed left parts differ
    /// but whose appended right part is the same share one window.
    #[tokio::test]
    async fn a_spoofed_leftmost_entry_does_not_open_a_new_window() {
        let clock = Arc::new(ManualClock::new());
        let app = auth_app(&clock, 1, trusting(1));
        let first = app
            .clone()
            .oneshot(forwarded_post("10.0.0.1:1", "1.1.1.1, 2.2.2.2"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK, "the first request passes");
        let second = app
            .clone()
            .oneshot(forwarded_post("10.0.0.1:2", "3.3.3.3, 2.2.2.2"))
            .await
            .unwrap();
        assert_eq!(
            second.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "changing only the left part keeps the window"
        );
        let other = app
            .oneshot(forwarded_post("10.0.0.1:3", "1.1.1.1, 4.4.4.4"))
            .await
            .unwrap();
        assert_eq!(
            other.status(),
            StatusCode::OK,
            "another appended address has its own window"
        );
    }

    /// Two trusted hops read the second entry from the right.
    #[tokio::test]
    async fn two_hops_key_on_the_second_entry_from_the_right() {
        let clock = Arc::new(ManualClock::new());
        let app = auth_app(&clock, 1, trusting(2));
        let first = app
            .clone()
            .oneshot(forwarded_post("10.0.0.1:1", "9.9.9.9, 1.1.1.1, 2.2.2.2"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK, "the first request passes");
        let same_client = app
            .clone()
            .oneshot(forwarded_post("10.0.0.1:2", "8.8.8.8, 1.1.1.1, 5.5.5.5"))
            .await
            .unwrap();
        assert_eq!(
            same_client.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the same second-from-right entry shares the window"
        );
        let other_client = app
            .oneshot(forwarded_post("10.0.0.1:3", "9.9.9.9, 6.6.6.6, 2.2.2.2"))
            .await
            .unwrap();
        assert_eq!(
            other_client.status(),
            StatusCode::OK,
            "another second-from-right entry has its own window"
        );
    }

    /// A header with fewer entries than hops, or whose chosen entry is not
    /// an IP address, falls back to the socket peer's window.
    #[tokio::test]
    async fn unusable_forwarded_headers_fall_back_to_the_peer() {
        for (hops, first, second) in [(2, "1.1.1.1", "3.3.3.3"), (1, "foo", "bar")] {
            let clock = Arc::new(ManualClock::new());
            let app = auth_app(&clock, 1, trusting(hops));
            let opener = app
                .clone()
                .oneshot(forwarded_post("10.0.0.1:1", first))
                .await
                .unwrap();
            assert_eq!(opener.status(), StatusCode::OK, "{first:?} passes");
            let repeat = app
                .oneshot(forwarded_post("10.0.0.1:2", second))
                .await
                .unwrap();
            assert_eq!(
                repeat.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "{first:?} and {second:?} both count against the peer"
            );
        }
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
