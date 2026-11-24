//! The HTTP middleware stack every request passes through.
//!
//! [`apply`] wraps a router, outermost first, in: request-id assignment
//! (`x-request-id`, kept from the client or generated as a UUID), `tracing`
//! logging of each request/response, CORS following Pleroma's `cors_plug`,
//! a 30 s request timeout answered with 408, and a 16 MiB request body limit
//! answered with 413. Logging never emits the `Authorization` header, the
//! value of a secret query parameter (`access_token`, `mfa_token`, `code`,
//! `state`, `token`), or the secret segment of a password-reset,
//! or email-confirmation path.

use std::{borrow::Cow, time::Duration};

use axum::{
    Router,
    http::{HeaderName, Method, Request, StatusCode, Uri, header},
};
use tower_http::{
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    timeout::TimeoutLayer,
    trace::{DefaultOnRequest, DefaultOnResponse, TraceLayer},
};
use tracing::{Level, Span, info_span};

/// Maximum accepted request body size, 16 MiB. Upload endpoints will get
/// their own, larger, limit in a later ticket.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// How long a request may run before the server answers 408 Request Timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long clients may cache a CORS preflight answer, the 86400 seconds of
/// Pleroma's `cors_plug`.
pub const CORS_MAX_AGE: Duration = Duration::from_hours(24);

/// Query parameters whose value must never reach the log: the bearer token
/// the streaming API accepts, the MFA challenge token, and the OAuth callback
/// `code` and `state`, plus any generic `token`.
const REDACTED_PARAMS: [&str; 5] = ["access_token", "mfa_token", "code", "state", "token"];

/// Route prefixes whose path carries a secret, each with whether the first
/// segment after the prefix (a user id) is kept and only the rest redacted.
const SECRET_PATH_PREFIXES: [(&str, bool); 2] = [
    ("/api/v1/pleroma/password_reset/", false),
    ("/api/account/confirm_email/", true),
];

/// Stands in for a redacted value in logged URIs.
const REDACTED: &str = "[redacted]";

/// Wraps `router` in the middleware stack described in the [module docs].
///
/// [module docs]: self
pub fn apply(router: Router) -> Router {
    router
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(cors_layer())
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(
            // The default message level is DEBUG, below the server's default
            // `info` filter, so raise request/response lines to INFO.
            TraceLayer::new_for_http()
                .make_span_with(http_span)
                .on_request(DefaultOnRequest::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
}

/// The CORS policy of `docs/ARCHITECTURE.md`: any origin may call the API
/// with the Mastodon client methods and headers, and clients may read the
/// pagination and rate limit response headers.
fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static("idempotency-key"),
            header::ACCEPT,
        ])
        .expose_headers([
            header::LINK,
            HeaderName::from_static("x-ratelimit-limit"),
            HeaderName::from_static("x-ratelimit-remaining"),
            HeaderName::from_static("x-ratelimit-reset"),
        ])
        .max_age(CORS_MAX_AGE)
}

/// The span every `tracing` log line of a request is recorded in: method plus
/// request target, with the target sanitized (see [`sanitized_uri`]). No
/// headers are logged, so the `Authorization` header can never leak.
fn http_span<B>(request: &Request<B>) -> Span {
    info_span!(
        "http_request",
        method = %request.method(),
        uri = %sanitized_uri(request.uri()),
    )
}

/// The request target fit for the log: the path with any secret segment
/// replaced (see [`redact_path`]), plus the query string with the value of
/// every secret parameter replaced by [`REDACTED`] (see [`redact_query`]).
fn sanitized_uri(uri: &Uri) -> String {
    let path = redact_path(uri.path());
    uri.query().map_or_else(
        || path.to_string(),
        |query| format!("{path}?{}", redact_query(query)),
    )
}

/// The path with the secret segment of each secret-bearing route replaced by
/// [`REDACTED`]: everything after the password-reset prefix, and the token (last segment) after the email-confirmation prefix, whose
/// leading user id is kept. Any other path is returned as is.
fn redact_path(path: &str) -> Cow<'_, str> {
    for (prefix, keeps_leading_segment) in SECRET_PATH_PREFIXES {
        let Some(rest) = path.strip_prefix(prefix) else {
            continue;
        };
        if rest.is_empty() {
            return Cow::Borrowed(path);
        }
        let kept = if keeps_leading_segment {
            rest.rfind('/')
                .and_then(|slash| rest.get(..=slash))
                .unwrap_or("")
        } else {
            ""
        };
        return Cow::Owned(format!("{prefix}{kept}{REDACTED}"));
    }
    Cow::Borrowed(path)
}

/// The query string with the value of each parameter in [`REDACTED_PARAMS`]
/// replaced by [`REDACTED`]; every other parameter is kept as sent.
fn redact_query(query: &str) -> String {
    query
        .split('&')
        .map(|param| match param.split_once('=') {
            Some((name, _value)) if REDACTED_PARAMS.contains(&name) => {
                format!("{name}={REDACTED}")
            }
            _ => param.to_owned(),
        })
        .collect::<Vec<String>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    use axum::{
        body::{Body, Bytes},
        http::header::CONTENT_TYPE,
        routing::{get, post},
    };
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use super::*;

    /// A token value that must never appear in a log line.
    const SECRET: &str = "sup3rsecrettoken";

    /// The stack over stub routes: a no-content GET and an echoing POST.
    fn test_app() -> Router {
        apply(
            Router::new()
                .route("/health", get(health_stub))
                .route("/upload", post(upload_stub)),
        )
    }

    /// Stub standing in for `/health`.
    async fn health_stub() -> StatusCode {
        StatusCode::NO_CONTENT
    }

    /// Stub consuming the request body: 200 when any arrived, 204 when empty.
    async fn upload_stub(body: Bytes) -> StatusCode {
        if body.is_empty() {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::OK
        }
    }

    /// A `MakeWriter` collecting every log line into a shared buffer.
    #[derive(Clone, Default, Debug)]
    struct LogBuffer(
        /// The bytes logged so far, shared with every writer clone.
        Arc<Mutex<Vec<u8>>>,
    );

    impl LogBuffer {
        /// The bytes logged so far, as a string.
        fn contents(&self) -> String {
            let Ok(got) = self.0.lock() else {
                return String::new();
            };
            String::from_utf8_lossy(&got).into_owned()
        }
    }

    impl io::Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let Ok(mut got) = self.0.lock() else {
                return Err(io::Error::other("log buffer poisoned"));
            };
            got.extend_from_slice(buf);
            drop(got);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'w> tracing_subscriber::fmt::MakeWriter<'w> for LogBuffer {
        type Writer = Self;

        fn make_writer(&'w self) -> Self::Writer {
            Self(Arc::clone(&self.0))
        }
    }

    #[tokio::test]
    async fn preflight_gets_cors_headers() {
        let response = test_app()
            .oneshot(
                Request::options("/anything")
                    .header("origin", "https://client.example")
                    .header("access-control-request-method", "POST")
                    .header(
                        "access-control-request-headers",
                        "authorization,content-type",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "preflight status");
        let headers = response.headers();
        assert_eq!(
            headers["access-control-allow-origin"], "*",
            "any origin is allowed"
        );
        assert_eq!(
            headers["access-control-allow-methods"], "GET,POST,PUT,PATCH,DELETE,OPTIONS",
            "the client API methods"
        );
        assert_eq!(
            headers["access-control-allow-headers"],
            "authorization,content-type,idempotency-key,accept",
            "the client API request headers"
        );
        assert_eq!(
            headers["access-control-max-age"], "86400",
            "preflight cache"
        );
    }

    #[tokio::test]
    async fn simple_request_gets_cors_and_exposed_headers() {
        let response = test_app()
            .oneshot(
                Request::get("/health")
                    .header("origin", "https://client.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "health status");
        let headers = response.headers();
        assert_eq!(headers["access-control-allow-origin"], "*", "any origin");
        assert_eq!(
            headers["access-control-expose-headers"],
            "link,x-ratelimit-limit,x-ratelimit-remaining,x-ratelimit-reset",
            "pagination and rate limit headers are readable"
        );
    }

    #[tokio::test]
    async fn response_carries_a_generated_request_id() {
        let response = test_app()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "health status");
        let got = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let parsed = Uuid::parse_str(&got).unwrap();
        assert!(!parsed.is_nil(), "a fresh UUID: {got}");
    }

    #[tokio::test]
    async fn incoming_request_id_is_propagated_to_the_response() {
        let response = test_app()
            .oneshot(
                Request::get("/health")
                    .header("x-request-id", "trace-me-please")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers()["x-request-id"],
            "trace-me-please",
            "the client's id is kept, not replaced"
        );
    }

    #[tokio::test]
    async fn oversized_body_is_rejected_with_413() {
        let oversized = vec![b'x'; MAX_BODY_BYTES + 1];
        let response = test_app()
            .oneshot(
                Request::post("/upload")
                    .header(CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from(oversized))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "one byte over the limit"
        );
    }

    #[tokio::test]
    async fn body_within_the_limit_is_accepted() {
        let response = test_app()
            .oneshot(
                Request::post("/upload")
                    .header(CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from("small body"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "within the limit");
    }

    #[tokio::test(start_paused = true)]
    async fn slow_handler_times_out_with_408() {
        /// A handler that answers long after [`REQUEST_TIMEOUT`].
        async fn forever() -> StatusCode {
            tokio::time::sleep(REQUEST_TIMEOUT * 2).await;
            StatusCode::OK
        }

        let app = apply(Router::new().route("/slow", get(forever)));
        let response = app
            .oneshot(Request::get("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::REQUEST_TIMEOUT,
            "a stalled handler is cut off"
        );
    }

    #[tokio::test]
    async fn logs_never_contain_secrets() {
        /// The request that is logged and then inspected.
        fn secret_request() -> Request<Body> {
            Request::get(format!("/health?access_token={SECRET}&lang=en").as_str())
                .header("authorization", format!("Bearer {SECRET}"))
                .body(Body::empty())
                .unwrap()
        }

        /// A password-reset link request, whose path carries the token.
        fn reset_request() -> Request<Body> {
            Request::get(format!("/api/v1/pleroma/password_reset/{SECRET}").as_str())
                .body(Body::empty())
                .unwrap()
        }

        /// A request whose query carries an MFA challenge token.
        fn mfa_request() -> Request<Body> {
            Request::get(format!("/oauth/mfa?mfa_token={SECRET}&lang=en").as_str())
                .body(Body::empty())
                .unwrap()
        }

        let buffer = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(buffer.clone())
            .finish();
        let guard: tracing::subscriber::DefaultGuard = tracing::subscriber::set_default(subscriber);

        // `tracing` caches per-callsite interest process-wide, so logging
        // call sites first reached while other tests ran (without a thread
        // subscriber of their own) may already be cached as "never". Send one
        // request to register any call site this test will use, then rebuild
        // the cache so every registered call site is evaluated against this
        // thread's capturing subscriber before the real request runs.
        drop(test_app().oneshot(secret_request()).await.unwrap());
        tracing_core::callsite::rebuild_interest_cache();

        drop(test_app().oneshot(secret_request()).await.unwrap());
        drop(test_app().oneshot(reset_request()).await.unwrap());
        drop(test_app().oneshot(mfa_request()).await.unwrap());
        drop(guard);

        let logged = buffer.contents();
        assert!(
            !logged.is_empty(),
            "the request was logged at all: {logged}"
        );
        assert!(
            !logged.contains(SECRET),
            "no token value in the log: {logged}"
        );
        assert!(
            logged.contains(&format!("access_token={REDACTED}")),
            "the redacted placeholder stands in: {logged}"
        );
        assert!(logged.contains("lang=en"), "harmless params stay: {logged}");
        assert!(
            logged.contains(&format!("/api/v1/pleroma/password_reset/{REDACTED}")),
            "the reset path is redacted: {logged}"
        );
        assert!(
            logged.contains(&format!("mfa_token={REDACTED}")),
            "the MFA token is redacted: {logged}"
        );
    }

    #[test]
    fn password_reset_token_path_is_redacted() {
        let uri = Uri::from_static("/api/v1/pleroma/password_reset/abc");
        assert_eq!(
            sanitized_uri(&uri),
            "/api/v1/pleroma/password_reset/[redacted]"
        );
    }

    #[test]
    fn confirm_email_token_is_redacted_and_user_id_kept() {
        let id = Uuid::nil();
        let path = format!("/api/account/confirm_email/{id}/tok");
        let uri = Uri::try_from(path.as_str()).unwrap();
        assert_eq!(
            sanitized_uri(&uri),
            format!("/api/account/confirm_email/{id}/[redacted]")
        );
    }

    #[test]
    fn secret_prefix_without_a_segment_is_left_as_is() {
        assert_eq!(
            redact_path("/api/v1/pleroma/password_reset/"),
            "/api/v1/pleroma/password_reset/",
            "nothing to redact"
        );
        assert!(
            matches!(redact_path("/api/v1/timelines/public"), Cow::Borrowed(_)),
            "an unrelated path is not copied"
        );
    }

    #[test]
    fn mfa_token_is_redacted_and_other_params_kept() {
        let uri = Uri::from_static("/oauth/mfa?mfa_token=x&redirect_uri=y");
        assert_eq!(
            sanitized_uri(&uri),
            "/oauth/mfa?mfa_token=[redacted]&redirect_uri=y"
        );
    }

    #[test]
    fn oauth_callback_code_and_state_are_redacted() {
        let uri = Uri::from_static("/oauth/github/callback?code=c&state=s");
        assert_eq!(
            sanitized_uri(&uri),
            "/oauth/github/callback?code=[redacted]&state=[redacted]"
        );
    }

    #[test]
    fn generic_token_param_is_redacted() {
        let uri = Uri::from_static("/x?token=t&other=o");
        assert_eq!(sanitized_uri(&uri), "/x?token=[redacted]&other=o");
    }

    #[test]
    fn uri_without_query_logs_the_path_only() {
        let uri = Uri::from_static("/api/v1/timelines/public");
        assert_eq!(sanitized_uri(&uri), "/api/v1/timelines/public");
    }

    #[test]
    fn query_without_token_is_kept_verbatim() {
        let uri = Uri::from_static("/api/v1/timelines/public?local=true&only_media=false");
        assert_eq!(
            sanitized_uri(&uri),
            "/api/v1/timelines/public?local=true&only_media=false"
        );
    }

    #[test]
    fn access_token_value_is_redacted_around_other_params() {
        let uri = Uri::from_static("/api/v2/stream?access_token=hunter2&reply=1");
        assert_eq!(
            sanitized_uri(&uri),
            format!("/api/v2/stream?access_token={REDACTED}&reply=1")
        );
    }

    #[test]
    fn token_param_without_value_is_left_as_is() {
        let uri = Uri::from_static("/health?access_token");
        assert_eq!(sanitized_uri(&uri), "/health?access_token");
    }

    #[test]
    fn redaction_only_matches_the_exact_param_name() {
        let uri = Uri::from_static("/x?not_access_token=visible&access_token_x=also");
        assert_eq!(
            sanitized_uri(&uri),
            "/x?not_access_token=visible&access_token_x=also"
        );
    }
}
