//! The media proxy: signed URL rewriting and the `/proxy` fetch route.
//!
//! Pleroma serves remote media through its own origin so clients never
//! contact (and leak their IP to) remote hosts: `Pleroma.Web.MediaProxy`
//! (`lib/pleroma/web/media_proxy.ex`) rewrites a remote URL into
//! `{base}/proxy/{sig}/{b64url(url)}` and
//! `Pleroma.Web.MediaProxy.MediaProxyController`
//! (`lib/pleroma/web/media_proxy/media_proxy_controller.ex`) validates the
//! signature and streams the bytes back. This module is the same scheme,
//! per the ticket:
//!
//! - [`proxy_url`] rewrites an http(s) URL into a signed proxy link when
//!   `MEDIA_PROXY_ENABLED` is set, the URL is not on this instance's own
//!   host and its host is not in `MEDIA_PROXY_BLACKLIST`; anything else
//!   passes through untouched. The link carries an optional filename
//!   segment (the URL's last path segment) purely so downloads get a
//!   sensible name; it is ignored when serving.
//! - `sig` is `base64url(HMAC-SHA256(MEDIA_PROXY_SECRET, b64url(url)))` —
//!   like Pleroma, the signature is over the *encoded* URL, not the raw
//!   one (Pleroma itself HMACs with SHA-1; the ticket pins SHA-256).
//! - [`decode`] validates a link's signature in constant time and yields
//!   the original URL, refusing anything that is not `http(s)`.
//! - [`router`] serves `GET /proxy/{sig}/{url}[/{filename}]` and the
//!   preview path (`/proxy/preview/…`, which serves the same bytes — no
//!   resizing). The route validates the signature (403
//!   `{"error":"Invalid signature"}`), fetches through the SSRF-guarded
//!   [`HttpClient`](crate::http_client), caps the body at
//!   `MEDIA_PROXY_MAX_BYTES` and aborts beyond it, and only passes media
//!   (`image/…`, `video/…`, `audio/…`) content types (anything else 424
//!   `{"error":"Unsupported media type"}`), forced `inline`, `nosniff`,
//!   cached for a day and locked down by a `default-src 'none'` CSP;
//!   `Range` and `If-None-Match`/`If-Modified-Since` are forwarded
//!   upstream and the `206`/`304` answers passed back.
//!
//! Every URL this route serves is also recorded in the admin-visible
//! media-proxy cache ledger ([`crate::models::media_proxy_cache`],
//! WBE-208), kept within `MEDIA_PROXY_CACHE_LIMIT` rows, and
//! [`purge_target`] mints the address a `PURGE` invalidation request for
//! a URL goes to when `MEDIA_PROXY_PURGE_URL` is configured — the
//! admin's `POST /media_proxy_caches/{delete,purge}` actions use both.

use std::{error::Error, fmt};

use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderName, StatusCode, header},
    response::Response,
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2_011::Sha256;
use subtle::ConstantTimeEq as _;
use tracing::{error, warn};
use url::Url;

use crate::{
    config::Config,
    error::AppError,
    http_client::{HttpError, HttpResponse},
    models::media_proxy_cache,
    state::AppState,
};

/// The HMAC flavor proxy signatures use: HMAC-SHA256 over the encoded URL.
type HmacSha256 = Hmac<Sha256>;

/// Path prefix of the proxy links [`proxy_url`] builds (and the route
/// prefix [`router`] serves).
const PROXY_PATH: &str = "/proxy";
/// Body message for a link whose signature does not verify.
const INVALID_SIGNATURE_MESSAGE: &str = "Invalid signature";
/// Body message for a proxied body whose type is not media.
const UNSUPPORTED_MESSAGE: &str = "Unsupported media type";
/// Body message for an upstream fetch that failed outright.
const UPSTREAM_MESSAGE: &str = "Upstream media request failed";
/// Body message for a proxied body over `MEDIA_PROXY_MAX_BYTES`.
const TOO_LARGE_MESSAGE: &str = "Media exceeds the size limit";
/// `cache-control` for proxied media: a day, per the ticket (Pleroma's
/// proxy default is a fortnight, `@default_cache_control_header` in
/// `lib/pleroma/reverse_proxy.ex`).
const CACHE_CONTROL: &str = "public, max-age=86400";
/// `x-content-type-options`: the type served here is the type to use.
const NOSNIFF: &str = "nosniff";
/// Proxied media renders in the browser; nothing downloads as an
/// attachment from this route.
const INLINE: &str = "inline";
/// `content-security-policy` for proxy responses, verbatim from the
/// ticket: no scripts, only inline styles and same-origin media and
/// images — in the spirit of Pleroma's `HttpSecurityPlug` media
/// policy (`lib/pleroma/web/plugs/http_security_plug.ex`).
const CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; media-src 'self' data:; img-src 'self' data:";
/// The request headers forwarded upstream: range requests and
/// conditional revalidation, Pleroma's `@range_headers` plus the
/// conditional half of its `@keep_req_headers`
/// (`lib/pleroma/reverse_proxy.ex`). Nothing else about the client —
/// cookies, bearer tokens, user agents — reaches the remote host.
static FORWARDED_REQUEST_HEADERS: [HeaderName; 4] = [
    HeaderName::from_static("range"),
    HeaderName::from_static("if-range"),
    HeaderName::from_static("if-none-match"),
    HeaderName::from_static("if-modified-since"),
];
/// The upstream validation/partial-content headers copied into proxy
/// responses (members of Pleroma's `@resp_cache_headers` plus
/// `content-range` from its `@keep_resp_headers`).
static VALIDATOR_HEADERS: [HeaderName; 3] = [
    HeaderName::from_static("etag"),
    HeaderName::from_static("last-modified"),
    HeaderName::from_static("content-range"),
];
/// The `content-security-policy` header name.
static CSP_HEADER: HeaderName = HeaderName::from_static("content-security-policy");

/// Rewrites `remote_url` into a signed proxy link for serving by
/// [`router`], following Pleroma's `MediaProxy.url/1`
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

/// How [`decode`] can refuse a proxy link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyError {
    /// The signature does not verify (or is not base64url at all).
    InvalidSignature,
    /// The payload decodes to something that is not an `http(s)` URL.
    /// Only reachable when a signature was minted with the real secret
    /// over a bad URL; it never occurs for links [`proxy_url`] builds.
    InvalidUrl,
    /// The media proxy is disabled (or has no secret), so no link can
    /// carry a valid signature.
    Disabled,
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSignature => f.write_str("media proxy signature does not verify"),
            Self::InvalidUrl => f.write_str("media proxy payload is not an http(s) URL"),
            Self::Disabled => f.write_str("the media proxy is disabled"),
        }
    }
}

impl Error for ProxyError {}

/// A disabled proxy is indistinguishable from an unrouted path (Pleroma
/// answers its `{:enabled, false}` arm with a 404), and an unverifiable
/// link is the ticket's 403 `{"error":"Invalid signature"}`.
impl From<ProxyError> for AppError {
    fn from(err: ProxyError) -> Self {
        match err {
            ProxyError::InvalidSignature => Self::Forbidden(INVALID_SIGNATURE_MESSAGE.to_owned()),
            ProxyError::InvalidUrl => Self::BadRequest("Invalid media url".to_owned()),
            ProxyError::Disabled => Self::NotFound,
        }
    }
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

/// The `/proxy` router, merged at the top level by [`crate::app`]: the
/// signature path serves the media, and the preview path
/// (`/proxy/preview/…`) serves exactly the same bytes — no resizing, per
/// the ticket.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/proxy/{sig}/{url}", get(serve_proxy))
        .route("/proxy/{sig}/{url}/{filename}", get(serve_proxy_named))
        .route("/proxy/preview/{sig}/{url}", get(serve_proxy))
        .route(
            "/proxy/preview/{sig}/{url}/{filename}",
            get(serve_proxy_named),
        )
}

/// `GET /proxy/{sig}/{url}` and its preview twin.
async fn serve_proxy(
    State(state): State<AppState>,
    Path((sig, encoded)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    fetch_proxied(&state, &sig, &encoded, &headers).await
}

/// The same route carrying the cosmetic `/{filename}` suffix; the
/// filename never influences what is served (Pleroma redirects on a
/// mismatch, this route simply ignores it).
async fn serve_proxy_named(
    State(state): State<AppState>,
    Path((sig, encoded, _filename)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    fetch_proxied(&state, &sig, &encoded, &headers).await
}

/// Validate a link and fetch its bytes: [`decode`] first (a bad signature
/// is the 403), then only the range/validator request headers are
/// forwarded upstream, and the body is capped at
/// `MEDIA_PROXY_MAX_BYTES` — the fetch aborts beyond it. A URL whose
/// upstream fetch completed is recorded in the admin-visible cache ledger
/// (WBE-208, see [`crate::models::media_proxy_cache`]) before the answer
/// is mapped.
async fn fetch_proxied(
    state: &AppState,
    sig: &str,
    encoded: &str,
    request_headers: &HeaderMap,
) -> Result<Response, AppError> {
    let url = decode(&state.config, sig, encoded)?;
    let forwarded = forwarded_headers(request_headers);
    match state
        .http_client
        .get_limited(&url, &forwarded, state.config.media_proxy.max_bytes)
        .await
    {
        Ok(upstream) => {
            record_served(state, &url).await;
            upstream_response(&upstream)
        }
        Err(HttpError::TooLarge) => {
            warn!(%url, "media proxy: upstream body exceeds the byte cap");
            Err(AppError::PayloadTooLarge(TOO_LARGE_MESSAGE.to_owned()))
        }
        Err(err) => {
            error!(%err, %url, "media proxy: upstream fetch failed");
            Err(AppError::UpstreamFailure(UPSTREAM_MESSAGE.to_owned()))
        }
    }
}

/// Add `url` to the media-proxy cache ledger and prune the table back to
/// `MEDIA_PROXY_CACHE_LIMIT` rows (see
/// [`record`](crate::models::media_proxy_cache::record)). The ledger is a
/// side note to the response it just served: a database failure is logged
/// and swallowed, never a 500 over a byte that arrived fine.
async fn record_served(state: &AppState, url: &str) {
    if let Err(err) =
        media_proxy_cache::record(&state.db, url, state.config.media_proxy.cache_limit).await
    {
        warn!(%err, %url, "media proxy: recording the served url failed");
    }
}

/// Map an upstream status: media and partial content are served, `304`
/// and `404` keep their status (the ticket's "same status"), and
/// anything else — upstream `5xx`, odd `3xx`, other `4xx` — becomes a
/// `502` (the ticket's "or 502").
fn upstream_response(upstream: &HttpResponse) -> Result<Response, AppError> {
    match upstream.status {
        200 | 206 => media_response(upstream),
        304 => not_modified_response(upstream),
        404 => Err(AppError::NotFound),
        status => {
            warn!(status, "media proxy: upstream answered unusably");
            Err(AppError::UpstreamFailure(UPSTREAM_MESSAGE.to_owned()))
        }
    }
}

/// The request headers worth forwarding (see
/// [`FORWARDED_REQUEST_HEADERS`]); absent or non-text values are dropped.
fn forwarded_headers(headers: &HeaderMap) -> Vec<(&str, &str)> {
    FORWARDED_REQUEST_HEADERS
        .iter()
        .filter_map(|name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| (name.as_str(), value))
        })
        .collect()
}

/// Serve a `200`/`206` upstream body as proxy media: only media types
/// pass (anything else is the ticket's 424), and the safety headers —
/// forced `inline`, `nosniff`, a day of caching and the strict CSP — are
/// applied over whatever the upstream claimed.
fn media_response(upstream: &HttpResponse) -> Result<Response, AppError> {
    let content_type = header_text(&upstream.headers, &header::CONTENT_TYPE);
    if !is_media_type(&content_type) {
        return Err(AppError::FailedDependency(UNSUPPORTED_MESSAGE.to_owned()));
    }
    let response = Response::builder()
        .status(upstream.status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_DISPOSITION, INLINE)
        .header(header::CACHE_CONTROL, CACHE_CONTROL)
        .header(header::X_CONTENT_TYPE_OPTIONS, NOSNIFF)
        .header(CSP_HEADER.clone(), CSP);
    let response = copy_validator_headers(response, &upstream.headers);
    response
        .body(Body::from(upstream.body.clone()))
        .map_err(|err| AppError::Internal(format!("media proxy response build failed: {err}")))
}

/// Pass an upstream `304` through: no body and no type, just the
/// validators the client revalidated with plus the cache/security
/// headers.
fn not_modified_response(upstream: &HttpResponse) -> Result<Response, AppError> {
    let response = Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(header::CACHE_CONTROL, CACHE_CONTROL)
        .header(header::X_CONTENT_TYPE_OPTIONS, NOSNIFF)
        .header(CSP_HEADER.clone(), CSP);
    let response = copy_validator_headers(response, &upstream.headers);
    response
        .body(Body::empty())
        .map_err(|err| AppError::Internal(format!("media proxy response build failed: {err}")))
}

/// Copy the upstream's `ETag`/`Last-Modified`/`Content-Range` onto a
/// response under construction.
fn copy_validator_headers(
    builder: axum::http::response::Builder,
    headers: &HeaderMap,
) -> axum::http::response::Builder {
    VALIDATOR_HEADERS.iter().fold(builder, |mut builder, name| {
        if let Some(value) = headers.get(name) {
            builder = builder.header(name.clone(), value.clone());
        }
        builder
    })
}

/// Whether a `Content-Type` names media this route may serve inline: the
/// top-level type is `image/…`, `video/…` or `audio/…` (case-insensitive,
/// parameters stripped).
fn is_media_type(content_type: &str) -> bool {
    let top_level = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    top_level.starts_with("image/")
        || top_level.starts_with("video/")
        || top_level.starts_with("audio/")
}

/// The visible text of header `name`, empty when absent or not text.
fn header_text(headers: &HeaderMap, name: &HeaderName) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use axum::{
        http::HeaderValue,
        response::{IntoResponse as _, Response},
    };

    use super::*;
    use crate::config::{
        BASE_URL_VAR, DATABASE_URL_VAR, MEDIA_BASE_URL_VAR, MEDIA_PROXY_BASE_URL_VAR,
        MEDIA_PROXY_BLACKLIST_VAR, MEDIA_PROXY_CACHE_LIMIT_VAR, MEDIA_PROXY_ENABLED_VAR,
        MEDIA_PROXY_MAX_BYTES_VAR, MEDIA_PROXY_PURGE_URL_VAR, MEDIA_PROXY_SECRET_VAR,
        TEST_BASE_URL, TEST_DATABASE_URL,
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

    /// An [`HttpResponse`] for the response-shaping helpers.
    fn upstream(status: u16, headers: &[(&str, &str)], body: &[u8]) -> HttpResponse {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::try_from(*name).unwrap();
            drop(map.insert(name, HeaderValue::from_str(value).unwrap()));
        }
        HttpResponse {
            status,
            headers: map,
            body: bytes::Bytes::copy_from_slice(body),
        }
    }

    /// Renders a handler error the way axum would.
    fn render(err: AppError) -> Response {
        err.into_response()
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
        let config =
            enabled_config(&[(MEDIA_PROXY_BLACKLIST_VAR, "Evil.Example, blocked.example")]);
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
    fn only_media_top_level_types_are_served() {
        for (content_type, media) in [
            ("image/png", true),
            ("VIDEO/MP4; codecs=avc1", true),
            (" audio/ogg ", true),
            ("text/html", false),
            ("application/json", false),
            ("", false),
            ("image", false),
        ] {
            assert_eq!(
                is_media_type(content_type),
                media,
                "{content_type:?} proxiable: {media}"
            );
        }
    }

    #[test]
    fn media_response_forces_inline_and_the_safety_headers() {
        let up = upstream(
            200,
            &[
                ("content-type", "image/png"),
                ("etag", "\"v1\""),
                ("last-modified", "Sun, 01 Jun 2025 00:00:00 GMT"),
                ("content-disposition", "attachment; filename=\"x\""),
            ],
            b"fake-png",
        );
        let response = media_response(&up).unwrap();
        assert_eq!(response.status(), StatusCode::OK, "upstream status kept");
        let headers = response.headers();
        assert_eq!(headers[header::CONTENT_TYPE], "image/png", "type passes");
        assert_eq!(
            headers[header::CONTENT_DISPOSITION],
            INLINE,
            "inline is forced over the upstream attachment"
        );
        assert_eq!(
            headers[header::CACHE_CONTROL],
            CACHE_CONTROL,
            "one day of cache"
        );
        assert_eq!(
            headers[header::X_CONTENT_TYPE_OPTIONS],
            NOSNIFF,
            "no sniffing"
        );
        assert_eq!(headers[&CSP_HEADER], CSP, "the locked-down CSP");
        assert_eq!(headers[header::ETAG], "\"v1\"", "validators ride along");
        assert_eq!(
            headers[header::LAST_MODIFIED],
            "Sun, 01 Jun 2025 00:00:00 GMT",
            "last-modified passes"
        );
    }

    #[test]
    fn media_response_passes_partial_content_through() {
        let up = upstream(
            206,
            &[
                ("content-type", "video/webm"),
                ("content-range", "bytes 0-3/10"),
            ],
            b"webm",
        );
        let response = media_response(&up).unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "206 kept");
        assert_eq!(
            response.headers()[header::CONTENT_RANGE],
            "bytes 0-3/10",
            "the range answer passes through"
        );
    }

    #[test]
    fn non_media_types_are_answered_424() {
        for content_type in ["text/html", "application/octet-stream", ""] {
            let up = upstream(200, &[("content-type", content_type)], b"x");
            let err = media_response(&up).unwrap_err();
            assert!(
                matches!(err, AppError::FailedDependency(_)),
                "`{content_type:?}` is not media, got {err:?}"
            );
            assert_eq!(
                err.to_string(),
                UNSUPPORTED_MESSAGE,
                "the body message is the ticket's"
            );
            assert_eq!(
                render(err).status(),
                StatusCode::FAILED_DEPENDENCY,
                "`{content_type:?}` answers 424"
            );
        }
    }

    #[test]
    fn not_modified_responses_keep_the_validators_without_a_type() {
        let up = upstream(304, &[("etag", "\"v1\"")], b"");
        let response = not_modified_response(&up).unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "304 kept");
        assert_eq!(
            response.headers().get(header::ETAG),
            Some(&HeaderValue::from_static("\"v1\"")),
            "the revalidated etag comes back"
        );
        assert!(
            !response.headers().contains_key(header::CONTENT_TYPE),
            "a 304 carries no content type"
        );
    }

    #[test]
    fn upstream_statuses_map_per_the_ticket() {
        let cases: [(u16, StatusCode); 5] = [
            (404, StatusCode::NOT_FOUND),
            (500, StatusCode::BAD_GATEWAY),
            (503, StatusCode::BAD_GATEWAY),
            (418, StatusCode::BAD_GATEWAY),
            (302, StatusCode::BAD_GATEWAY),
        ];
        for (status, want) in cases {
            let up = upstream(status, &[("content-type", "image/png")], b"");
            let rendered = match upstream_response(&up) {
                Ok(response) => response,
                Err(err) => render(err),
            };
            assert_eq!(rendered.status(), want, "upstream {status} becomes {want}");
        }
    }

    #[test]
    fn only_range_and_validator_request_headers_are_forwarded() {
        let mut headers = HeaderMap::new();
        drop(headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-1")));
        drop(headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"v1\"")));
        drop(headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        ));
        drop(headers.insert(header::COOKIE, HeaderValue::from_static("session=x")));
        assert_eq!(
            forwarded_headers(&headers),
            vec![("range", "bytes=0-1"), ("if-none-match", "\"v1\"")],
            "credentials never reach the upstream, range and revalidation do"
        );
    }

    #[test]
    fn proxy_error_display_and_http_mapping() {
        assert_eq!(
            ProxyError::InvalidSignature.to_string(),
            "media proxy signature does not verify",
            "Display is log-facing"
        );
        assert!(ProxyError::Disabled.source().is_none(), "no wrapped source");
        let forbidden = render(AppError::from(ProxyError::InvalidSignature));
        assert_eq!(
            forbidden.status(),
            StatusCode::FORBIDDEN,
            "a bad signature is a 403"
        );
        assert!(
            matches!(
                AppError::from(ProxyError::InvalidSignature),
                AppError::Forbidden(message) if message == INVALID_SIGNATURE_MESSAGE
            ),
            "with exactly the body message the ticket names"
        );
        assert!(
            matches!(AppError::from(ProxyError::Disabled), AppError::NotFound),
            "a disabled proxy is a 404 like any unrouted path"
        );
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
        let target =
            purge_target(&config, REMOTE).expect("an enabled proxy with a purge url purges");
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
}
