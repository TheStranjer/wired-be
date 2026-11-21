//! The mock-peer fixtures the `WebFinger` children's tests share: the
//! handles, domains and documents they answer with, the loopback servers
//! that serve them, and the clients and assertions that drive the lookup
//! against them. Test-only.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use reqwest::header::CONTENT_TYPE;
use serde_json::Value;
use tokio::{net::TcpListener, task::JoinHandle};

use crate::http_client::HttpClient;

use super::{
    URI_MACRO, URI_MACRO_ENCODED, WebfingerError, XRD_XML, finger_with_base, resolve_acct_with_base,
};

/// `User-Agent` for the test clients.
const TEST_UA: &str = "Wired (https://test.example)";
/// A `WebFinger` handle whose domain is [`DOMAIN`].
pub(super) const ACCT: &str = "alice@remote.example";
/// The domain every mock advertises as its own.
pub(super) const DOMAIN: &str = "remote.example";
/// The actor URL every correct mock links to for [`ACCT`].
pub(super) const ACTOR: &str = "https://remote.example/users/alice";
/// The namespace a real XRD document declares.
pub(super) const XRD_NAMESPACE: &str = "http://docs.oasis-open.org/ns/xri/xrd-1.0";
/// A good JRD answer about [`ACCT`]: the profile-page link first (which
/// the reader must skip), the self link second.
pub(super) const JRD_ALICE: &str = r#"{
        "subject": "acct:alice@remote.example",
        "aliases": ["https://remote.example/users/alice"],
        "links": [
            {"rel": "http://webfinger.net/rel/profile-page",
             "type": "text/html",
             "href": "https://remote.example/@alice"},
            {"rel": "self",
             "type": "application/activity+json",
             "href": "https://remote.example/users/alice"},
            {"rel": "http://ostatus.org/schema/1.0/subscribe",
             "template": "https://remote.example/ostatus_subscribe?acct={uri}"}
        ]
    }"#;
/// The same document as XRD.
pub(super) const XRD_ALICE: &str = r#"<XRD xmlns="http://docs.oasis-open.org/ns/xri/xrd-1.0">
        <Subject>acct:alice@remote.example</Subject>
        <Link rel="http://webfinger.net/rel/profile-page" type="text/html"
              href="https://remote.example/@alice"/>
        <Link rel="self" type="application/activity+json"
              href="https://remote.example/users/alice"/>
    </XRD>"#;
/// A JRD answer whose every self link names the wrong media type.
pub(super) const JRD_WRONG_TYPES: &str = r#"{"subject": "acct:alice@remote.example", "links": [
        {"rel": "self", "type": "text/html", "href": "https://remote.example/users/alice"},
        {"rel": "self", "type": "application/json", "href": "https://remote.example/wrong"},
        {"rel": "http://webfinger.net/rel/profile-page", "type": "text/html",
         "href": "https://remote.example/@alice"}
    ]}"#;
/// A JRD answer with only the profile-page link: no actor at all.
pub(super) const JRD_PROFILE_ONLY: &str = r#"{"subject": "acct:alice@remote.example", "links": [
        {"rel": "http://webfinger.net/rel/profile-page", "type": "text/html",
         "href": "https://remote.example/@alice"}
    ]}"#;
/// A JRD answer for another domain: wrong subject domain, wrong actor
/// host — nothing here belongs to [`DOMAIN`].
pub(super) const JRD_EVIL: &str = r#"{"subject": "acct:alice@evil.example", "links": [
        {"rel": "self", "type": "application/activity+json",
         "href": "https://evil.example/users/alice"}
    ]}"#;
/// A JRD answer whose subject belongs elsewhere but whose actor URL is
/// on [`DOMAIN`]: the OR arm of the containment rule passes it.
pub(super) const JRD_FOREIGN_SUBJECT_LOCAL_ACTOR: &str = r#"{"subject": "acct:alice@other.example", "links": [
        {"rel": "self", "type": "application/activity+json",
         "href": "https://remote.example/users/alice"}
    ]}"#;
/// A JRD answer whose subject is on [`DOMAIN`] but whose actor URL is on
/// the webfinger domain: allowed by the OR rule, and what a correct
/// answer must return — the foreign href.
pub(super) const JRD_LOCAL_SUBJECT_FOREIGN_ACTOR: &str = r#"{"subject": "acct:alice@remote.example", "links": [
        {"rel": "self", "type": "application/activity+json",
         "href": "https://webfinger.example/users/alice"}
    ]}"#;
/// A JRD answer with no subject at all and an actor URL on another
/// domain: with no subject to vouch for the answer, containment fails.
pub(super) const JRD_NO_SUBJECT_FOREIGN_ACTOR: &str = r#"{"links": [
        {"rel": "self", "type": "application/activity+json",
         "href": "https://evil.example/users/alice"}
    ]}"#;

/// The client under test: the SSRF guard is off, so it may reach the
/// loopback mocks (the mode `HTTP_ALLOW_PRIVATE_HOSTS` selects).
pub(super) fn client() -> HttpClient {
    HttpClient::new_for_tests(TEST_UA)
}

/// The resolved-actor assertion shared by the wire tests.
pub(super) fn assert_actor(result: Result<String, WebfingerError>, expect: &str) {
    match result {
        Ok(actor) => assert_eq!(actor, expect, "the self link's href"),
        Err(err) => panic!("resolution failed: {err}"),
    }
}

/// A router answering `path` with `body`, labelled `content_type` when
/// one is given.
pub(super) fn static_route(
    path: &'static str,
    content_type: Option<&'static str>,
    body: impl Into<String>,
) -> Router {
    /// Answer with the configured content type, or `text/plain` (what
    /// an axum `String` response carries) when the fixture declares none.
    async fn handler(State((content_type, body)): State<(Option<String>, String)>) -> Response {
        let declared = content_type.unwrap_or_else(|| "text/plain; charset=utf-8".to_owned());
        ([(CONTENT_TYPE, declared)], body).into_response()
    }
    Router::new()
        .route(path, get(handler))
        .with_state((content_type.map(str::to_owned), body.into()))
}

/// A router answering `path` with a fixed status.
pub(super) fn status_route(path: &'static str, status: StatusCode) -> Router {
    /// Reply with the configured status.
    async fn handler(State(status): State<StatusCode>) -> StatusCode {
        status
    }
    Router::new().route(path, get(handler)).with_state(status)
}

/// A host-meta route advertising an LRDD template that points back at
/// this server's `/lrdd`, using the percent-encoded macro on request.
pub(super) fn host_meta_route(encoded_macro: bool) -> Router {
    /// Advertise `<own authority>/lrdd?uri=<configured macro>` as the
    /// LRDD template, the host taken from the request so the template
    /// points back at this very mock.
    async fn handler(
        headers: HeaderMap,
        State(macro_text): State<String>,
    ) -> Result<Response, StatusCode> {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;
        let body = format!(
            r#"<XRD xmlns="{XRD_NAMESPACE}"><Link rel="lrdd" template="http://{host}/lrdd?uri={macro_text}"/></XRD>"#
        );
        Ok(([(CONTENT_TYPE, XRD_XML)], body).into_response())
    }
    let macro_text = if encoded_macro {
        URI_MACRO_ENCODED
    } else {
        URI_MACRO
    };
    Router::new()
        .route("/.well-known/host-meta", get(handler))
        .with_state(macro_text.to_owned())
}

/// Serves `app` on a loopback port; returns its task handle (to abort
/// when the test is done) and the `http://` base URL to hand
/// [`resolve_acct_with_base`] as the scheme-and-authority override.
pub(super) async fn serve(app: Router) -> (JoinHandle<()>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("test server stopped: {err}");
        }
    });
    (server, base)
}

/// Resolves `acct` against `app` served on loopback, through the
/// scheme-and-authority override.
pub(super) async fn resolve_against(app: Router, acct: &str) -> Result<String, WebfingerError> {
    let (server, base) = serve(app).await;
    let result = resolve_acct_with_base(&client(), acct, Some(&base)).await;
    server.abort();
    result
}

/// Reads the full JRD for `acct` against `app` served on loopback,
/// through the scheme-and-authority override.
pub(super) async fn finger_against(app: Router, acct: &str) -> Result<Value, WebfingerError> {
    let (server, base) = serve(app).await;
    let result = finger_with_base(&client(), acct, Some(&base)).await;
    server.abort();
    result
}

/// A router counting the requests it answers and then failing, to prove
/// a code path never reached it.
pub(super) fn counting_failure_route(path: &'static str, hits: &Arc<AtomicUsize>) -> Router {
    /// Record the hit and fail, so the count proves whether the client
    /// asked this path at all.
    async fn handler(State(counter): State<Arc<AtomicUsize>>) -> Response {
        let _previous = counter.fetch_add(1, Ordering::SeqCst);
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
    Router::new()
        .route(path, get(handler))
        .with_state(Arc::clone(hits))
}
