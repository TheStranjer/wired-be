//! Tests for signed links: rewriting, signatures, decoding and purge targets.

use super::*;
use crate::config::{
    BASE_URL_VAR, DATABASE_URL_VAR, MEDIA_BASE_URL_VAR, MEDIA_PROXY_BASE_URL_VAR,
    MEDIA_PROXY_BLACKLIST_VAR, MEDIA_PROXY_CACHE_LIMIT_VAR, MEDIA_PROXY_ENABLED_VAR,
    MEDIA_PROXY_MAX_BYTES_VAR, MEDIA_PROXY_PURGE_URL_VAR, MEDIA_PROXY_SECRET_VAR, TEST_BASE_URL,
    TEST_DATABASE_URL,
};

/// A signing secret comfortably past the 32-character minimum.
const TEST_SECRET: &str = "0123456789abcdef0123456789abcdef01234567";
/// The remote URL every proxy test rewrites.
const REMOTE: &str = "https://remote.example/media/cat.gif";

/// A [`Config`] over the test constants with `extra` applied (which
/// may also override them).
fn config_with(extra: &[(&str, &str)]) -> Config {
    Config::from_lookup(|key| {
        extra
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_owned())
            .or_else(|| match key {
                DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
                BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
                _ => None,
            })
    })
    .unwrap()
}

/// A [`Config`] with the proxy enabled plus `extra` variables.
fn enabled_config(extra: &[(&str, &str)]) -> Config {
    let mut vars = vec![
        (MEDIA_PROXY_ENABLED_VAR, "true"),
        (MEDIA_PROXY_SECRET_VAR, TEST_SECRET),
    ];
    vars.extend_from_slice(extra);
    config_with(&vars)
}

/// The `(sig, encoded)` pair a generated proxy link carries.
fn sig_and_encoded(link: &str) -> (String, String) {
    let (_, tail) = link.split_once("/proxy/").unwrap();
    let mut segments = tail.split('/');
    (
        segments.next().unwrap().to_owned(),
        segments.next().unwrap().to_owned(),
    )
}

#[test]
fn enabled_rewrites_remote_urls_and_decode_round_trips() {
    let config = enabled_config(&[]);
    let link = proxy_url(&config, REMOTE);
    assert!(
        link.starts_with("https://test.example/proxy/"),
        "the link hangs off the instance base url: {link}"
    );
    assert!(
        link.ends_with("/cat.gif"),
        "the filename suffix rides on: {link}"
    );
    let (sig, encoded) = sig_and_encoded(&link);
    assert_eq!(
        encoded,
        URL_SAFE_NO_PAD.encode(REMOTE.as_bytes()),
        "the url segment is the base64url of the original"
    );
    assert_eq!(
        decode(&config, &sig, &encoded).unwrap(),
        REMOTE,
        "encode/decode round trip"
    );
}

#[test]
fn the_signature_is_base64url_hmac_sha256_over_the_encoded_url() {
    let config = enabled_config(&[]);
    let encoded = URL_SAFE_NO_PAD.encode(REMOTE.as_bytes());
    let mut mac = HmacSha256::new_from_slice(TEST_SECRET.as_bytes()).unwrap();
    mac.update(encoded.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    assert_eq!(
        proxy_url(&config, REMOTE),
        format!("https://test.example/proxy/{sig}/{encoded}/cat.gif"),
        "Pleroma's scheme with the ticket's SHA-256 pin"
    );
}

#[test]
fn a_tampered_signature_is_rejected() {
    let config = enabled_config(&[]);
    let (sig, encoded) = sig_and_encoded(&proxy_url(&config, REMOTE));
    let last = sig.chars().last().unwrap();
    let tampered = format!(
        "{}{}",
        sig.strip_suffix(last).unwrap(),
        if last == 'A' { 'B' } else { 'A' }
    );
    assert_eq!(
        decode(&config, &tampered, &encoded).unwrap_err(),
        ProxyError::InvalidSignature,
        "flipping a signature character must not verify"
    );
    assert_eq!(
        decode(&config, "", &encoded).unwrap_err(),
        ProxyError::InvalidSignature,
        "an empty signature is rejected"
    );
    assert_eq!(
        decode(&config, "!!!!", &encoded).unwrap_err(),
        ProxyError::InvalidSignature,
        "a signature that is not base64url is rejected"
    );
}

#[test]
fn a_tampered_payload_is_rejected() {
    let config = enabled_config(&[]);
    let (sig, _) = sig_and_encoded(&proxy_url(&config, REMOTE));
    let swapped = URL_SAFE_NO_PAD.encode(b"https://remote.example/media/dog.gif");
    assert_eq!(
        decode(&config, &sig, &swapped).unwrap_err(),
        ProxyError::InvalidSignature,
        "the signature is bound to exactly one encoded URL"
    );
}

#[test]
fn a_valid_signature_over_a_non_http_url_is_invalid() {
    let config = enabled_config(&[]);
    let payload = "ftp://remote.example/pub";
    let encoded = URL_SAFE_NO_PAD.encode(payload.as_bytes());
    let sig = encoded_signature(TEST_SECRET, &encoded).unwrap();
    assert_eq!(
        decode(&config, &sig, &encoded).unwrap_err(),
        ProxyError::InvalidUrl,
        "only http(s) may be proxied"
    );
}

#[test]
fn disabled_proxy_passes_urls_through_and_refuses_links() {
    let config = config_with(&[]);
    assert_eq!(
        proxy_url(&config, REMOTE),
        REMOTE,
        "disabled means byte-identical passthrough"
    );
    let (sig, encoded) = sig_and_encoded(&proxy_url(&enabled_config(&[]), REMOTE));
    assert_eq!(
        decode(&config, &sig, &encoded).unwrap_err(),
        ProxyError::Disabled,
        "a link cannot verify while the proxy is off"
    );
}

#[test]
fn blacklisted_hosts_are_not_proxied() {
    let config = enabled_config(&[(MEDIA_PROXY_BLACKLIST_VAR, "Evil.Example, blocked.example")]);
    for blacklisted in [
        "https://evil.example/a.png",
        "https://blocked.example/a.png",
    ] {
        assert_eq!(
            proxy_url(&config, blacklisted),
            blacklisted,
            "{blacklisted} names a blacklisted host (match is case-insensitive)"
        );
    }
    assert!(
        proxy_url(&config, "https://good.example/a.png").contains("/proxy/"),
        "an unlisted host is still proxied"
    );
}

#[test]
fn local_urls_are_not_proxied() {
    let config = enabled_config(&[(MEDIA_BASE_URL_VAR, "https://cdn.example/media")]);
    for local in [
        "https://test.example/media/x.png",
        "https://cdn.example/media/x.png",
    ] {
        assert_eq!(
            proxy_url(&config, local),
            local,
            "local {local} passes through"
        );
    }
    let ported = enabled_config(&[(BASE_URL_VAR, "http://localhost:4000")]);
    assert_eq!(
        proxy_url(&ported, "http://localhost:4000/media/x.png"),
        "http://localhost:4000/media/x.png",
        "the instance authority with its non-default port is local too"
    );
}

#[test]
fn non_http_and_unparsable_urls_pass_through() {
    let config = enabled_config(&[]);
    for url in ["ftp://example.com/x", "/media/x.png", "not a url", ""] {
        assert_eq!(
            proxy_url(&config, url),
            url,
            "{url:?} is not an http(s) URL and must not be rewritten"
        );
    }
}

#[test]
fn the_base_url_override_is_honoured() {
    let config = enabled_config(&[(MEDIA_PROXY_BASE_URL_VAR, "https://cdn.example/")]);
    let link = proxy_url(&config, REMOTE);
    assert!(
        link.starts_with("https://cdn.example/proxy/"),
        "the override replaces the origin, slash-normalised: {link}"
    );
}

#[test]
fn the_filename_suffix_follows_the_last_path_segment() {
    let config = enabled_config(&[]);
    for (url, suffix) in [
        (REMOTE, "/cat.gif"),
        ("https://remote.example/a/b/c.png", "/c.png"),
        ("https://remote.example/a/b/", ""),
        ("https://remote.example", ""),
        ("https://remote.example/x?q=1.png", "/x"),
    ] {
        let link = proxy_url(&config, url);
        let (sig, encoded) = sig_and_encoded(&link);
        assert_eq!(
            link,
            format!("https://test.example/proxy/{sig}/{encoded}{suffix}"),
            "the suffix is the last path segment; the query is not the path"
        );
        assert_eq!(
            encoded,
            URL_SAFE_NO_PAD.encode(url.as_bytes()),
            "the encoded segment keeps the full url, query included"
        );
    }
}

#[test]
fn max_bytes_is_taken_from_the_environment() {
    let config = enabled_config(&[(MEDIA_PROXY_MAX_BYTES_VAR, "4096")]);
    assert_eq!(config.media_proxy.max_bytes, 4096, "custom cap");
}

#[test]
fn for_tests_ships_the_proxy_disabled() {
    assert_eq!(
        proxy_url(&Config::for_tests(), REMOTE),
        REMOTE,
        "the shared test config leaves every entity URL untouched"
    );
}

#[test]
fn cache_limit_defaults_to_one_hundred_thousand() {
    let config = config_with(&[]);
    assert_eq!(
        config.media_proxy.cache_limit, 100_000,
        "the ticket default"
    );
    let custom = config_with(&[(MEDIA_PROXY_CACHE_LIMIT_VAR, "5")]);
    assert_eq!(custom.media_proxy.cache_limit, 5, "a custom limit reads");
}

#[test]
fn the_purge_target_rehangs_the_signed_link_on_the_purge_origin() {
    let config = enabled_config(&[(MEDIA_PROXY_PURGE_URL_VAR, "https://cdn.example/")]);
    assert_eq!(
        config.media_proxy.purge_url.as_deref(),
        Some("https://cdn.example"),
        "the trailing slash is trimmed on the read"
    );
    let target = purge_target(&config, REMOTE).expect("an enabled proxy with a purge url purges");
    let (sig, encoded) = sig_and_encoded(&target);
    assert!(
        target.starts_with("https://cdn.example/proxy/"),
        "the request goes to the purge origin, not the instance: {target}"
    );
    assert!(
        target.ends_with("/cat.gif"),
        "the filename segment rides along: {target}"
    );
    assert_eq!(
        decode(&config, &sig, &encoded).unwrap(),
        REMOTE,
        "the target carries exactly this url's signed link"
    );
}

#[test]
fn the_purge_target_is_none_without_a_purge_url_or_an_enabled_proxy() {
    assert_eq!(
        purge_target(&enabled_config(&[]), REMOTE),
        None,
        "no MEDIA_PROXY_PURGE_URL means no purge request, the ticket's skip"
    );
    let disabled = config_with(&[(MEDIA_PROXY_PURGE_URL_VAR, "https://cdn.example")]);
    assert_eq!(
        purge_target(&disabled, REMOTE),
        None,
        "a disabled proxy has no cached link to purge"
    );
    let config = enabled_config(&[(MEDIA_PROXY_PURGE_URL_VAR, "https://cdn.example")]);
    assert_eq!(
        purge_target(&config, "not a url"),
        None,
        "an unparsable url signs to nothing"
    );
    assert_eq!(
        purge_target(&config, "ftp://remote.example/x"),
        None,
        "and a non-http scheme likewise"
    );
}
