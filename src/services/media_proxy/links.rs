//! Signed proxy links: building them with [`proxy_url`] and [`purge_target`]
//! and validating them with [`decode`].

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2_011::Sha256;
use subtle::ConstantTimeEq as _;
use url::Url;

use super::{PROXY_PATH, ProxyError};
use crate::config::Config;

/// The HMAC flavor proxy signatures use: HMAC-SHA256 over the encoded URL.
type HmacSha256 = Hmac<Sha256>;

/// Rewrites `remote_url` into a signed proxy link for serving by
/// [`router`](super::router), following Pleroma's `MediaProxy.url/1`
/// (`lib/pleroma/web/media_proxy.ex`).
///
/// The original URL is returned unchanged unless the proxy is enabled and
/// `remote_url` is an `http(s)` URL that is neither local (served by this
/// instance itself) nor on a blacklisted host.
#[must_use]
pub fn proxy_url(config: &Config, remote_url: &str) -> String {
    let Some((secret, parsed)) = proxy_target(config, remote_url) else {
        return remote_url.to_owned();
    };
    let Some(path) = signed_link(secret, &parsed, remote_url) else {
        return remote_url.to_owned();
    };
    let base = config
        .media_proxy
        .base_url
        .as_deref()
        .unwrap_or(&config.base_url);
    format!("{base}{path}")
}

/// The signed proxy path `/proxy/{sig}/{b64url(url)}[/{filename}]` for
/// `remote_url` under `secret` — the body of the link [`proxy_url`] hangs
/// off its base and [`purge_target`] off the purge origin. `None` only
/// when the signature cannot be minted; the caller has already gated
/// scheme, locality and blacklist.
fn signed_link(secret: &str, parsed: &Url, remote_url: &str) -> Option<String> {
    let encoded = URL_SAFE_NO_PAD.encode(remote_url.as_bytes());
    let sig = encoded_signature(secret, &encoded)?;
    let mut path = format!("{PROXY_PATH}/{sig}/{encoded}");
    if let Some(filename) = filename_for(parsed) {
        path.push('/');
        path.push_str(&filename);
    }
    Some(path)
}

/// The address a `PURGE` cache-invalidation request for media `url` goes
/// to: the signed proxy link re-hung on the purge origin.
///
/// The admin purge endpoint's outbound half (WBE-208) — exactly what a
/// cache fronting the `/proxy` routes holds the bytes under. `None` —
/// send nothing — when the media proxy is off (no link exists to purge),
/// `MEDIA_PROXY_PURGE_URL` is unset (the ticket's "skipped when unset"),
/// or `url` is no signable http(s) URL.
#[must_use]
pub fn purge_target(config: &Config, url: &str) -> Option<String> {
    let proxy = &config.media_proxy;
    if !proxy.enabled {
        return None;
    }
    let purge = proxy.purge_url.as_deref()?;
    let secret = proxy.secret.as_deref()?;
    let parsed = Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let path = signed_link(secret, &parsed, url)?;
    Some(format!("{purge}{path}"))
}

/// The pieces [`proxy_url`] needs — the secret and the parsed URL — when
/// `remote_url` is proxiable; [`None`] means it passes through.
fn proxy_target<'config>(config: &'config Config, remote_url: &str) -> Option<(&'config str, Url)> {
    let proxy = &config.media_proxy;
    if !proxy.enabled {
        return None;
    }
    let secret = proxy.secret.as_deref()?;
    let parsed = Url::parse(remote_url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    if is_local(config, &parsed) || is_blacklisted(proxy, &parsed) {
        return None;
    }
    Some((secret, parsed))
}

/// Whether `url` is served by this instance itself: its authority is the
/// instance origin (host, plus port when non-default) or its host is the
/// host of the configured media prefix — Pleroma's `local?` and its
/// upload-`base_url` whitelist entry combined.
fn is_local(config: &Config, url: &Url) -> bool {
    if url.authority() == config.domain() {
        return true;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let Ok(media) = Url::parse(&config.media_base_url) else {
        return false;
    };
    media
        .host_str()
        .is_some_and(|media_host| media_host.eq_ignore_ascii_case(host))
}

/// Whether `url`'s host is blacklisted (`MEDIA_PROXY_BLACKLIST`, matched
/// case-insensitively against bare hosts).
fn is_blacklisted(proxy: &crate::config::MediaProxyConfig, url: &Url) -> bool {
    url.host_str()
        .is_some_and(|host| proxy.blacklist.iter().any(|e| e.eq_ignore_ascii_case(host)))
}

/// The last path segment of `url`, when it names a file: the optional
/// `/{filename}` suffix of proxy links (Pleroma's `filename/1`).
fn filename_for(url: &Url) -> Option<String> {
    let last = url.path().rsplit('/').next()?;
    (!last.is_empty() && last != "." && last != "..").then(|| last.to_owned())
}

/// The raw HMAC-SHA256 of `message` keyed by `secret`.
fn hmac_bytes(secret: &str, message: &[u8]) -> Option<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(message);
    Some(mac.finalize().into_bytes().to_vec())
}

/// The base64url signature over the already-encoded URL: Pleroma's
/// `base64_sig64` with the ticket's SHA-256 pin.
fn encoded_signature(secret: &str, encoded: &str) -> Option<String> {
    Some(URL_SAFE_NO_PAD.encode(hmac_bytes(secret, encoded.as_bytes())?))
}

/// Validates a proxy link's two path segments and returns the original
/// URL.
///
/// `sig` must match the base64url HMAC-SHA256 of `encoded` — in constant
/// time — before `encoded` is base64url-decoded, and what comes back must
/// be an `http(s)` URL. Pleroma's `decode_url/2` with the ticket's
/// hardening.
///
/// # Errors
///
/// [`ProxyError::Disabled`] when the proxy is off (or unconfigured),
/// [`ProxyError::InvalidSignature`] when `sig` does not verify over
/// `encoded` (or either segment is not base64url), and
/// [`ProxyError::InvalidUrl`] when a verifying payload is not an
/// `http(s)` URL.
pub fn decode(config: &Config, sig: &str, encoded: &str) -> Result<String, ProxyError> {
    let proxy = &config.media_proxy;
    if !proxy.enabled {
        return Err(ProxyError::Disabled);
    }
    let secret = proxy.secret.as_deref().ok_or(ProxyError::Disabled)?;
    let want = hmac_bytes(secret, encoded.as_bytes()).ok_or(ProxyError::InvalidSignature)?;
    let given = URL_SAFE_NO_PAD
        .decode(sig)
        .map_err(|_err| ProxyError::InvalidSignature)?;
    if !bool::from(given.as_slice().ct_eq(want.as_slice())) {
        return Err(ProxyError::InvalidSignature);
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_err| ProxyError::InvalidSignature)?;
    let url = String::from_utf8(decoded).map_err(|_err| ProxyError::InvalidUrl)?;
    let Ok(parsed) = Url::parse(&url) else {
        return Err(ProxyError::InvalidUrl);
    };
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(ProxyError::InvalidUrl);
    }
    Ok(url)
}

#[cfg(test)]
mod tests;
