//! Fetching the page a card comes from: the SSRF-guarded, size-capped,
//! HTML-gated `GET` behind [`fetch_card`].
//!
//! [`fetch_card`] spends the `FETCH_TIMEOUT` budget [`fetch_card_within`]
//! enforces over [`fetch_and_parse`] — a capped `GET` whose status
//! [`HttpResponse::error_for_status`] gates, whose content type
//! [`denotes_html`] gates and whose body [`parse_card`] then reads — and
//! every transport refusal arrives as [`RichMediaError`], through the
//! `From<HttpError>` the module root writes for it. The card itself, and
//! the reading of the page, live elsewhere in this service.

use std::time::Duration;

use crate::http_client::{HttpClient, HttpResponse};

use super::{Card, RichMediaError, parse_card};

/// The most a [`fetch_card`] response body may grow to: 1 MiB.
const MAX_CARD_BODY_BYTES: usize = 1024 * 1024;

/// The overall budget of one [`fetch_card`]: connect, redirects, body read
/// and parse all included.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The `Accept` header value [`fetch_card`] asks the page with.
const HTML_ACCEPT: &str = "text/html";

/// Fetches `url` over the SSRF-guarded, redirects-pinned `http` client
/// (see [`crate::http_client`]) and answers the [`Card`] the page
/// describes, under the overall `FETCH_TIMEOUT` budget.
///
/// # Errors
///
/// [`RichMediaError::HttpStatus`] when the peer answers 4xx/5xx,
/// [`RichMediaError::NotHtml`] when the response's content type is not
/// HTML, [`RichMediaError::TooLarge`] when the body exceeds 1 MiB,
/// [`RichMediaError::Timeout`] when fetch and parse run past the budget,
/// [`RichMediaError::Empty`] when the page yields no card, and
/// [`RichMediaError::Blocked`] when a local policy (the SSRF guard, an
/// unparsable or non-`http(s)` URL) or a transport failure stopped the
/// request.
pub async fn fetch_card(http: &HttpClient, url: &str) -> Result<Card, RichMediaError> {
    fetch_card_within(http, url, FETCH_TIMEOUT).await
}

/// [`fetch_card`] with the overall budget as a parameter, so a timeout
/// test does not wait the full five seconds (house style: the private
/// `HttpClient::with_timeout`).
#[cfg(test)]
async fn fetch_card_with_timeout(
    http: &HttpClient,
    url: &str,
    budget: Duration,
) -> Result<Card, RichMediaError> {
    fetch_card_within(http, url, budget).await
}

/// Runs one [`fetch_and_parse`] under `budget`: whichever finishes first
/// answers, and an elapsed budget is [`RichMediaError::Timeout`].
async fn fetch_card_within(
    http: &HttpClient,
    url: &str,
    budget: Duration,
) -> Result<Card, RichMediaError> {
    match tokio::time::timeout(budget, fetch_and_parse(http, url)).await {
        Ok(outcome) => outcome,
        Err(_elapsed) => Err(RichMediaError::Timeout),
    }
}

/// One untimed card fetch: a capped `GET` asking for HTML, its status
/// checked, its content type gated, and its body read lossily into
/// [`parse_card`].
async fn fetch_and_parse(http: &HttpClient, url: &str) -> Result<Card, RichMediaError> {
    let response = http
        .get_limited(url, &[("accept", HTML_ACCEPT)], MAX_CARD_BODY_BYTES)
        .await?
        .error_for_status()?;
    if !denotes_html(&response) {
        return Err(RichMediaError::NotHtml);
    }
    let html = String::from_utf8_lossy(&response.body);
    parse_card(&html, url).ok_or(RichMediaError::Empty)
}

/// Whether a response's `content-type` names HTML: `text/html` or a
/// `+html` suffixed media type, with parameters (a charset suffix) and
/// case ignored. A missing or unrepresentable header is not HTML.
fn denotes_html(response: &HttpResponse) -> bool {
    response
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let media_type = value.trim().to_ascii_lowercase();
            media_type.starts_with("text/html") || media_type.contains("+html")
        })
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        http::{HeaderName, StatusCode, header},
        routing::get,
    };
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;

    /// `User-Agent` of the test fetch clients.
    const TEST_UA: &str = "Wired (https://wired.test)";
    /// An overall budget small enough that a stalling server loses the race.
    const SHORT_BUDGET: Duration = Duration::from_millis(50);
    /// A delay far past `SHORT_BUDGET`, served by the stalling handler.
    const STALL: Duration = Duration::from_secs(30);
    /// A minimal Open Graph page, served labelled as HTML by the mocks.
    const OG_PAGE: &str = r#"<html><head>
        <meta property="og:title" content="Fetched Title">
        <meta property="og:description" content="Fetched Description">
        <meta property="og:image" content="https://cdn.example.com/pic.png">
    </head><body></body></html>"#;

    /// Binds an unused loopback port.
    async fn bind() -> TcpListener {
        TcpListener::bind("127.0.0.1:0")
            .await
            .expect("free loopback port")
    }

    /// The `path` on `listener` as an absolute `http` URL.
    fn url_for(listener: &TcpListener, path: &str) -> String {
        format!(
            "http://{}{path}",
            listener.local_addr().expect("bound address")
        )
    }

    /// Serves `app` on `listener` until the caller aborts the task.
    fn spawn_server(listener: TcpListener, app: Router) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test server stopped: {err}");
            }
        })
    }

    /// Answers 200 with [`OG_PAGE`] labelled `text/html; charset=utf-8`.
    async fn html_page() -> ([(HeaderName, &'static str); 1], &'static str) {
        (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            OG_PAGE,
        )
    }

    /// Answers 200 with JSON: the content-type gate must refuse it.
    async fn json_page() -> ([(HeaderName, &'static str); 1], &'static str) {
        (
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"og:title":"Not HTML"}"#,
        )
    }

    /// Answers 200 with HTML that names no title: a parse, but no card.
    async fn titleless_page() -> ([(HeaderName, &'static str); 1], &'static str) {
        (
            [(header::CONTENT_TYPE, "text/html")],
            "<html><body>no title here</body></html>",
        )
    }

    /// Answers 200 with an HTML body one byte past the fetch cap.
    async fn oversized_page() -> ([(HeaderName, &'static str); 1], String) {
        (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "x".repeat(MAX_CARD_BODY_BYTES + 1),
        )
    }

    /// Answers 404 with an empty body.
    async fn missing_page() -> StatusCode {
        StatusCode::NOT_FOUND
    }

    /// Accepts the request and then never answers: only the budget ends it.
    async fn stalling_page() -> StatusCode {
        tokio::time::sleep(STALL).await;
        StatusCode::OK
    }

    /// A served Open Graph page comes back as its card, page URL and all.
    #[tokio::test]
    async fn fetch_card_parses_a_served_page() {
        let listener = bind().await;
        let url = url_for(&listener, "/post");
        let server = spawn_server(listener, Router::new().route("/post", get(html_page)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let card = fetch_card(&client, &url)
            .await
            .expect("a served OG page yields a card");

        assert_eq!(card.url, url, "the card keeps the page URL");
        assert_eq!(card.title, "Fetched Title", "og:title");
        assert_eq!(card.description, "Fetched Description", "og:description");
        assert_eq!(
            card.image.as_deref(),
            Some("https://cdn.example.com/pic.png"),
            "og:image"
        );
        server.abort();
    }

    /// A response labelled `application/json` is refused before parsing.
    #[tokio::test]
    async fn fetch_card_refuses_a_non_html_response() {
        let listener = bind().await;
        let url = url_for(&listener, "/api");
        let server = spawn_server(listener, Router::new().route("/api", get(json_page)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let result = fetch_card(&client, &url).await;

        assert_eq!(
            result.unwrap_err(),
            RichMediaError::NotHtml,
            "JSON is not a card page"
        );
        server.abort();
    }

    /// A body past the 1 MiB cap is refused as `TooLarge`.
    #[tokio::test]
    async fn fetch_card_refuses_an_oversized_body() {
        let listener = bind().await;
        let url = url_for(&listener, "/huge");
        let server = spawn_server(listener, Router::new().route("/huge", get(oversized_page)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let result = fetch_card(&client, &url).await;

        assert_eq!(
            result.unwrap_err(),
            RichMediaError::TooLarge,
            "one byte past the cap is too large"
        );
        server.abort();
    }

    /// A server that accepts and never answers loses the race against the
    /// budget and is refused as `Timeout` (short override so the test does
    /// not wait out the real 5 s).
    #[tokio::test]
    async fn a_stalling_server_times_out() {
        let listener = bind().await;
        let url = url_for(&listener, "/stall");
        let server = spawn_server(listener, Router::new().route("/stall", get(stalling_page)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let result = fetch_card_with_timeout(&client, &url, SHORT_BUDGET).await;

        assert_eq!(
            result.unwrap_err(),
            RichMediaError::Timeout,
            "the overall budget fires while the peer stalls"
        );
        server.abort();
    }

    /// A 404 answer is refused as `HttpStatus` carrying the code.
    #[tokio::test]
    async fn a_404_answer_is_a_http_status_error() {
        let listener = bind().await;
        let url = url_for(&listener, "/gone");
        let server = spawn_server(listener, Router::new().route("/gone", get(missing_page)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let result = fetch_card(&client, &url).await;

        assert_eq!(
            result.unwrap_err(),
            RichMediaError::HttpStatus(404),
            "the peer's 404 is kept as the error's payload"
        );
        server.abort();
    }

    /// An HTML page that names no title parses, but yields no card.
    #[tokio::test]
    async fn a_page_without_a_title_is_empty() {
        let listener = bind().await;
        let url = url_for(&listener, "/blank");
        let server = spawn_server(listener, Router::new().route("/blank", get(titleless_page)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let result = fetch_card(&client, &url).await;

        assert_eq!(
            result.unwrap_err(),
            RichMediaError::Empty,
            "a titleless page is no card"
        );
        server.abort();
    }

    /// An SSRF refusal, like an unparsable URL, is `Blocked`: nothing left
    /// this process.
    #[tokio::test]
    async fn refused_urls_are_blocked_before_parsing() {
        let guarded = HttpClient::new(TEST_UA);

        let loopback = fetch_card(&guarded, "http://127.0.0.1:9/page").await;
        assert_eq!(
            loopback.unwrap_err(),
            RichMediaError::Blocked,
            "the SSRF guard refuses loopback"
        );
        let junk = fetch_card(&guarded, "not a url").await;
        assert_eq!(
            junk.unwrap_err(),
            RichMediaError::Blocked,
            "an unparsable URL never leaves the process"
        );
    }

    /// `denotes_html` gates on the media type, not the whole header: the
    /// `text/html` prefix with any parameters, and `+html` suffix types,
    /// in any case; anything else — including no header at all — is not
    /// HTML.
    #[test]
    fn html_content_type_gate() {
        for (value, expected) in [
            ("text/html", true),
            ("text/html; charset=utf-8", true),
            ("Text/HTML; Charset=UTF-8", true),
            (" application/xhtml+xml;charset=gbk", false),
            ("application/feed+html", true),
            ("application/json", false),
            ("text/plain", false),
        ] {
            let response = served_with_content_type(value);
            assert_eq!(denotes_html(&response), expected, "content-type {value}");
        }
        let headerless = HttpResponse {
            status: 200,
            headers: axum::http::HeaderMap::new(),
            url: url::Url::parse("http://test.example/").unwrap(),
            body: bytes::Bytes::new(),
        };
        assert!(
            !denotes_html(&headerless),
            "no content-type header is not HTML"
        );
    }

    /// A 200 response carrying only the `content-type` header `value`.
    fn served_with_content_type(value: &str) -> HttpResponse {
        let mut headers = axum::http::HeaderMap::new();
        drop(headers.insert(
            "content-type",
            axum::http::HeaderValue::from_str(value).expect("a visible header value"),
        ));
        HttpResponse {
            status: 200,
            headers,
            url: url::Url::parse("http://test.example/").unwrap(),
            body: bytes::Bytes::from_static(b"<html></html>"),
        }
    }
}
