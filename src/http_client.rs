//! The outbound HTTP client every remote call goes through.
//!
//! [`HttpClient`] wraps one pooled `reqwest` client (cloning is cheap) and
//! enforces the rules federation needs: only `http`/`https` URLs, a 5 s
//! connect timeout, a 20 s total timeout that covers the body read, at
//! most 5 redirects, a hard cap on how many body bytes are buffered
//! (via [`HttpClient::get_limited`]) so a hostile peer cannot exhaust
//! this process with an endless response, and — through the [`ssrf`]
//! module — an SSRF guard that refuses targets on loopback, private or
//! otherwise reserved addresses, at every resolved address and at every
//! redirect hop, pinning each connection to a validated address. The
//! guard is on by default and is disabled only by
//! `HTTP_ALLOW_PRIVATE_HOSTS` (see [`crate::config`]).

pub mod ssrf;

use std::{error::Error, fmt, time::Duration};

use bytes::{Bytes, BytesMut};
use chrono::Utc;
use reqwest::{
    Client, Method,
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect,
};
use serde::de::DeserializeOwned;
use tracing::error;
use url::Url;

use crate::{
    config::Config,
    federation::signature::{SignedHeaders, SigningKey, sign_request},
};

pub use ssrf::is_blocked_ip;

/// Time allowed to establish the TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Time allowed for the whole request, response body read included.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
/// Redirects followed before the response is handed back unanswered.
const MAX_REDIRECTS: usize = 5;
/// First status code [`HttpResponse::error_for_status`] reports as an error.
const ERROR_STATUS_MIN: u16 = 400;
/// Product name the instance identifies itself with in `User-Agent`.
const USER_AGENT_NAME: &str = "Wired";
/// Media type federation peers speak: sent as `Content-Type` on signed
/// POSTs and asked for via `Accept` on signed GETs.
const ACTIVITY_JSON: &str = "application/activity+json";

/// Formats the instance `User-Agent` header value:
/// `Wired (<BASE_URL>)`, with `; contact <CONTACT_EMAIL>` appended when a
/// contact address is configured.
#[must_use]
pub fn user_agent(base_url: &str, contact_email: Option<&str>) -> String {
    let contact = contact_email.map_or_else(String::new, |email| format!("; contact {email}"));
    format!("{USER_AGENT_NAME} ({base_url}){contact}")
}

/// The shared outbound HTTP client. Cloning is cheap: every clone drives
/// the same `reqwest` connection pool.
#[derive(Clone, Debug, Default)]
pub struct HttpClient {
    /// The wrapped `reqwest` client.
    inner: Client,
    /// The SSRF guard, or [`None`] when private hosts are allowed (see
    /// `HTTP_ALLOW_PRIVATE_HOSTS`): pre-flight URL checks consult it, while
    /// connect-time and redirect-time checks are baked into `inner`'s
    /// resolver and redirect policy.
    ssrf: Option<ssrf::SsrfGuard>,
}

impl HttpClient {
    /// Builds a client with the project's outbound defaults (5 s connect
    /// timeout, 20 s total timeout, at most 5 redirects), the SSRF guard
    /// enabled, and `user_agent` as its `User-Agent`.
    ///
    /// `reqwest`'s builder only rejects a configuration whose `User-Agent`
    /// holds bytes an HTTP header value cannot carry; that cannot happen
    /// for a config-sourced value, and instead of panicking the client
    /// degrades to a rebuilt configuration without that `User-Agent`.
    #[must_use]
    pub fn new(user_agent: &str) -> Self {
        Self::configured(Some(user_agent), false, TOTAL_TIMEOUT)
    }

    /// Builds the client a test talking to local mini-servers needs: the
    /// SSRF guard is disabled (as with `HTTP_ALLOW_PRIVATE_HOSTS=true`,
    /// see [`crate::config`]), so loopback targets are reachable, and
    /// every other outbound rule matches [`HttpClient::new`].
    #[cfg(any(test, feature = "integration-tests"))]
    #[must_use]
    pub fn new_for_tests(user_agent: &str) -> Self {
        Self::configured(Some(user_agent), true, TOTAL_TIMEOUT)
    }

    /// Test-only client overriding the total timeout, so a timeout test
    /// does not have to wait out [`TOTAL_TIMEOUT`]. Like
    /// [`new_for_tests`](Self::new_for_tests) it talks to local servers
    /// and therefore skips the SSRF guard.
    #[cfg(test)]
    #[must_use]
    fn with_timeout(user_agent: &str, total: Duration) -> Self {
        Self::configured(Some(user_agent), true, total)
    }

    /// Assembles the real client for the given knobs, degrading in two
    /// steps rather than panicking if `reqwest` rejects the configuration
    /// (which only happens for a `User-Agent` no header value can carry).
    fn configured(user_agent: Option<&str>, allow_private: bool, total: Duration) -> Self {
        match Self::try_configured(user_agent, allow_private, total) {
            Ok(client) => client,
            Err(err) => {
                error!(%err, "outbound client rejected its User-Agent; rebuilding without it");
                Self::try_configured(None, allow_private, total).unwrap_or_else(|fallback| {
                    error!(%fallback, "outbound client rejected its configuration; using reqwest defaults");
                    Self {
                        inner: Client::default(),
                        ssrf: (!allow_private).then_some(ssrf::SsrfGuard),
                    }
                })
            }
        }
    }

    /// Builds the `reqwest` client, installing the SSRF guard — both its
    /// pre-flight half (kept on `Self`) and its connect-time halves (the
    /// pinned DNS resolver and the redirect policy) — unless
    /// `allow_private` disables it.
    fn try_configured(
        user_agent: Option<&str>,
        allow_private: bool,
        total: Duration,
    ) -> Result<Self, reqwest::Error> {
        let mut builder = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(total);
        if let Some(agent) = user_agent {
            builder = builder.user_agent(agent);
        }
        let inner = if allow_private {
            builder
                .redirect(redirect::Policy::limited(MAX_REDIRECTS))
                .build()?
        } else {
            builder
                .dns_resolver(ssrf::SsrfGuard)
                .redirect(ssrf::redirect_policy())
                .build()?
        };
        Ok(Self {
            inner,
            ssrf: (!allow_private).then_some(ssrf::SsrfGuard),
        })
    }

    /// Fetches `url` with a `GET`, plus `headers`, reading the response
    /// body incrementally and giving up with [`HttpError::TooLarge`] as
    /// soon as more than `max_bytes` have arrived.
    ///
    /// # Errors
    ///
    /// [`HttpError::InvalidUrl`] if `url` is unparsable or not an
    /// `http`/`https` URL, [`HttpError::Blocked`] if the SSRF guard
    /// refuses the target (or a redirect hop's target),
    /// [`HttpError::Timeout`] if a timeout elapses,
    /// [`HttpError::TooLarge`] per the body cap, and [`HttpError::Connect`]
    /// for transport failures (including headers HTTP cannot represent).
    /// Status codes are not checked — chain
    /// [`HttpResponse::error_for_status`] to fail on 4xx/5xx.
    pub async fn get_limited(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        max_bytes: usize,
    ) -> Result<HttpResponse, HttpError> {
        let request = self.build_request(Method::GET, url, headers).await?;
        read_body(request.send().await?, max_bytes).await
    }

    /// Sends `body` with a `POST` to `url`, plus `headers`, and reads the
    /// full response. Delivery endpoints mostly care about the status; the
    /// body is buffered because callers routinely log or parse it.
    ///
    /// # Errors
    ///
    /// Same as [`HttpClient::get_limited`], except that the response body
    /// is read without a size cap, so [`HttpError::TooLarge`] cannot occur.
    pub async fn post_bytes(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<HttpResponse, HttpError> {
        let request = self
            .build_request(Method::POST, url, headers)
            .await?
            .body(body);
        read_body(request.send().await?, usize::MAX).await
    }

    /// Validates `url` (`http`/`https` only), passes it through the SSRF
    /// guard, and assembles an un-sent request carrying `headers` on top
    /// of the client defaults. The guard runs after header validation so
    /// that an unsendable header is reported without touching DNS at all.
    ///
    /// # Errors
    ///
    /// [`HttpError::InvalidUrl`] for a non-`http(s)` or unparsable URL,
    /// [`HttpError::Connect`] if a header name or value is one HTTP cannot
    /// represent, since such a request could never leave this process, and
    /// [`HttpError::Blocked`] when the SSRF guard refuses the target.
    async fn build_request(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<reqwest::RequestBuilder, HttpError> {
        let Ok(parsed) = Url::parse(url) else {
            return Err(HttpError::InvalidUrl);
        };
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(HttpError::InvalidUrl);
        }
        let builder = self
            .inner
            .request(method, parsed.as_str())
            .headers(header_map(headers)?);
        if let Some(guard) = &self.ssrf {
            guard.check_target(&parsed).await?;
        }
        Ok(builder)
    }

    /// Fetches `url` with a `GET` HTTP-signed by the actor key
    /// `key_id`/`private_pem` (see [`crate::federation::signature`]):
    /// `Accept: application/activity+json` plus the signed `Host`, `Date`
    /// and `Signature` headers. This is the shape of an authorized-fetch
    /// or remote-actor dereference.
    ///
    /// # Errors
    ///
    /// [`HttpError::Connect`] if the request cannot be signed (`url` is not
    /// a signable http(s) URL, or `private_pem` does not parse), otherwise
    /// the same as [`HttpClient::get_limited`] — except that the response
    /// body is read without a size cap, so [`HttpError::TooLarge`] cannot
    /// occur.
    pub async fn signed_get(
        &self,
        url: &str,
        key_id: &str,
        private_pem: &str,
    ) -> Result<HttpResponse, HttpError> {
        let signed = sign_for_send("GET", url, None, key_id, private_pem)?;
        self.get_limited(url, &ap_headers(&signed, false), usize::MAX)
            .await
    }

    /// Delivers `body` to `url` with a HTTP-signed `POST`:
    /// `Accept` and `Content-Type` of `application/activity+json` plus the
    /// signed `Host`, `Date`, `Digest` and `Signature` headers. This is the
    /// shape of an inbox delivery.
    ///
    /// # Errors
    ///
    /// Same as [`HttpClient::signed_get`] (with `url` also checked for
    /// signability before anything is sent), then the same as
    /// [`HttpClient::post_bytes`].
    pub async fn signed_post(
        &self,
        url: &str,
        body: &[u8],
        key_id: &str,
        private_pem: &str,
    ) -> Result<HttpResponse, HttpError> {
        let signed = sign_for_send("POST", url, Some(body), key_id, private_pem)?;
        self.post_bytes(url, &ap_headers(&signed, true), body.to_vec())
            .await
    }
}

/// The [`HttpClient`] the instance ships with: the public origin and
/// contact email from `config` formatted per [`user_agent`], with the
/// SSRF guard enabled unless `config.http_allow_private_hosts` (the
/// `HTTP_ALLOW_PRIVATE_HOSTS` environment variable) disables it.
impl From<&Config> for HttpClient {
    fn from(config: &Config) -> Self {
        Self::configured(
            Some(&user_agent(
                &config.base_url,
                config.contact_email.as_deref(),
            )),
            config.http_allow_private_hosts,
            TOTAL_TIMEOUT,
        )
    }
}

/// Collects `(name, value)` pairs into a header map; a repeated name keeps
/// its last value.
///
/// # Errors
///
/// [`HttpError::Connect`] naming the first entry HTTP cannot represent.
fn header_map(headers: &[(&str, &str)]) -> Result<HeaderMap, HttpError> {
    let mut map = HeaderMap::with_capacity(headers.len());
    for &(key, value) in headers {
        let name = HeaderName::try_from(key)
            .map_err(|err| HttpError::Connect(format!("invalid header name `{key}`: {err}")))?;
        let value = HeaderValue::from_str(value).map_err(|err| {
            HttpError::Connect(format!("invalid value for header `{key}`: {err}"))
        })?;
        let replaced = map.insert(name, value);
        drop(replaced);
    }
    Ok(map)
}

/// Signs `method url` [`at the current moment`](Utc::now), folding a
/// signing failure into [`HttpError::Connect`]: the request never left
/// this process because it could not be built.
///
/// # Errors
///
/// [`HttpError::Connect`] if [`sign_request`] rejects `url` or
/// `private_pem`, or the RSA operation fails.
fn sign_for_send(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    key_id: &str,
    private_pem: &str,
) -> Result<SignedHeaders, HttpError> {
    let key = SigningKey {
        key_id,
        private_pem,
    };
    sign_request(method, url, body, &key, Utc::now())
        .map_err(|err| HttpError::Connect(format!("could not sign the request: {err}")))
}

/// The header pairs of a signed federation exchange: `Accept` (and, with
/// `with_body`, the `Content-Type` the body is labelled by) plus the
/// signed `Host`, `Date`, optional `Digest` and `Signature`. `Host` is
/// sent explicitly so the peer sees exactly the host that was signed.
#[must_use]
fn ap_headers(signed: &SignedHeaders, with_body: bool) -> Vec<(&str, &str)> {
    let mut headers = vec![("accept", ACTIVITY_JSON)];
    if with_body {
        headers.push(("content-type", ACTIVITY_JSON));
    }
    headers.push(("host", &signed.host));
    headers.push(("date", &signed.date));
    if let Some(digest) = &signed.digest {
        headers.push(("digest", digest));
    }
    headers.push(("signature", &signed.signature));
    headers
}

/// Reads `response`'s body incrementally into memory, stopping with
/// [`HttpError::TooLarge`] once more than `max_bytes` have arrived.
async fn read_body(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<HttpResponse, HttpError> {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let mut body = BytesMut::new();
    while let Some(chunk) = response.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() > max_bytes {
            return Err(HttpError::TooLarge);
        }
    }
    Ok(HttpResponse {
        status,
        headers,
        body: body.freeze(),
    })
}

/// An outbound response read in full: status, headers and buffered body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    /// HTTP status code of the response.
    pub status: u16,
    /// Response headers.
    pub headers: HeaderMap,
    /// The buffered response body.
    pub body: Bytes,
}

impl HttpResponse {
    /// Parses [`body`](Self::body) as JSON of shape `T`.
    ///
    /// # Errors
    ///
    /// The [`serde_json::Error`] for malformed JSON or a shape mismatch;
    /// this is a pure parsing helper, no HTTP is involved.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    /// Turns a status at or above 400 into [`HttpError::Status`], passing
    /// `self` through unchanged otherwise (mirrors `reqwest`'s helper of
    /// the same name, but reports this module's error type).
    ///
    /// # Errors
    ///
    /// [`HttpError::Status`] carrying [`status`](Self::status) when it is
    /// 400 or greater.
    pub fn error_for_status(self) -> Result<Self, HttpError> {
        if self.status >= ERROR_STATUS_MIN {
            Err(HttpError::Status(self.status))
        } else {
            Ok(self)
        }
    }
}

/// How an outbound HTTP request can fail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HttpError {
    /// The URL is unparsable, or not an `http`/`https` URL.
    InvalidUrl,
    /// A connect or total timeout elapsed before the response was read.
    Timeout,
    /// The response body passed the `max_bytes` cap of
    /// [`HttpClient::get_limited`].
    TooLarge,
    /// The request never completed: a transport failure, a peer that
    /// closed the exchange, or a header HTTP cannot represent. The payload
    /// is the underlying client's message.
    Connect(String),
    /// The peer answered 4xx/5xx and the caller opted into treating that
    /// as an error (see [`HttpResponse::error_for_status`]); the payload is
    /// the status code.
    Status(u16),
    /// A local policy refused the request: today the SSRF guard (see
    /// [`ssrf`]), which rejects targets whose address is loopback,
    /// private, or otherwise reserved; the payload states the reason.
    Blocked(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl => f.write_str("URL is unparsable or not an http(s) URL"),
            Self::Timeout => f.write_str("HTTP request timed out"),
            Self::TooLarge => f.write_str("HTTP response exceeded the body size limit"),
            Self::Connect(detail) => write!(f, "HTTP request failed: {detail}"),
            Self::Status(code) => write!(f, "HTTP peer answered {code}"),
            Self::Blocked(reason) => write!(f, "HTTP request blocked: {reason}"),
        }
    }
}

impl Error for HttpError {}

/// Transport outcomes map onto the variants that describe them; timeouts
/// keep their own variant because callers treat them as retryable, and an
/// SSRF refusal surfacing through the guard's resolver or redirect policy
/// keeps [`HttpError::Blocked`] with its reason.
impl From<reqwest::Error> for HttpError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            return Self::Timeout;
        }
        if let Some(reason) = ssrf::blocked_reason(&err) {
            return Self::Blocked(reason);
        }
        Self::Connect(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        Json, Router,
        body::Body,
        extract::State,
        http::{HeaderMap, Method, StatusCode, Uri, header},
        response::Response,
        routing::{get, post},
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use serde_json::Value;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;
    use crate::{
        federation::signature::{digest_header, signing_string},
        util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM, verify_sha256},
    };

    /// `User-Agent` used by clients that only exercise request handling.
    const TEST_UA: &str = "Wired (https://wired.test)";
    /// The key id the signed-request tests present.
    const TEST_KEY_ID: &str = "https://wired.test/actor#main-key";
    /// The `headers` parameter of a signed bodyless GET.
    const GET_HEADERS_PARAM: &str = "(request-target) host date";
    /// The `headers` parameter of a signed POST (with digest).
    const POST_HEADERS_PARAM: &str = "(request-target) host date digest";
    /// A 100-byte body served by the `/big` route.
    const BIG_BODY: [u8; 100] = [b'x'; 100];
    /// Total timeout small enough to let a sleeping handler lose the race.
    const SHORT_TIMEOUT: Duration = Duration::from_millis(150);

    /// Binds an unused loopback port.
    async fn bind() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").await.unwrap()
    }

    /// The `path` on `listener` as an absolute `http` URL.
    fn url_for(listener: &TcpListener, path: &str) -> String {
        format!("http://{}{path}", listener.local_addr().unwrap())
    }

    /// Serves `app` on `listener` until the test runtime shuts down.
    fn spawn_server(listener: TcpListener, app: Router) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test server stopped: {err}");
            }
        })
    }

    /// The visible text of header `name`, empty when absent or not visible.
    fn header_text(headers: &HeaderMap, name: &str) -> String {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    /// Answers with the request's own `user-agent` header.
    async fn echo_user_agent(headers: HeaderMap) -> String {
        header_text(&headers, "user-agent")
    }

    /// Answers with the request's own `x-marker` header.
    async fn echo_marker(headers: HeaderMap) -> String {
        header_text(&headers, "x-marker")
    }

    /// Answers with the fixed 100-byte [`BIG_BODY`].
    async fn big() -> &'static [u8] {
        &BIG_BODY
    }

    /// Sleeps far past [`SHORT_TIMEOUT`] before answering.
    async fn slow() -> &'static str {
        tokio::time::sleep(Duration::from_secs(30)).await;
        "late"
    }

    /// Echoes the request body back verbatim.
    async fn echo_body(body: Bytes) -> Bytes {
        body
    }

    /// Answers with a 301 to the absolute URL carried in state.
    async fn hop(State(location): State<String>) -> Response {
        Response::builder()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header(header::LOCATION, location)
            .body(Body::empty())
            .unwrap()
    }

    /// The redirect destination: proves the client followed the hop.
    async fn redirect_target() -> &'static str {
        "followed"
    }

    #[test]
    fn user_agent_names_instance_and_contact() {
        assert_eq!(
            user_agent("https://social.example", None),
            "Wired (https://social.example)",
            "no contact email: bare instance id"
        );
        assert_eq!(
            user_agent("https://social.example", Some("admin@example.org")),
            "Wired (https://social.example); contact admin@example.org",
            "contact email is appended"
        );
    }

    #[tokio::test]
    async fn get_sends_the_configured_user_agent() {
        let listener = bind().await;
        let url = url_for(&listener, "/ua");
        let server = spawn_server(listener, Router::new().route("/ua", get(echo_user_agent)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let response = client.get_limited(&url, &[], 1024).await.unwrap();

        assert_eq!(response.status, 200, "status");
        assert_eq!(
            String::from_utf8_lossy(&response.body),
            TEST_UA,
            "the builder's User-Agent reaches the peer"
        );
        assert_eq!(
            response
                .headers
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/plain; charset=utf-8"),
            "response headers are kept"
        );
        server.abort();
    }

    #[tokio::test]
    async fn config_client_sends_the_instance_user_agent() {
        let listener = bind().await;
        let url = url_for(&listener, "/ua");
        let server = spawn_server(listener, Router::new().route("/ua", get(echo_user_agent)));
        let client = HttpClient::from(&Config::for_tests());

        let response = client.get_limited(&url, &[], 1024).await.unwrap();

        assert_eq!(
            String::from_utf8_lossy(&response.body),
            "Wired (https://test.example)",
            "a test config has base_url but no contact email"
        );
        server.abort();
    }

    #[tokio::test]
    async fn request_headers_are_sent() {
        let listener = bind().await;
        let url = url_for(&listener, "/marker");
        let server = spawn_server(listener, Router::new().route("/marker", get(echo_marker)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let response = client
            .get_limited(&url, &[("x-marker", "yes")], 1024)
            .await
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&response.body),
            "yes",
            "the extra header reaches the peer"
        );
        server.abort();
    }

    #[tokio::test]
    async fn body_over_the_limit_is_too_large_and_at_it_fits() {
        let listener = bind().await;
        let url = url_for(&listener, "/big");
        let server = spawn_server(listener, Router::new().route("/big", get(big)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let tight = client.get_limited(&url, &[], BIG_BODY.len() - 1).await;
        assert_eq!(
            tight.unwrap_err(),
            HttpError::TooLarge,
            "one byte over the cap fails"
        );

        let exact = client.get_limited(&url, &[], BIG_BODY.len()).await.unwrap();
        assert_eq!(exact.body.len(), BIG_BODY.len(), "exactly max_bytes fits");
        server.abort();
    }

    #[tokio::test]
    async fn a_stalled_response_times_out() {
        let listener = bind().await;
        let url = url_for(&listener, "/slow");
        let server = spawn_server(listener, Router::new().route("/slow", get(slow)));
        let client = HttpClient::with_timeout(TEST_UA, SHORT_TIMEOUT);

        let result = client.get_limited(&url, &[], 1024).await;

        assert_eq!(
            result.unwrap_err(),
            HttpError::Timeout,
            "the total timeout fires while the peer stalls"
        );
        server.abort();
    }

    #[tokio::test]
    async fn non_http_urls_are_rejected_before_any_connection() {
        let client = HttpClient::new_for_tests(TEST_UA);
        for url in [
            "ftp://example.com/pub",
            "file:///etc/passwd",
            "not a url",
            "https://",
            "",
        ] {
            let result = client.get_limited(url, &[], 1024).await;
            assert_eq!(
                result.unwrap_err(),
                HttpError::InvalidUrl,
                "{url} must not be fetched"
            );
        }
    }

    #[tokio::test]
    async fn loopback_literal_is_blocked_by_default_and_allowed_with_the_flag() {
        let listener = bind().await;
        let url = url_for(&listener, "/ua");
        let server = spawn_server(listener, Router::new().route("/ua", get(echo_user_agent)));

        let guarded = HttpClient::new(TEST_UA);
        let blocked = guarded.get_limited(&url, &[], 1024).await;
        match blocked {
            Err(HttpError::Blocked(reason)) => {
                assert!(
                    reason.contains("127.0.0.1"),
                    "the reason names the refused address: {reason}"
                );
            }
            other => panic!("a loopback literal must be Blocked, got {other:?}"),
        }

        // The same client with `HTTP_ALLOW_PRIVATE_HOSTS` semantics.
        let open = HttpClient::new_for_tests(TEST_UA);
        let allowed = open.get_limited(&url, &[], 1024).await.unwrap();
        assert_eq!(allowed.status, 200, "the flag reaches the local server");
        server.abort();
    }

    #[tokio::test]
    async fn localhost_hostname_is_blocked_before_connecting() {
        let listener = bind().await;
        let port = listener.local_addr().unwrap().port();
        let server = spawn_server(listener, Router::new().route("/ua", get(echo_user_agent)));
        let client = HttpClient::new(TEST_UA);

        let result = client
            .get_limited(&format!("http://localhost:{port}/ua"), &[], 1024)
            .await;

        match result {
            Err(HttpError::Blocked(reason)) => {
                assert!(
                    reason.contains("127.0.0.1") || reason.contains("::1"),
                    "the reason names the resolved loopback address: {reason}"
                );
            }
            other => panic!("`localhost` must resolve to a blocked address, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn config_client_blocks_loopback_unless_the_config_flag_allows_it() {
        let listener = bind().await;
        let url = url_for(&listener, "/ua");
        let server = spawn_server(listener, Router::new().route("/ua", get(echo_user_agent)));
        // A default config leaves HTTP_ALLOW_PRIVATE_HOSTS unset, so the
        // shipped client must guard; `Config::for_tests` sets the flag.
        let guarded = Config::from_lookup(|key| match key {
            crate::config::DATABASE_URL_VAR => Some("postgres://localhost/wired".to_owned()),
            crate::config::BASE_URL_VAR => Some("https://test.example".to_owned()),
            _ => None,
        })
        .unwrap();
        assert!(
            !guarded.http_allow_private_hosts,
            "the default config keeps the guard on"
        );

        let blocked = HttpClient::from(&guarded)
            .get_limited(&url, &[], 1024)
            .await;
        assert!(
            matches!(blocked, Err(HttpError::Blocked(_))),
            "a default config's client refuses loopback, got {blocked:?}"
        );

        let open = HttpClient::from(&Config::for_tests());
        let allowed = open.get_limited(&url, &[], 1024).await.unwrap();
        assert_eq!(
            allowed.status, 200,
            "the test config's flag reaches the peer"
        );
        server.abort();
    }

    #[tokio::test]
    async fn post_bytes_is_blocked_by_the_guard_too() {
        let client = HttpClient::new(TEST_UA);
        let result = client
            .post_bytes("http://127.0.0.1:9/ingest", &[], b"x".to_vec())
            .await;
        assert!(
            matches!(result, Err(HttpError::Blocked(_))),
            "delivery POSTs pass the same guard, got {result:?}"
        );
    }

    #[tokio::test]
    async fn redirect_is_followed() {
        let listener = bind().await;
        let hop_url = url_for(&listener, "/hop");
        let target = url_for(&listener, "/final");
        let app = Router::new()
            .route("/hop", get(hop).with_state(target))
            .route("/final", get(redirect_target));
        let server = spawn_server(listener, app);
        let client = HttpClient::new_for_tests(TEST_UA);

        let response = client
            .get_limited(&hop_url, &[], 1024)
            .await
            .unwrap()
            .error_for_status()
            .unwrap();

        assert_eq!(response.status, 200, "final status after the hop");
        assert_eq!(
            String::from_utf8_lossy(&response.body),
            "followed",
            "the client ended up at the target"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_redirect_chain_past_five_fails_as_connect() {
        let listener = bind().await;
        let loop_url = url_for(&listener, "/hop");
        let app = Router::new().route("/hop", get(hop).with_state(loop_url.clone()));
        let server = spawn_server(listener, app);
        let client = HttpClient::new_for_tests(TEST_UA);

        let Err(err) = client.get_limited(&loop_url, &[], 1024).await else {
            panic!("a client looping past its redirect limit must not get a response");
        };
        assert!(
            matches!(err, HttpError::Connect(_)),
            "exceeding the redirect limit is a transport failure"
        );
        server.abort();
    }

    #[tokio::test]
    async fn post_bytes_sends_the_body() {
        let listener = bind().await;
        let url = url_for(&listener, "/echo");
        let server = spawn_server(listener, Router::new().route("/echo", post(echo_body)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let response = client
            .post_bytes(&url, &[("x-marker", "pong")], b"activity".to_vec())
            .await
            .unwrap();

        assert_eq!(response.status, 200, "echo status");
        assert_eq!(
            response.body,
            Bytes::from_static(b"activity"),
            "the body round-trips"
        );
        server.abort();
    }

    #[tokio::test]
    async fn unsendable_headers_fail_as_connect() {
        let client = HttpClient::new_for_tests(TEST_UA);
        let result = client
            .get_limited("https://example.com/x", &[("x bad name", "ok")], 1024)
            .await;
        assert!(
            matches!(result, Err(HttpError::Connect(_))),
            "a header HTTP cannot represent never leaves the process"
        );
    }

    #[test]
    fn error_for_status_flags_4xx_and_5xx_only() {
        let base = HttpResponse {
            status: 0,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        };
        for (status, failed) in [
            (200_u16, false),
            (204, false),
            (301, false),
            (399, false),
            (400, true),
            (404, true),
            (503, true),
        ] {
            let checked = HttpResponse {
                status,
                ..base.clone()
            }
            .error_for_status();
            assert_eq!(
                matches!(checked, Err(HttpError::Status(_))),
                failed,
                "status {status}"
            );
            if failed {
                assert_eq!(
                    checked.unwrap_err(),
                    HttpError::Status(status),
                    "the status code is carried"
                );
            }
        }
    }

    #[test]
    fn json_parses_the_body() {
        let response = HttpResponse {
            status: 200,
            headers: HeaderMap::new(),
            body: Bytes::from_static(br#"{"name":"wired"}"#),
        };
        let value: Value = response.json().unwrap();
        assert_eq!(value["name"], "wired", "body parsed");
        let broken = HttpResponse {
            body: Bytes::from_static(b"{oops"),
            ..response
        };
        let parse_err = broken.json::<Value>().unwrap_err();
        assert!(
            !parse_err.to_string().is_empty(),
            "malformed JSON reports a parse error"
        );
    }

    #[test]
    fn http_error_display_names_the_cause() {
        assert_eq!(
            HttpError::InvalidUrl.to_string(),
            "URL is unparsable or not an http(s) URL",
            "InvalidUrl"
        );
        assert_eq!(
            HttpError::Timeout.to_string(),
            "HTTP request timed out",
            "Timeout"
        );
        assert_eq!(
            HttpError::TooLarge.to_string(),
            "HTTP response exceeded the body size limit",
            "TooLarge"
        );
        assert_eq!(
            HttpError::Connect("refused".to_owned()).to_string(),
            "HTTP request failed: refused",
            "Connect keeps its detail"
        );
        assert_eq!(
            HttpError::Status(503).to_string(),
            "HTTP peer answered 503",
            "Status carries the code"
        );
        assert_eq!(
            HttpError::Blocked("private address".to_owned()).to_string(),
            "HTTP request blocked: private address",
            "Blocked keeps its reason"
        );
        assert!(HttpError::Timeout.source().is_none(), "no wrapped source");
    }

    #[tokio::test]
    async fn invalid_user_agent_degrades_instead_of_panicking() {
        let listener = bind().await;
        let url = url_for(&listener, "/ua");
        let server = spawn_server(listener, Router::new().route("/ua", get(echo_user_agent)));
        // A control character is illegal in a header value, so the builder
        // rejects the configuration; `configured` must degrade, not panic.
        // Guard off: the degraded client still fetches the loopback peer.
        let client = HttpClient::configured(Some("bad\nagent"), true, TOTAL_TIMEOUT);

        let response = client.get_limited(&url, &[], 1024).await.unwrap();

        assert_eq!(response.status, 200, "the fallback client still fetches");
        assert!(
            response.body.is_empty(),
            "the degraded client sends no custom User-Agent"
        );
        server.abort();
    }

    #[tokio::test]
    async fn degradation_keeps_the_ssrf_guard_enabled() {
        let listener = bind().await;
        let url = url_for(&listener, "/ua");
        let server = spawn_server(listener, Router::new().route("/ua", get(echo_user_agent)));
        // Same rejected `User-Agent`, but with the guard on: the rebuild
        // without it must not quietly drop address filtering.
        let client = HttpClient::configured(Some("bad\nagent"), false, TOTAL_TIMEOUT);

        let result = client.get_limited(&url, &[], 1024).await;

        assert!(
            matches!(result, Err(HttpError::Blocked(_))),
            "a degraded client still blocks loopback, got {result:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn clones_share_the_client() {
        let listener = bind().await;
        let url = url_for(&listener, "/ua");
        let server = spawn_server(listener, Router::new().route("/ua", get(echo_user_agent)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let via_clone = HttpClient::clone(&client)
            .get_limited(&url, &[], 1024)
            .await
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&via_clone.body),
            TEST_UA,
            "a clone sends with the same configuration"
        );
        server.abort();
    }

    /// Reports what a federation peer would see: the request target as
    /// `(request-target)` is signed, plus the header values a signed
    /// request must carry (absent keys are omitted).
    async fn echo_ap_request(method: Method, uri: Uri, headers: HeaderMap) -> Json<Value> {
        let target = format!(
            "{} {}",
            method.as_str().to_lowercase(),
            uri.query().map_or_else(
                || uri.path().to_owned(),
                |query| format!("{}?{query}", uri.path()),
            ),
        );
        let mut seen = serde_json::Map::new();
        drop(seen.insert("request-target".to_owned(), Value::String(target)));
        for name in [
            "accept",
            "content-type",
            "host",
            "date",
            "digest",
            "signature",
        ] {
            if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
                drop(seen.insert(name.to_owned(), Value::String(value.to_owned())));
            }
        }
        Json(Value::Object(seen))
    }

    /// The named parameter of a `Signature` header value, unquoted.
    fn signature_param(header: &str, name: &str) -> String {
        let prefix = format!("{name}=\"");
        let pair = header
            .split(',')
            .find(|p| p.starts_with(&prefix))
            .unwrap_or_else(|| panic!("signature header carries a {name} parameter: {header}"));
        let value = pair
            .strip_prefix(prefix.as_str())
            .unwrap_or_else(|| panic!("the {name} parameter is quoted: {header}"));
        value.trim_end_matches('"').to_owned()
    }

    /// The raw signature bytes out of a peer-observed `Signature` header,
    /// checking the other three parameters name this test's key, the
    /// `rsa-sha256` algorithm and `headers_param`.
    fn observed_signature(header: &str, headers_param: &str) -> Vec<u8> {
        assert_eq!(
            signature_param(header, "keyId"),
            TEST_KEY_ID,
            "the signature names our key id"
        );
        assert_eq!(
            signature_param(header, "algorithm"),
            "rsa-sha256",
            "the algorithm Mastodon and Pleroma speak"
        );
        assert_eq!(
            signature_param(header, "headers"),
            headers_param,
            "the signed header list"
        );
        BASE64.decode(signature_param(header, "signature")).unwrap()
    }

    /// The four lines a peer can rebuild for us, as the text the signature
    /// must verify over.
    fn peer_signing_string(seen: &Value) -> String {
        signing_string(
            seen["request-target"].as_str().unwrap(),
            seen["host"].as_str().unwrap(),
            seen["date"].as_str().unwrap(),
            seen.get("digest").and_then(Value::as_str),
        )
    }

    #[tokio::test]
    async fn signed_get_sends_activity_json_and_a_verifiable_signature() {
        let listener = bind().await;
        let url = url_for(&listener, "/actor?page=2");
        let server = spawn_server(
            listener,
            Router::new().route("/actor", get(echo_ap_request)),
        );
        let client = HttpClient::new_for_tests(TEST_UA);

        let response = client
            .signed_get(&url, TEST_KEY_ID, FIXTURE_PRIVATE_PKCS1_PEM)
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let seen: Value = response.json().unwrap();
        server.abort();

        assert_eq!(seen["accept"], ACTIVITY_JSON, "asks for AP JSON");
        assert!(
            seen.get("content-type").is_none(),
            "a GET sends no body to label"
        );
        assert_eq!(
            seen["request-target"], "get /actor?page=2",
            "the peer sees method and path with the query"
        );
        assert!(seen.get("digest").is_none(), "a GET signs no digest");
        assert!(
            seen["date"].as_str().unwrap().ends_with(" GMT"),
            "an IMF-fixdate date"
        );
        let signature = observed_signature(seen["signature"].as_str().unwrap(), GET_HEADERS_PARAM);
        assert!(
            verify_sha256(
                FIXTURE_PUBLIC_PKCS1_PEM,
                peer_signing_string(&seen).as_bytes(),
                &signature,
            ),
            "the signature verifies over exactly what the peer saw"
        );
    }

    #[tokio::test]
    async fn signed_get_sends_the_host_it_signed() {
        let listener = bind().await;
        let url = url_for(&listener, "/actor");
        let port = listener.local_addr().unwrap().port();
        let server = spawn_server(
            listener,
            Router::new().route("/actor", get(echo_ap_request)),
        );
        let client = HttpClient::new_for_tests(TEST_UA);

        let response = client
            .signed_get(&url, TEST_KEY_ID, FIXTURE_PRIVATE_PKCS1_PEM)
            .await
            .unwrap();
        let seen: Value = response.json().unwrap();
        server.abort();

        assert_eq!(
            seen["host"],
            format!("127.0.0.1:{port}"),
            "the non-default port rides into the signed host"
        );
    }

    #[tokio::test]
    async fn signed_post_adds_digest_and_content_type_to_the_signature() {
        let listener = bind().await;
        let url = url_for(&listener, "/inbox");
        let server = spawn_server(
            listener,
            Router::new().route("/inbox", post(echo_ap_request)),
        );
        let client = HttpClient::new_for_tests(TEST_UA);
        let body = br#"{"type":"Create"}"#;

        let response = client
            .signed_post(&url, body, TEST_KEY_ID, FIXTURE_PRIVATE_PKCS1_PEM)
            .await
            .unwrap();
        let seen: Value = response.json().unwrap();
        server.abort();

        assert_eq!(seen["accept"], ACTIVITY_JSON, "asks for AP JSON");
        assert_eq!(
            seen["content-type"], ACTIVITY_JSON,
            "labels the activity body"
        );
        assert_eq!(
            seen["digest"],
            digest_header(body),
            "the peer sees the digest we signed"
        );
        let signature = observed_signature(seen["signature"].as_str().unwrap(), POST_HEADERS_PARAM);
        assert!(
            verify_sha256(
                FIXTURE_PUBLIC_PKCS1_PEM,
                peer_signing_string(&seen).as_bytes(),
                &signature,
            ),
            "the four-line signing string verifies"
        );
    }

    #[tokio::test]
    async fn unsignable_requests_fail_before_any_connection() {
        let client = HttpClient::new_for_tests(TEST_UA);
        let bad_url = client
            .signed_get(
                "ftp://example.com/pub",
                TEST_KEY_ID,
                FIXTURE_PRIVATE_PKCS1_PEM,
            )
            .await;
        assert!(
            matches!(bad_url, Err(HttpError::Connect(_))),
            "a non-http(s) URL cannot be signed, got {bad_url:?}"
        );
        let junk_key = client
            .signed_get("https://peer.invalid/actor", TEST_KEY_ID, "not a key")
            .await;
        assert!(
            matches!(junk_key, Err(HttpError::Connect(_))),
            "unparseable key material fails the same way, got {junk_key:?}"
        );
    }
}
