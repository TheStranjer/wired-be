//! The `/proxy` route: fetching a validated link's bytes and answering with
//! the safety headers.

use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderName, StatusCode, header},
    response::Response,
    routing::get,
};
use tracing::{error, warn};

use super::{TOO_LARGE_MESSAGE, UNSUPPORTED_MESSAGE, UPSTREAM_MESSAGE, decode};
use crate::{
    error::AppError,
    http_client::{HttpError, HttpResponse},
    models::media_proxy_cache,
    state::AppState,
    util::{
        headers::NOSNIFF,
        media_type::{essence, top_level},
    },
};

/// `cache-control` for proxied media: a day, per the ticket (Pleroma's
/// proxy default is a fortnight, `@default_cache_control_header` in
/// `lib/pleroma/reverse_proxy.ex`).
const CACHE_CONTROL: &str = "public, max-age=86400";
/// Proxied media renders in the browser; nothing downloads as an
/// attachment from this route.
const INLINE: &str = "inline";
/// `content-security-policy` for proxy responses, verbatim from the
/// ticket: no scripts, only inline styles and same-origin media and
/// images — in the spirit of Pleroma's `HttpSecurityPlug` media
/// policy (`lib/pleroma/web/plugs/http_security_plug.ex`).
const CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; media-src 'self' data:; img-src 'self' data:";
/// The raster image types served inline. Everything else under `image/`
/// (notably `image/svg+xml`, which can run script) is refused.
const RASTER_IMAGE_TYPES: [&str; 16] = [
    "image/png",
    "image/apng",
    "image/jpeg",
    "image/jpg",
    "image/pjpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "image/bmp",
    "image/heic",
    "image/heif",
    "image/tiff",
    "image/jxl",
    "image/x-icon",
    "image/vnd.microsoft.icon",
    "image/x-ms-bmp",
];
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
        .header(header::X_CONTENT_TYPE_OPTIONS, NOSNIFF.clone())
        .header(header::CONTENT_SECURITY_POLICY, CSP);
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
        .header(header::X_CONTENT_TYPE_OPTIONS, NOSNIFF.clone())
        .header(header::CONTENT_SECURITY_POLICY, CSP);
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

/// Whether a `Content-Type` names media this route may serve inline.
///
/// Images must be one of [`RASTER_IMAGE_TYPES`], so `image/svg+xml` (which
/// can carry script and would run on the instance origin) and any XML,
/// JSON or HTML type under an `image/` prefix are refused. Video and audio
/// pass unless the subtype is a structured-syntax (`+xml`, `+json`, …) or
/// markup type. Case-insensitive, parameters stripped.
fn is_media_type(content_type: &str) -> bool {
    let media_type = essence(content_type);
    match top_level(&media_type) {
        Some("image") => RASTER_IMAGE_TYPES.contains(&media_type.as_str()),
        Some(_) => media_type.split_once('/').is_some_and(|(_, subtype)| {
            !subtype.is_empty()
                && !subtype.contains('+')
                && !subtype.contains("html")
                && !subtype.contains("xml")
                && !subtype.contains("svg")
        }),
        None => false,
    }
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
mod tests;
