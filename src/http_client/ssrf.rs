//! SSRF guard for the outbound [`HttpClient`](super::HttpClient).
//!
//! Every target is checked before this process connects to it. Literal IP
//! hosts are tested directly against [`is_blocked_ip`]; hostnames are
//! resolved with [`tokio::net::lookup_host`] and refused if *any* resolved
//! address is blocked. The check is not just a pre-flight: the client
//! installs `SsrfGuard` as its `reqwest` DNS resolver, so the addresses
//! it connects to are the very ones the guard validated (no DNS rebinding
//! window between check and connect), and `redirect_policy` re-checks
//! redirect targets — including IP literals, for which `hyper` skips DNS
//! resolution entirely. All ports are allowed; only addresses are
//! filtered. Behavioural reference: Pleroma's private-IP checks in
//! `lib/pleroma/http/` and `lib/pleroma/web/rich_media/helpers.ex`.

use std::{
    error::Error,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};

use reqwest::{
    dns::{Addrs, Name, Resolve, Resolving},
    redirect,
};
use tokio::net::lookup_host;
use url::{Host, Url};

use super::HttpError;

/// The suffix of every block reason: what kind of address was refused.
const BLOCKED_ADDRESS_TEXT: &str = "is loopback, private, or otherwise reserved";

/// The error type `reqwest`'s [`Resolve`] contract boxes failures into.
type BoxedError = Box<dyn Error + Send + Sync>;

/// Whether `ip` is an address a federating client must never connect to.
///
/// Blocked ranges: loopback, RFC 1918 private (10/8, 172.16/12,
/// 192.168/16), link-local (169.254/16, `fe80::/10`), unique-local
/// (`fc00::/7`), unspecified, multicast, broadcast, CGNAT (100.64/10),
/// and an IPv4-mapped IPv6 (`::ffff:0:0/96`) of any of those. All ports
/// are allowed; only the address is judged.
#[must_use]
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => blocked_v6(v6),
    }
}

/// The IPv4 half of [`is_blocked_ip`].
fn blocked_v4(ip: Ipv4Addr) -> bool {
    let [first, second, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || (first == 100 && (64..=127).contains(&second))
}

/// The IPv6 half of [`is_blocked_ip`], including IPv4-mapped addresses.
fn blocked_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return blocked_v4(mapped);
    }
    let [head, ..] = ip.segments();
    (head & 0xffc0) == 0xfe80 || (head & 0xfe00) == 0xfc00
}

/// The address filtering every outbound [`HttpClient`] request passes
/// through: the pre-flight check ([`SsrfGuard::check_target`]), the pinned
/// connect-time resolver (`impl Resolve`) and the redirect re-check
/// ([`redirect_policy`]). Installed on the client unless
/// `HTTP_ALLOW_PRIVATE_HOSTS` disables the guard.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SsrfGuard;

impl SsrfGuard {
    /// Validates `url`'s host before any request is sent: literal IP
    /// hosts go straight to [`is_blocked_ip`], hostnames are resolved and
    /// refused if any resolved address is blocked. A hostname that fails
    /// to resolve passes here — there is nothing to block, and `reqwest`
    /// reports the failure as [`HttpError::Connect`] when it connects.
    ///
    /// # Errors
    ///
    /// [`HttpError::Blocked`] naming the refused address.
    pub(crate) async fn check_target(&self, url: &Url) -> Result<(), HttpError> {
        match url.host() {
            Some(Host::Ipv4(ip)) => reject_literal(IpAddr::V4(ip)),
            Some(Host::Ipv6(ip)) => reject_literal(IpAddr::V6(ip)),
            Some(Host::Domain(name)) => self.check_domain(name, url).await,
            None => Ok(()),
        }
    }

    /// Resolves hostname `name` (on the URL's effective port) and refuses
    /// it when any address is blocked.
    async fn check_domain(&self, name: &str, url: &Url) -> Result<(), HttpError> {
        let port = url.port_or_known_default().unwrap_or(0);
        let Ok(addresses) = lookup_host((name, port)).await else {
            return Ok(());
        };
        let addresses: Vec<SocketAddr> = addresses.collect();
        if let Some(blocked) = addresses.iter().find(|address| is_blocked_ip(address.ip())) {
            return Err(HttpError::Blocked(format!(
                "host `{name}` resolves to {ip}, which {BLOCKED_ADDRESS_TEXT}",
                ip = blocked.ip()
            )));
        }
        Ok(())
    }

    /// Refuse the resolved set when any address is blocked; hand `reqwest`
    /// the validated addresses to connect with.
    fn validated(host: &str, addresses: Vec<SocketAddr>) -> Result<Addrs, BoxedError> {
        for address in &addresses {
            if is_blocked_ip(address.ip()) {
                let reason = format!(
                    "host `{host}` resolves to {ip}, which {BLOCKED_ADDRESS_TEXT}",
                    ip = address.ip()
                );
                let blocked: BoxedError = Box::new(BlockedError::new(reason));
                return Err(blocked);
            }
        }
        Ok(Box::new(addresses.into_iter()))
    }
}

impl Resolve for SsrfGuard {
    /// Connect-time half of the guard: resolves `name` with port 0 (the
    /// request's own port is applied by `hyper`) and hands `reqwest` only
    /// addresses that passed [`is_blocked_ip`], refusing the connection
    /// outright when any address is blocked. Because this lookup *is* the
    /// one the connection uses, no rebinding can slip a blocked address
    /// past the pre-flight [`check_target`](Self::check_target).
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            match lookup_host((host.as_str(), 0)).await {
                Ok(addresses) => Self::validated(&host, addresses.collect()),
                Err(err) => {
                    let boxed: BoxedError = err.into();
                    Err(boxed)
                }
            }
        })
    }
}

/// An SSRF refusal travelling inside a `reqwest` error chain, so
/// [`blocked_reason`] can recognise a guard rejection (from the resolver or
/// the redirect policy) no matter how many layers wrap it.
#[derive(Debug)]
pub(crate) struct BlockedError {
    /// Human-readable reason the target was refused.
    reason: String,
}

impl BlockedError {
    /// A refusal carrying `reason`.
    const fn new(reason: String) -> Self {
        Self { reason }
    }
}

impl fmt::Display for BlockedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl Error for BlockedError {}

/// The reason an SSRF refusal reports, walking `err`'s source chain for a
/// [`BlockedError`] marker; [`None`] for every other failure.
pub(crate) fn blocked_reason(err: &(dyn Error + 'static)) -> Option<String> {
    let mut cursor: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(current) = cursor {
        if let Some(blocked) = current.downcast_ref::<BlockedError>() {
            return Some(blocked.reason.clone());
        }
        cursor = current.source();
    }
    None
}

/// The redirect policy the guarded client installs: refuses every hop whose
/// target is a blocked IP literal (those bypass the DNS resolver when
/// `hyper` connects to them) and otherwise follows the caller's
/// `max_redirects`-hop limit.
pub(crate) fn redirect_policy(max_redirects: usize) -> redirect::Policy {
    let limited = redirect::Policy::limited(max_redirects);
    redirect::Policy::custom(move |attempt| {
        let target = attempt.url().clone();
        if let Some(reason) = blocked_redirect_reason(&target) {
            return attempt.error(BlockedError::new(reason));
        }
        limited.redirect(attempt)
    })
}

/// The refusal text for redirect target `url`, or [`None`] when the hop
/// may be followed. Only IP literals are judged here: a hostname hop goes
/// through [`SsrfGuard`]'s resolver when it connects, which is its check.
fn blocked_redirect_reason(url: &Url) -> Option<String> {
    let ip = match url.host() {
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
        Some(Host::Domain(_)) | None => return None,
    };
    is_blocked_ip(ip)
        .then(|| format!("redirect target address {ip}, which {BLOCKED_ADDRESS_TEXT} (SSRF guard)"))
}

/// Turns a blocked literal-IP host into its refusal.
fn reject_literal(ip: IpAddr) -> Result<(), HttpError> {
    if is_blocked_ip(ip) {
        Err(HttpError::Blocked(format!(
            "target address {ip}, which {BLOCKED_ADDRESS_TEXT} (SSRF guard)"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use super::*;

    /// Parses `text` into a [`Url`] for the check functions.
    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn is_blocked_ip_refuses_every_reserved_range() {
        for (text, range) in [
            ("127.0.0.1", "IPv4 loopback"),
            ("127.1.2.3", "IPv4 loopback, not just .1"),
            ("10.0.0.1", "10/8"),
            ("10.255.255.255", "10/8 upper edge"),
            ("172.16.0.1", "172.16/12 lower edge"),
            ("172.31.255.255", "172.16/12 upper edge"),
            ("192.168.1.1", "192.168/16"),
            ("169.254.0.1", "169.254/16 lower edge"),
            ("169.254.255.255", "169.254/16 upper edge"),
            ("100.64.0.1", "CGNAT lower edge"),
            ("100.127.255.255", "CGNAT upper edge"),
            ("0.0.0.0", "IPv4 unspecified"),
            ("224.0.0.1", "IPv4 multicast lower edge"),
            ("239.10.10.10", "IPv4 multicast"),
            ("255.255.255.255", "IPv4 broadcast"),
            ("::1", "IPv6 loopback"),
            ("::", "IPv6 unspecified"),
            ("ff02::1", "IPv6 multicast"),
            ("fe80::1", "fe80::/10 lower edge"),
            ("febf:ffff::1", "fe80::/10 upper edge"),
            ("fc00::1", "fc00::/7 lower edge"),
            ("fd12:3456::789a", "fc00::/7 (unique local)"),
            ("::ffff:127.0.0.1", "IPv4-mapped loopback"),
            ("::ffff:10.0.0.5", "IPv4-mapped private"),
            ("::ffff:169.254.1.1", "IPv4-mapped link-local"),
            ("::ffff:100.64.0.1", "IPv4-mapped CGNAT"),
            ("::ffff:0.0.0.0", "IPv4-mapped unspecified"),
            ("::ffff:255.255.255.255", "IPv4-mapped broadcast"),
            ("::ffff:224.0.0.1", "IPv4-mapped multicast"),
        ] {
            let ip = IpAddr::from_str(text).unwrap();
            assert!(is_blocked_ip(ip), "{text} ({range}) must be blocked");
        }
    }

    #[test]
    fn is_blocked_ip_allows_public_addresses() {
        for (text, range) in [
            ("8.8.8.8", "public DNS"),
            ("1.1.1.1", "public DNS (Cloudflare)"),
            ("93.184.216.34", "public web"),
            ("2606:4700:4700::1111", "public DNS over IPv6"),
            ("2001:4860:4860::8888", "public DNS over IPv6 (Google)"),
            ("100.63.0.1", "just below CGNAT"),
            ("100.128.0.1", "just above CGNAT"),
            ("172.15.0.1", "just below 172.16/12"),
            ("172.32.0.1", "just above 172.16/12"),
            ("169.253.0.1", "just below 169.254/16"),
            ("169.255.0.1", "just above 169.254/16"),
            ("fe7f:ffff::1", "just below fe80::/10"),
            ("::ffff:8.8.8.8", "IPv4-mapped public address"),
        ] {
            let ip = IpAddr::from_str(text).unwrap();
            assert!(!is_blocked_ip(ip), "{text} ({range}) must pass");
        }
    }

    #[test]
    fn blocked_redirect_reason_refuses_private_literals() {
        for text in [
            "http://127.0.0.1:8080/x",
            "https://10.1.2.3/y",
            "http://192.168.0.1/z",
            "http://169.254.9.9/w",
            "http://100.64.0.8/v",
            "http://[fe80::2]/u",
            "http://[fd00::1]/t",
            "http://[::ffff:192.168.0.1]/s",
            "http://[::1]/r",
        ] {
            let reason = blocked_redirect_reason(&url(text))
                .unwrap_or_else(|| panic!("{text} must be refused by the redirect policy"));
            assert!(
                reason.contains("SSRF guard"),
                "{text} reason names the guard: {reason}"
            );
        }
    }

    #[test]
    fn blocked_redirect_reason_allows_public_and_named_targets() {
        for text in [
            "https://8.8.8.8/x",
            "https://[2606:4700:4700::1111]/y",
            "https://example.com/z",
        ] {
            assert!(
                blocked_redirect_reason(&url(text)).is_none(),
                "{text} must be followed (hostnames are re-checked by the resolver at connect)"
            );
        }
    }

    #[tokio::test]
    async fn resolver_refuses_a_host_that_resolves_to_loopback() {
        let name: Name = "localhost".parse().unwrap();
        let outcome = SsrfGuard.resolve(name).await;
        match outcome {
            Ok(mut addrs) => {
                let reached = addrs.next();
                panic!("`localhost` must not resolve through the guard, got {reached:?}");
            }
            Err(err) => {
                let reason = blocked_reason(err.as_ref())
                    .unwrap_or_else(|| panic!("a guard refusal is in the chain: {err}"));
                assert!(
                    reason.contains("resolves to") && reason.contains("loopback"),
                    "the reason names the cause: {reason}"
                );
            }
        }
    }

    #[tokio::test]
    async fn resolver_reports_plain_dns_failures_as_unblocked() {
        let name: Name = "no-such-host.invalid".parse().unwrap();
        match SsrfGuard.resolve(name).await {
            Ok(mut addrs) => {
                assert!(
                    addrs.next().is_none(),
                    "an `.invalid` host resolves to nothing"
                );
            }
            Err(err) => {
                assert!(
                    blocked_reason(err.as_ref()).is_none(),
                    "a DNS failure is not an SSRF block: {err}"
                );
            }
        }
    }

    /// A wrapper that reports its payload via `source()`, standing in for
    /// the `reqwest`/`hyper` layers that carry a guard refusal. (`std::io::Error`
    /// deliberately does not expose its payload as a source.)
    #[derive(Debug)]
    struct Chain<E: Error + 'static> {
        /// The wrapped error.
        inner: E,
    }

    impl<E: Error + 'static> fmt::Display for Chain<E> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "request failed: {}", self.inner)
        }
    }

    impl<E: Error + 'static> Error for Chain<E> {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.inner)
        }
    }

    #[test]
    fn blocked_reason_walks_the_error_source_chain() {
        let wrapped = Chain {
            inner: BlockedError::new("private address 10.0.0.1".to_owned()),
        };
        assert_eq!(
            blocked_reason(&wrapped).as_deref(),
            Some("private address 10.0.0.1"),
            "the marker is found one layer down"
        );
        let unrelated = Chain {
            inner: std::io::Error::other("no marker here"),
        };
        assert_eq!(
            blocked_reason(&unrelated),
            None,
            "a chain without a marker reports nothing"
        );
        assert_eq!(
            blocked_reason(&std::io::Error::other("plain failure")).as_deref(),
            None,
            "a plain DNS/connect failure is not an SSRF block"
        );
        assert_eq!(
            BlockedError::new("kept".to_owned()).to_string(),
            "kept",
            "the display text is the reason"
        );
    }

    #[tokio::test]
    async fn check_target_refuses_literals_and_localhost_by_name() {
        for (text, needle) in [
            ("http://127.0.0.1:8080/x", "127.0.0.1"),
            ("http://[::ffff:10.9.8.7]/y", "::ffff:10.9.8.7"),
            // Which loopback address `localhost` names first is environment-dependent.
            ("http://localhost:8080/z", "resolves to"),
        ] {
            let outcome = SsrfGuard.check_target(&url(text)).await;
            match outcome {
                Err(HttpError::Blocked(reason)) => assert!(
                    reason.contains(needle),
                    "{text} must name {needle}: {reason}"
                ),
                Ok(()) => panic!("{text} must be blocked"),
                Err(other) => panic!("{text} must block, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn check_target_passes_public_literals() {
        assert!(
            SsrfGuard
                .check_target(&url("http://8.8.8.8:9999/x"))
                .await
                .is_ok(),
            "a public literal address passes the pre-flight (the port is unrestricted)"
        );
    }
}
