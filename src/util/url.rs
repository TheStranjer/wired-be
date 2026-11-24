//! The `http(s)` URL checks shared by federation, signing, delivery and the
//! admin API.
//!
//! These guard origin containment, request signing and delivery targets, so
//! there is exactly one definition of "an absolute `http` or `https` URL with
//! a host" and of how a host is spelled: [`Url`] lowercases hosts, and every
//! caller compares what these helpers return.

use url::Url;

use crate::config::Config;

/// Parses `text` as an absolute URL on the `http` or `https` scheme with a
/// host; `None` for anything else.
#[must_use]
pub fn parse_http_url(text: &str) -> Option<Url> {
    let parsed = Url::parse(text).ok()?;
    let usable = matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some();
    usable.then_some(parsed)
}

/// True when `text` is an absolute `http(s)` URL with a host. Callers store
/// the raw text, not the re-rendered URL.
#[must_use]
pub fn is_http_url(text: &str) -> bool {
    parse_http_url(text).is_some()
}

/// The host of an absolute `http(s)` URL, `None` for anything else.
///
/// URL parsing lowercases the host, so hosts compare case-insensitively. The
/// port is not part of it.
#[must_use]
pub fn http_host(text: &str) -> Option<String> {
    parse_http_url(text)?.host_str().map(str::to_owned)
}

/// The URL's host, plus `:port` only when the URL spells out a non-default
/// port (`Url::port` reports `None` for an absent or default one). A URL
/// without a host gets an empty host part.
#[must_use]
pub fn host_with_port(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    url.port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"))
}

/// Whether two URLs name the same origin: the same host and port.
///
/// The comparison is case-insensitive on [`host_with_port`]. Userinfo is
/// ignored, so `https://evil@peer` is `https://peer`, and so is the scheme
/// (`http` and `https` on their default ports compare equal, as delivery has
/// always treated an inbox). A URL without a host matches nothing.
#[must_use]
pub fn same_origin(a: &Url, b: &Url) -> bool {
    a.host_str().is_some()
        && b.host_str().is_some()
        && host_with_port(a).eq_ignore_ascii_case(&host_with_port(b))
}

/// Whether `url` is on this instance's own origin, the one in
/// [`Config::base_url`]. A `base_url` that does not parse matches nothing.
#[must_use]
pub fn is_local_url(config: &Config, url: &Url) -> bool {
    Url::parse(&config.base_url).is_ok_and(|base| same_origin(&base, url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TEST_BASE_URL;

    /// Parses a URL the test spells out.
    fn url(text: &str) -> Url {
        Url::parse(text).expect("valid url")
    }

    /// Host case does not matter.
    #[test]
    fn same_origin_ignores_host_case() {
        assert!(same_origin(
            &url("https://Peer.Example/a"),
            &url("https://peer.example/b")
        ));
    }

    /// A different port is a different origin; a default port is not a port.
    #[test]
    fn same_origin_compares_ports() {
        assert!(!same_origin(
            &url("https://peer.example:8443/"),
            &url("https://peer.example/")
        ));
        assert!(same_origin(
            &url("https://peer.example:443/"),
            &url("https://peer.example/")
        ));
        assert!(!same_origin(
            &url("https://a.example/"),
            &url("https://b.example/")
        ));
    }

    /// Userinfo never makes or breaks a match.
    #[test]
    fn same_origin_ignores_userinfo() {
        assert!(same_origin(
            &url("https://evil@peer.example/"),
            &url("https://peer.example/")
        ));
        assert!(same_origin(
            &url("https://u:p@peer.example/"),
            &url("https://x@peer.example/")
        ));
    }

    /// The scheme is not compared: `http` and `https` on the default ports
    /// of each are the same origin, as `is_local_inbox` has always treated
    /// them.
    #[test]
    fn same_origin_ignores_the_scheme() {
        assert!(same_origin(
            &url("http://peer.example/"),
            &url("https://peer.example/")
        ));
    }

    /// URLs without a host match nothing, not even each other.
    #[test]
    fn same_origin_needs_a_host() {
        assert!(!same_origin(&url("urn:uuid:1"), &url("urn:uuid:2")));
        assert!(!same_origin(
            &url("urn:uuid:1"),
            &url("https://peer.example/")
        ));
    }

    /// `is_local_url` compares against the configured base URL.
    #[test]
    fn is_local_url_compares_against_the_base_url() {
        let config = Config::for_tests();
        assert!(is_local_url(
            &config,
            &url(&format!("{TEST_BASE_URL}/users/alice"))
        ));
        assert!(is_local_url(&config, &url("https://TEST.EXAMPLE/x")));
        assert!(is_local_url(&config, &url("https://evil@test.example/x")));
        assert!(!is_local_url(&config, &url("https://test.example.evil/x")));
        assert!(!is_local_url(&config, &url("https://test.example:8443/x")));
    }

    /// Both web schemes parse; the result is the parsed URL.
    #[test]
    fn parse_http_url_accepts_http_and_https() {
        assert!(parse_http_url("https://example.org/a?b=c#d").is_some());
        assert!(parse_http_url("http://127.0.0.1:8080/hook").is_some());
        assert_eq!(
            parse_http_url("https://example.org/a").map(|url| url.to_string()),
            Some("https://example.org/a".to_owned()),
        );
    }

    /// Other schemes, relative text and a missing host are refused.
    #[test]
    fn parse_http_url_rejects_everything_else() {
        for text in [
            "ftp://example.org/a",
            "mailto:a@example.org",
            "example.org/hook",
            "/relative/path",
            "https://",
            "http://:80/x",
            "file:///etc/passwd",
            "",
            "not a url",
        ] {
            assert!(parse_http_url(text).is_none(), "{text:?} must be refused");
        }
    }

    /// `is_http_url` agrees with `parse_http_url`.
    #[test]
    fn is_http_url_accepts_http_and_https_only() {
        assert!(is_http_url("https://example.org/a"));
        assert!(is_http_url("http://example.org"));
        assert!(!is_http_url("ftp://example.org"));
        assert!(!is_http_url("mailto:a@example.org"));
        assert!(!is_http_url("https://"));
        assert!(!is_http_url("http://:80/x"));
        assert!(!is_http_url("example.org/hook"));
    }

    /// The host is lowercased and the port left out.
    #[test]
    fn http_host_lowercases_and_drops_the_port() {
        assert_eq!(
            http_host("HTTPS://Peer.Example/x").as_deref(),
            Some("peer.example"),
        );
        assert_eq!(
            http_host("http://127.0.0.1:4000/users/alice#main-key").as_deref(),
            Some("127.0.0.1"),
        );
        assert_eq!(http_host("urn:uuid:1234"), None);
        assert_eq!(http_host("ftp://x.example/a"), None);
        assert_eq!(http_host("not a url"), None);
    }

    /// Only a real, non-default port is appended.
    #[test]
    fn host_with_port_appends_only_non_default_ports() {
        let spell = |text: &str| host_with_port(&Url::parse(text).expect("valid url"));
        assert_eq!(spell("https://h"), "h");
        assert_eq!(
            spell("https://h:443"),
            "h",
            "Url normalises the default port"
        );
        assert_eq!(spell("https://h:8443"), "h:8443");
        assert_eq!(spell("http://h:80/x"), "h");
    }
}
