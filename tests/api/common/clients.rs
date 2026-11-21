//! The end-to-end scenario helpers: registering clients and accounts,
//! logging in and the bearer-optional request shortcuts.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};

use super::{TestRequest, TestResponse, assert_answer};

// ─────────────────────  The end-to-end scenario helpers  ─────────────────────
//
// The `e2e_*.rs` modules drive whole journeys through the public HTTP API
// and assert what the API answers. These are the three or four calls every
// such journey starts from — register a client, register an account with
// it, log in — plus the bearer-optional request shortcuts they repeat.
// Area tests that need a row in a state no endpoint can put it in keep
// using the model fixtures above; a scenario needs neither.

/// The redirect URI every scenario client registers: the address the mock
/// consent form posts back to and the `/oauth/authorize` tests redirect on.
pub const TEST_REDIRECT_URI: &str = "https://client.example/callback";

/// An application a scenario registered over `POST /api/v1/apps`: the
/// credential pair `POST /oauth/token` authenticates, with the scopes the
/// registration asked for (which bound every grant it can win).
#[derive(Clone, Debug)]
pub struct TestClient {
    /// The public client id, `client_id` on the wire.
    pub client_id: String,
    /// The client secret, `client_secret` on the wire.
    pub client_secret: String,
    /// The registered scopes, space-separated, as the answer renders them.
    pub scopes: String,
}

impl TestClient {
    /// The client's credential pair merged into a token-endpoint `body`:
    /// the shape a scenario that builds its own grant body needs.
    ///
    /// # Panics
    ///
    /// If `body` is not a JSON object; every grant body is one.
    #[must_use]
    pub fn with_credentials(&self, body: Value) -> Value {
        let mut merged = body;
        let map = merged
            .as_object_mut()
            .expect("a grant body is a JSON object");
        for (key, value) in [
            ("client_id", self.client_id.clone()),
            ("client_secret", self.client_secret.clone()),
        ] {
            let _credential = map.insert(key.to_owned(), Value::String(value));
        }
        merged
    }
}

/// Registers a client named `name` over `POST /api/v1/apps` and hands back
/// its credentials.
///
/// This is a scenario's first call, since registration and the password
/// grant both need a client and no fixture supplies one.
///
/// # Panics
///
/// If the registration is refused or its answer is missing a credential.
pub async fn register_client(app: &Router, name: &str, scopes: &str) -> TestClient {
    let response = TestRequest::post("/api/v1/apps")
        .json(&json!({
            "client_name": name,
            "redirect_uris": TEST_REDIRECT_URI,
            "scopes": scopes,
        }))
        .send(app)
        .await;
    assert_answer(&response, StatusCode::OK, "the scenario's app registration");
    let body = response.json_value();
    TestClient {
        client_id: string_field(&body, "client_id"),
        client_secret: string_field(&body, "client_secret"),
        scopes: registered_scopes(&body, scopes),
    }
}

/// The string field `key` of a JSON object answer.
///
/// # Panics
///
/// If the key is absent or holds something other than a string.
fn string_field(body: &Value, key: &str) -> String {
    body[key]
        .as_str()
        .unwrap_or_else(|| panic!("the answer carries a string {key}: {body}"))
        .to_owned()
}

/// The scopes `POST /api/v1/apps` answered: its entity lists them as a
/// JSON array, which the token endpoint takes back as one
/// space-separated string. `None` when the answer carries no list.
///
/// # Panics
///
/// If the list holds something other than a string.
fn registered_scopes(body: &Value, asked: &str) -> String {
    body["scopes"].as_array().map_or_else(
        || asked.to_owned(),
        |list| {
            list.iter()
                .map(|scope| {
                    scope
                        .as_str()
                        .unwrap_or_else(|| panic!("a scope is a string: {scope}"))
                })
                .collect::<Vec<_>>()
                .join(" ")
        },
    )
}

/// The `client_credentials` token of `client`: the app-only bearer a
/// scenario presents to endpoints that need *a* valid token and no user,
/// above all `POST /api/v1/accounts`.
///
/// # Panics
///
/// If the grant is refused or answers no `access_token`.
pub async fn client_credentials_token(app: &Router, client: &TestClient) -> String {
    let response = TestRequest::post("/oauth/token")
        .json(&client.with_credentials(json!({
            "grant_type": "client_credentials",
            "scope": client.scopes,
        })))
        .send(app)
        .await;
    assert_answer(&response, StatusCode::OK, "the client_credentials grant");
    access_token_of(&response)
}

/// The `access_token` of a 200 token answer.
///
/// # Panics
///
/// If the body carries no string `access_token`.
#[must_use]
pub fn access_token_of(response: &TestResponse) -> String {
    string_field(&response.json_value(), "access_token")
}

/// Registers `username` (at `username@example.org`, with `password`) over
/// `POST /api/v1/accounts` and hands back the access token the
/// registration issued for the new account.
///
/// It presents `client`'s `client_credentials` token, the credential the
/// route takes. The account is a real one: the password is the submitted
/// one, so [`login_password`] works for it from then on.
///
/// # Panics
///
/// If the registration is refused or answers no `access_token`.
pub async fn register_via_api(
    app: &Router,
    client: &TestClient,
    username: &str,
    password: &str,
) -> String {
    let bearer = client_credentials_token(app, client).await;
    let response = TestRequest::post("/api/v1/accounts")
        .bearer(&bearer)
        .json(&json!({
            "username": username,
            "email": format!("{username}@example.org"),
            "password": password,
            "agreement": true,
        }))
        .send(app)
        .await;
    assert_answer(&response, StatusCode::OK, "the scenario's registration");
    access_token_of(&response)
}

/// `POST /oauth/token` with the `password` grant for `username`, answered
/// raw.
///
/// A scenario asserts a refusal (a pending account, a wrong password, an
/// MFA challenge) as well as a login with the answer. The grant asks for
/// the client's whole registered scope set.
pub async fn login_password(
    app: &Router,
    client: &TestClient,
    username: &str,
    password: &str,
) -> TestResponse {
    TestRequest::post("/oauth/token")
        .json(&client.with_credentials(json!({
            "grant_type": "password",
            "username": username,
            "password": password,
            "scope": client.scopes,
        })))
        .send(app)
        .await
}

/// `GET`s `path` through `app`, presenting `token` when the scenario has
/// one to present (`None` is the anonymous call).
pub async fn api_get(app: &Router, token: Option<&str>, path: &str) -> TestResponse {
    let request = TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `POST`s `body` to `path` as JSON, with `token` when the scenario has
/// one. A body-less action write passes [`Value::Null`] and sends no body
/// at all, which is how a client posts a follow or a favourite.
pub async fn api_post(app: &Router, token: Option<&str>, path: &str, body: &Value) -> TestResponse {
    with_optional_body(TestRequest::post(path), token, body)
        .send(app)
        .await
}

/// `PATCH`es `path` as JSON with `body`, with `token` when given.
pub async fn api_patch(
    app: &Router,
    token: Option<&str>,
    path: &str,
    body: &Value,
) -> TestResponse {
    with_optional_body(TestRequest::patch(path), token, body)
        .send(app)
        .await
}

/// `PUT`s `path` as JSON with `body`, with `token` when given.
pub async fn api_put(app: &Router, token: Option<&str>, path: &str, body: &Value) -> TestResponse {
    with_optional_body(TestRequest::put(path), token, body)
        .send(app)
        .await
}

/// `DELETE`s `path`, with `token` and — for the admin writes that take a
/// body on `DELETE` — a JSON `body`. A null `body` sends none.
pub async fn api_delete(
    app: &Router,
    token: Option<&str>,
    path: &str,
    body: &Value,
) -> TestResponse {
    with_optional_body(TestRequest::delete(path), token, body)
        .send(app)
        .await
}

/// Adds the bearer and the JSON body a write of [`api_post`],
/// [`api_patch`], [`api_put`] or [`api_delete`] spells.
fn with_optional_body<'a>(
    request: TestRequest,
    token: Option<&'a str>,
    body: &'a Value,
) -> TestRequest {
    let with_bearer = match token {
        Some(found) => request.bearer(found),
        None => request,
    };
    match body.as_object() {
        Some(_) => with_bearer.json(body),
        None => with_bearer,
    }
}

/// The `id` of the entity a 200 answer carries, as text.
///
/// # Panics
///
/// If the body has no string `id`.
#[must_use]
pub fn entity_id(response: &TestResponse) -> String {
    string_field(&response.json_value(), "id")
}
