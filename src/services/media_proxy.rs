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

mod links;
mod serve;

use std::{error::Error, fmt};

use crate::error::AppError;

pub use self::{
    links::{decode, proxy_url, purge_target},
    serve::router,
};

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

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, response::IntoResponse as _};

    use super::*;

    #[test]
    fn proxy_error_display_and_http_mapping() {
        assert_eq!(
            ProxyError::InvalidSignature.to_string(),
            "media proxy signature does not verify",
            "Display is log-facing"
        );
        assert!(ProxyError::Disabled.source().is_none(), "no wrapped source");
        let forbidden = AppError::from(ProxyError::InvalidSignature).into_response();
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
}
