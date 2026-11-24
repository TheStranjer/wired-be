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
//!
//! The module is a tree: `headers` holds the header-map, signing and
//! body-reading helpers, `response` the [`HttpResponse`] type and `error`
//! the [`HttpError`] type. The [`HttpClient`] type keeps its one inherent
//! `impl` block here, so `send` is the child carrying the request-sending
//! methods' tests, and `test_support` (test-only) holds the mock-server
//! fixtures the children's unit tests share. Every public item is
//! re-exported here, so an outside path stays `crate::http_client::…`.

pub mod ssrf;

mod error;
mod headers;
mod response;
mod send;

#[cfg(test)]
mod test_support;

use std::time::Duration;

use reqwest::{Client, Method, redirect};
use tracing::error;
use url::Url;

use crate::config::Config;

use self::headers::{ap_headers, header_map, read_body, sign_for_send};

pub use self::{error::HttpError, response::HttpResponse, ssrf::is_blocked_ip};

/// Time allowed to establish the TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Time allowed for the whole request, response body read included.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
/// Redirects followed before the response is handed back unanswered.
const MAX_REDIRECTS: usize = 5;
/// Response-body cap (1 MiB) for callers that parse what the peer answers,
/// matching Mastodon's default; callers that read only the status pick a
/// tighter cap of their own.
pub const DEFAULT_RESPONSE_BYTES: usize = 1024 * 1024;
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
        Self::configured(Some(user_agent), false, TOTAL_TIMEOUT, MAX_REDIRECTS)
    }

    /// Builds the client a test talking to local mini-servers needs: the
    /// SSRF guard is disabled (as with `HTTP_ALLOW_PRIVATE_HOSTS=true`,
    /// see [`crate::config`]), so loopback targets are reachable, and
    /// every other outbound rule matches [`HttpClient::new`].
    #[cfg(any(test, feature = "integration-tests"))]
    #[must_use]
    pub fn new_for_tests(user_agent: &str) -> Self {
        Self::configured(Some(user_agent), true, TOTAL_TIMEOUT, MAX_REDIRECTS)
    }

    /// Builds the client a federation object fetch runs on: the same
    /// outbound rules as [`new`] (or [`new_for_tests`](Self::new_for_tests)
    /// when `allow_private` disables the SSRF guard), except that at most
    /// `max_redirects` redirects are followed instead of [`MAX_REDIRECTS`]
    /// — the tighter budget [`crate::federation::fetch_object`] keeps.
    #[must_use]
    pub(crate) fn with_redirect_limit(
        user_agent: &str,
        allow_private: bool,
        max_redirects: usize,
    ) -> Self {
        Self::configured(
            Some(user_agent),
            allow_private,
            TOTAL_TIMEOUT,
            max_redirects,
        )
    }

    /// Test-only client overriding the total timeout, so a timeout test
    /// does not have to wait out [`TOTAL_TIMEOUT`]. Like
    /// [`new_for_tests`](Self::new_for_tests) it talks to local servers
    /// and therefore skips the SSRF guard.
    #[cfg(test)]
    #[must_use]
    fn with_timeout(user_agent: &str, total: Duration) -> Self {
        Self::configured(Some(user_agent), true, total, MAX_REDIRECTS)
    }

    /// Assembles the real client for the given knobs, degrading in two
    /// steps rather than panicking if `reqwest` rejects the configuration
    /// (which only happens for a `User-Agent` no header value can carry).
    fn configured(
        user_agent: Option<&str>,
        allow_private: bool,
        total: Duration,
        max_redirects: usize,
    ) -> Self {
        match Self::try_configured(user_agent, allow_private, total, max_redirects) {
            Ok(client) => client,
            Err(err) => {
                error!(%err, "outbound client rejected its User-Agent; rebuilding without it");
                Self::try_configured(None, allow_private, total, max_redirects).unwrap_or_else(
                    |fallback| {
                        error!(%fallback, "outbound client rejected its configuration; using reqwest defaults");
                        Self {
                            inner: Client::default(),
                            ssrf: (!allow_private).then_some(ssrf::SsrfGuard),
                        }
                    },
                )
            }
        }
    }

    /// Builds the `reqwest` client, installing the SSRF guard — both its
    /// pre-flight half (kept on `Self`) and its connect-time halves (the
    /// pinned DNS resolver and the redirect policy) — unless
    /// `allow_private` disables it. `max_redirects` caps the redirects
    /// both policies follow.
    fn try_configured(
        user_agent: Option<&str>,
        allow_private: bool,
        total: Duration,
        max_redirects: usize,
    ) -> Result<Self, reqwest::Error> {
        let mut builder = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(total);
        if let Some(agent) = user_agent {
            builder = builder.user_agent(agent);
        }
        let inner = if allow_private {
            builder
                .redirect(redirect::Policy::limited(max_redirects))
                .build()?
        } else {
            builder
                .dns_resolver(ssrf::SsrfGuard)
                .redirect(ssrf::redirect_policy(max_redirects))
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
    /// response, giving up as soon as more than `max_bytes` of body have
    /// arrived. Delivery endpoints mostly care about the status; the body
    /// is buffered because callers routinely log or parse it.
    ///
    /// # Errors
    ///
    /// Same as [`HttpClient::get_limited`]: an over-long response body is
    /// [`HttpError::TooLarge`].
    pub async fn post_bytes(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
        max_bytes: usize,
    ) -> Result<HttpResponse, HttpError> {
        let request = self
            .build_request(Method::POST, url, headers)
            .await?
            .body(body);
        read_body(request.send().await?, max_bytes).await
    }

    /// Sends `method` to `url` with `headers` and reads the full response:
    /// the generic call behind the verbs this client is not otherwise
    /// spelled for — today the media cache's `PURGE` invalidation request
    /// (see [`crate::api::admin::ops`]). The body is capped at `max_bytes`.
    ///
    /// # Errors
    ///
    /// Same as [`HttpClient::post_bytes`]; `method` itself is whatever
    /// `reqwest` accepts, an unparsable one having failed long before this
    /// call at its [`Method::from_bytes`] construction site.
    pub async fn request(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        max_bytes: usize,
    ) -> Result<HttpResponse, HttpError> {
        let request = self.build_request(method, url, headers).await?;
        read_body(request.send().await?, max_bytes).await
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
    /// the same as [`HttpClient::get_limited`], with the response body
    /// capped at `max_bytes`.
    pub async fn signed_get(
        &self,
        url: &str,
        key_id: &str,
        private_pem: &str,
        max_bytes: usize,
    ) -> Result<HttpResponse, HttpError> {
        let signed = sign_for_send("GET", url, None, key_id, private_pem)?;
        self.get_limited(url, &ap_headers(&signed, false), max_bytes)
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
    /// [`HttpClient::post_bytes`], the response body capped at
    /// [`DEFAULT_RESPONSE_BYTES`].
    pub async fn signed_post(
        &self,
        url: &str,
        body: &[u8],
        key_id: &str,
        private_pem: &str,
    ) -> Result<HttpResponse, HttpError> {
        let signed = sign_for_send("POST", url, Some(body), key_id, private_pem)?;
        self.post_bytes(
            url,
            &ap_headers(&signed, true),
            body.to_vec(),
            DEFAULT_RESPONSE_BYTES,
        )
        .await
    }
}

/// The [`HttpClient`] the instance ships with: the public origin and
/// contact email from `config` formatted per [`user_agent`], with the
/// SSRF guard enabled unless `config.fetch.http_allow_private_hosts` (the
/// `HTTP_ALLOW_PRIVATE_HOSTS` environment variable) disables it.
impl From<&Config> for HttpClient {
    fn from(config: &Config) -> Self {
        Self::configured(
            Some(&user_agent(
                &config.base_url,
                config.contact_email.as_deref(),
            )),
            config.fetch.http_allow_private_hosts,
            TOTAL_TIMEOUT,
            MAX_REDIRECTS,
        )
    }
}

#[cfg(test)]
mod tests {
    use axum::{Router, routing::get};

    use super::*;
    use crate::http_client::test_support::*;

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
            !guarded.fetch.http_allow_private_hosts,
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
    async fn invalid_user_agent_degrades_instead_of_panicking() {
        let listener = bind().await;
        let url = url_for(&listener, "/ua");
        let server = spawn_server(listener, Router::new().route("/ua", get(echo_user_agent)));
        // A control character is illegal in a header value, so the builder
        // rejects the configuration; `configured` must degrade, not panic.
        // Guard off: the degraded client still fetches the loopback peer.
        let client = HttpClient::configured(Some("bad\nagent"), true, TOTAL_TIMEOUT, MAX_REDIRECTS);

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
        let client =
            HttpClient::configured(Some("bad\nagent"), false, TOTAL_TIMEOUT, MAX_REDIRECTS);

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
}
