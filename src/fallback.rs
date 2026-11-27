//! The liveness probe and the router-level fallback [`app`](crate::app)
//! mounts.
//!
//! A JSON 404 for every unmatched path (decision 8), marked with the
//! `x-wired-fallback` header the route-inventory conformance test reads,
//! answering Pleroma's `api_not_implemented` below `/api` and the plain
//! "Not found" everywhere else.

use axum::{
    Router,
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, uri::PathAndQuery},
    response::{IntoResponse as _, Response},
};
use tower::ServiceExt as _;

use crate::error::AppError;

/// The path prefix Pleroma's `LegacyPleromaApiRerouterPlug` rewrites.
const LEGACY_PLEROMA_PREFIX: &str = "/api/pleroma/";

/// What an unmatched `/api/pleroma/<rest>` is re-dispatched as.
const V1_PLEROMA_PREFIX: &str = "/api/v1/pleroma/";

/// Liveness probe.
pub(crate) async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// The header every [`not_found`] answer carries, naming the router-level
/// fallback as the responder: the route-inventory conformance test
/// (`tests/api/route_inventory.rs`) sends one credential-less request per
/// row of
/// [`api::route_inventory::routes`](crate::api::route_inventory::routes)
/// and reads this header off the
/// answer to tell "no route matched" apart from a real handler that
/// answered with its own 404.
const FALLBACK_HEADER: HeaderName = HeaderName::from_static("x-wired-fallback");

/// [`FALLBACK_HEADER`]'s value: the header is a flag, so it only says "yes".
const FALLBACK_HEADER_VALUE: &str = "1";

/// The header map [`not_found`] hands its [`AppError`]: just the marker.
fn fallback_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.extend([(
        FALLBACK_HEADER,
        HeaderValue::from_static(FALLBACK_HEADER_VALUE),
    )]);
    headers
}

/// Fallback handler for every unmatched path, answered as a JSON 404
/// (decision 8). Pleroma's router-level fallback distinguishes unknown
/// endpoints under `/api` — 404 "Not implemented"
/// (`lib/pleroma/web/fallback/redirect_controller.ex`, `api_not_implemented`)
/// — from every other unknown path, 404 "Not found"; both are produced here
/// as [`AppError`]s, and both carry [`FALLBACK_HEADER`]. Known paths called
/// with the wrong method never reach this handler, so they keep axum's
/// default 405.
pub(crate) async fn not_found(uri: Uri) -> (HeaderMap, AppError) {
    let error = error_for_unmatched(uri.path());
    (fallback_headers(), error)
}

/// `uri` with a leading `/api/pleroma/` swapped for `/api/v1/pleroma/`, the
/// query kept; `None` when the path is not under that prefix or the result
/// is not a valid URI.
fn legacy_pleroma_target(uri: &Uri) -> Option<Uri> {
    let rest = uri.path().strip_prefix(LEGACY_PLEROMA_PREFIX)?;
    let rewritten = uri.query().map_or_else(
        || format!("{V1_PLEROMA_PREFIX}{rest}"),
        |query| format!("{V1_PLEROMA_PREFIX}{rest}?{query}"),
    );
    let path_and_query = rewritten.parse::<PathAndQuery>().ok()?;
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(path_and_query);
    Uri::from_parts(parts).ok()
}

/// The app-wide fallback: Pleroma's `LegacyPleromaApiRerouterPlug`, then [`not_found`].
///
/// A request under `/api/pleroma/` that no route matched is handed, with its
/// method, headers and body, to `routes` (the whole router, whose own
/// fallback is [`not_found`]) as the same request to `/api/v1/pleroma/`.
/// Everything else gets [`not_found`] directly.
pub(crate) async fn reroute_legacy_pleroma(routes: Router, mut request: Request) -> Response {
    let Some(target) = legacy_pleroma_target(request.uri()) else {
        return not_found(request.uri().clone()).await.into_response();
    };
    *request.uri_mut() = target;
    routes
        .oneshot(request)
        .await
        .unwrap_or_else(|never| match never {})
}

/// Which 404 an unmatched `path` answers with: Pleroma's
/// `api_not_implemented` below `/api`, the plain "Not found" everywhere else.
fn error_for_unmatched(path: &str) -> AppError {
    if path == "/api" || path.starts_with("/api/") {
        AppError::NotImplemented
    } else {
        AppError::RouteNotFound
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request};
    use http_body_util::BodyExt as _;

    use crate::{app, config::Config, state::AppState};

    /// A config with only the required variables set, for tests that don't
    /// exercise config.
    fn test_config() -> Config {
        Config::for_tests()
    }

    /// Router backed by a pool that never connects, for routes that don't touch the DB.
    fn test_app() -> Router {
        app(AppState::lazy_for_tests(test_config()))
    }

    #[tokio::test]
    async fn health_returns_no_content() {
        let response = test_app()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "health status");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(body.is_empty(), "health body should be empty");
    }

    /// A matched route is not the fallback: `/health` answers without the
    /// `x-wired-fallback` marker the inventory test reads.
    #[tokio::test]
    async fn matched_route_carries_no_fallback_marker() {
        let response = test_app()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(FALLBACK_HEADER),
            None,
            "a route the router matched is not the fallback"
        );
    }

    /// The two 404s of the fallback are chosen by the path alone: below
    /// `/api` Pleroma's `api_not_implemented`, everywhere else "Not found",
    /// and a prefix that only looks like `/api` is not below it.
    #[test]
    fn unmatched_paths_choose_their_error() {
        assert!(
            matches!(
                error_for_unmatched("/api/v1/nothing"),
                AppError::NotImplemented
            ),
            "a path below /api is api_not_implemented"
        );
        assert!(
            matches!(error_for_unmatched("/api"), AppError::NotImplemented),
            "the bare /api prefix is api_not_implemented too"
        );
        assert!(
            matches!(error_for_unmatched("/nope"), AppError::RouteNotFound),
            "a path outside /api is the plain 404"
        );
        assert!(
            matches!(error_for_unmatched("/apibot"), AppError::RouteNotFound),
            "an /api lookalike is not below /api/"
        );
    }

    /// Only a path below `/api/pleroma/` is rewritten, and its query survives.
    #[test]
    fn legacy_pleroma_paths_are_rewritten_to_v1() {
        let target = |text: &str| legacy_pleroma_target(&text.parse::<Uri>().unwrap());
        assert_eq!(
            target("/api/pleroma/captcha").map(|uri| uri.to_string()),
            Some("/api/v1/pleroma/captcha".to_owned()),
            "the prefix is swapped"
        );
        assert_eq!(
            target("/api/pleroma/a/b?x=1&y=2").map(|uri| uri.to_string()),
            Some("/api/v1/pleroma/a/b?x=1&y=2".to_owned()),
            "deeper segments and the query are kept"
        );
        assert_eq!(target("/api/pleroma"), None, "the bare prefix is left");
        assert_eq!(target("/api/pleromax/a"), None, "a lookalike is left");
        assert_eq!(target("/api/v1/pleroma/a"), None, "v1 is left");
    }

    #[tokio::test]
    async fn unknown_route_returns_not_found() {
        let response = test_app()
            .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "unknown route status"
        );
        assert_eq!(
            response.headers().get(FALLBACK_HEADER),
            Some(&HeaderValue::from_static(FALLBACK_HEADER_VALUE)),
            "the fallback marks its own answer"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            r#"{"error":"Not found"}"#,
            "unknown non-API route body is the router-level 404"
        );
    }

    #[tokio::test]
    async fn unknown_api_route_returns_not_implemented() {
        let response = test_app()
            .oneshot(Request::get("/api/v1/nothing").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "unrouted API path status"
        );
        assert_eq!(
            response.headers().get(FALLBACK_HEADER),
            Some(&HeaderValue::from_static(FALLBACK_HEADER_VALUE)),
            "the /api fallback marks its own answer too"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            r#"{"error":"Not implemented"}"#,
            "unrouted path under /api is Pleroma's api_not_implemented"
        );
    }
}
