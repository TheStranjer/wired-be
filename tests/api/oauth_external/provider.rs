//! The mock provider the `oauth_external` area's flow runs against, and
//! the flow's consts: the redirect URIs, the two configured provider
//! names, the uids and the callback URI, beside the endpoints on the
//! mock's own loopback port — the authorize page a browser alone may
//! visit, the token exchange that checks its form field by field, and
//! the userinfo read whose bearer suffix selects the answer document.

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use url::form_urlencoded;
use wired_be::config::{Config, OauthProvider};

use crate::common;

/// The redirect URI the app fixture registers.
pub(super) const REDIRECT: &str = "https://client.example/callback";
/// The out-of-band redirect URI: the code is answered as JSON instead of
/// redirected (Pleroma's `@oob_token_redirect_uri`).
pub(super) const OOB: &str = "urn:ietf:wg:oauth:2.0:oob";
/// The provider name the fixture configuration answers.
pub(super) const PROVIDER: &str = "github";
/// A second configured provider, for a state issued to another one.
pub(super) const OTHER: &str = "keycloak";
/// The provider's own client id, as the fixture configuration sets it.
pub(super) const PROVIDER_CLIENT_ID: &str = "provider-client-id";
/// The provider's own client secret, as the fixture configuration sets it.
pub(super) const PROVIDER_CLIENT_SECRET: &str = "provider-client-secret";
/// The scopes the fixture provider asks the provider for.
pub(super) const PROVIDER_SCOPES: &str = "read:user user:email";
/// The uid the happy path links to a local user.
pub(super) const LINKED_UID: &str = "4242";
/// A uid nobody has linked.
pub(super) const UNLINKED_UID: &str = "90210";
/// The callback URL the provider is told to answer to.
pub(super) const CALLBACK_URI: &str = "https://test.example/oauth/github/callback";

/// The mock provider, on a loopback port of its own.
#[derive(Debug)]
pub(super) struct MockProvider {
    /// The `http://{addr}` prefix of its routes.
    pub(super) base_url: String,
    /// The task serving it, aborted when the test is done.
    server: JoinHandle<()>,
}

impl MockProvider {
    /// Binds a port and serves the mock provider until [`stop`](Self::stop).
    pub(super) async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock provider binds a port");
        let base_url = format!("http://{}", listener.local_addr().expect("a bound address"));
        let server = tokio::spawn(async move {
            let app = Router::new()
                .route("/authorize", get(authorize_page))
                .route("/token", post(token_endpoint))
                .route("/userinfo", get(userinfo_endpoint));
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock provider stopped: {err}");
            }
        });
        Self { base_url, server }
    }

    /// Stops the server.
    pub(super) fn stop(self) {
        self.server.abort();
    }

    /// The provider definition pointing at this mock, with the field names
    /// the mock's document carries at its top level.
    pub(super) fn provider(&self, name: &str) -> OauthProvider {
        OauthProvider {
            name: name.to_owned(),
            client_id: PROVIDER_CLIENT_ID.to_owned(),
            client_secret: PROVIDER_CLIENT_SECRET.to_owned(),
            authorize_url: format!("{}/authorize", self.base_url),
            token_url: format!("{}/token", self.base_url),
            userinfo_url: format!("{}/userinfo", self.base_url),
            scopes: PROVIDER_SCOPES.to_owned(),
            uid_field: "id".to_owned(),
            nickname_field: "login".to_owned(),
            email_field: "email".to_owned(),
            name_field: "name".to_owned(),
        }
    }

    /// The same definition with every field read through a dotted path into
    /// the mock's `nested` object.
    pub(super) fn nested_provider(&self, name: &str) -> OauthProvider {
        OauthProvider {
            uid_field: "nested.id".to_owned(),
            nickname_field: "nested.handle".to_owned(),
            email_field: "nested.missing".to_owned(),
            ..self.provider(name)
        }
    }

    /// [`common::test_config`] with `providers` configured.
    pub(super) fn config_with(providers: Vec<OauthProvider>) -> Config {
        let mut config = common::test_config();
        config.oauth_providers = providers;
        config
    }

    /// [`common::test_config`] with both fixture providers configured.
    pub(super) fn config(&self) -> Config {
        Self::config_with(vec![self.provider(PROVIDER), self.provider(OTHER)])
    }
}

/// The mock's authorize page. Nothing here ever fetches it: the flow hands
/// the URL to a browser, so an answer of 500 turns a server-to-server fetch
/// of the authorization endpoint into a failed login a test can see.
async fn authorize_page() -> Response {
    axum::http::Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("the authorize page is a browser's to visit"))
        .expect("a fixed response")
}

/// The token endpoint: the form exchange is checked field by field, and the
/// `code` names the scenario — `server-error` refuses the exchange,
/// `no-token` answers without an `access_token`, and anything else hands
/// out `at-{code}`.
async fn token_endpoint(headers: HeaderMap, body: String) -> Response {
    let pairs: Vec<(String, String)> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let field = |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let form_ok = headers
        .get("content-type")
        .is_some_and(|value| value == "application/x-www-form-urlencoded");
    let well_formed = form_ok
        && field("grant_type").as_deref() == Some("authorization_code")
        && field("code").is_some_and(|code| !code.is_empty())
        && field("redirect_uri").as_deref() == Some(CALLBACK_URI)
        && field("client_id").as_deref() == Some(PROVIDER_CLIENT_ID)
        && field("client_secret").as_deref() == Some(PROVIDER_CLIENT_SECRET);
    if !well_formed {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"error": "invalid_request", "sent_fields": pairs.len()}),
        );
    }
    match field("code").unwrap_or_default().as_str() {
        "server-error" => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"error": "server_error"}),
        ),
        "no-token" => json_response(StatusCode::OK, &json!({"token_type": "Bearer"})),
        code => json_response(
            StatusCode::OK,
            &json!({"access_token": format!("at-{code}"), "token_type": "Bearer"}),
        ),
    }
}

/// The userinfo endpoint: the bearer must be the one the exchange handed
/// out, and its suffix names the document — `html` answers something that is
/// not JSON, `no-uid` a document without the configured id field,
/// `forbidden` refuses the read, and anything else is read as a uid.
async fn userinfo_endpoint(headers: HeaderMap) -> Response {
    let Some(token) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer at-"))
        .map(str::to_owned)
    else {
        return json_response(StatusCode::UNAUTHORIZED, &json!({"error": "no_bearer"}));
    };
    match token.as_str() {
        "html" => axum::http::Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/html")
            .body(Body::from("<p>not JSON</p>"))
            .expect("a fixed response"),
        "forbidden" => json_response(StatusCode::FORBIDDEN, &json!({"error": "nope"})),
        "no-uid" => json_response(StatusCode::OK, &json!({"login": "nameless"})),
        uid => json_response(
            StatusCode::OK,
            &json!({
                "id": uid,
                "login": format!("user-{uid}"),
                "email": format!("user-{uid}@example.org"),
                "name": format!("User {uid}"),
                "nested": {"id": uid, "handle": format!("user-{uid}")},
            }),
        ),
    }
}

/// A JSON response with `status` and `body`.
fn json_response(status: StatusCode, body: &Value) -> Response {
    axum::http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("a fixed response")
}
