//! External OAuth logins: `GET /oauth/prepare_request`, the provider
//! redirect it hands the browser, the provider's callback, and the
//! registration form an unlinked provider account is picked up by.
//!
//! The reference is Pleroma's `prepare_request`, `request`, `callback`,
//! `registration_details` and `register` in
//! `lib/pleroma/web/o_auth/o_auth_controller.ex`, the Ueberauth half of
//! `/oauth`. A client that wants to log its user in through, say, GitHub runs the
//! endpoints here before it ever sees an authorization code:
//!
//! 1. `GET /oauth/prepare_request?provider=<name>&…` takes the very
//!    parameters `GET /oauth/authorize` takes — the same response type,
//!    application, redirect URI and scope checks — beside the provider
//!    name, and answers 200
//!    `{"url": "{BASE_URL}/oauth/<provider>?state=<state>"}`. The
//!    parameters are stored in `oauth_login_states` for ten minutes:
//!    Pleroma serialises them into the `state` string it hands Ueberauth,
//!    this server keeps them server side and hands out an opaque key.
//! 2. `GET /oauth/{provider}?state=<state>` reads that row back and
//!    redirects 302 to the provider's own authorization endpoint with the
//!    configured `client_id`, this instance's callback URL,
//!    `response_type=code`, the provider's `scope` and the state. An
//!    unknown, expired or foreign-provider state is the 400
//!    `{"error":"Invalid state"}`.
//! 3. `GET /oauth/{provider}/callback?code=<code>&state=<state>` validates
//!    the state again, exchanges the code at the provider's token endpoint
//!    and reads the account document from its userinfo endpoint — both
//!    through the SSRF-guarded [`HttpClient`]. The configured `uid_field`
//!    of that document names the account:
//!    * linked to a local user by `user_identities` → that account is
//!      logged in. The readiness gate ([`crate::api::oauth`]'s
//!      `check_account_ready`, the one the token endpoint applies) runs, and the answer is exactly the one
//!      `POST /oauth/authorize` gives for the *stored* parameters: a 302 to
//!      the app's `redirect_uri` with `code` (and the client's `state`), or
//!      the out-of-band code page. No consent screen is rendered and no
//!      password asked for: the provider authenticated the user, and the
//!      scopes were validated when the round trip was prepared.
//!    * not linked → the provider's account info is stored under the
//!      `pending` key of the state's parameters and the browser is
//!      redirected 302 to `{BASE_URL}/oauth/registration_details?state=…`,
//!      where the registration form takes over (4).
//!    * the provider reported an `error`, or the exchange, the read or the
//!      identification failed → 302 to the app's `redirect_uri` with
//!      `error=access_denied` (and the client's `state`), the way Pleroma
//!      bounces a failed Ueberauth callback back to where it came from.
//! 4. `GET /oauth/registration_details?state=<state>` renders that form
//!    ([`registration_page`]): the `pending` account's `nickname`, `email`
//!    and `name` prefilled and editable, the state carried hidden, posting
//!    to `POST /oauth/register`. There the account is created through
//!    [`create_local_user`] with a random password nobody can ever type —
//!    the provider is the login — the provider identity is linked in
//!    `user_identities`, and the ORIGINAL authorization-code flow finishes
//!    exactly as (3) finishes it for a linked user. `REGISTRATIONS_OPEN`
//!    gates the write (a closed instance re-renders the form with the
//!    refusal, and the invite path stays out of reach: the form carries no
//!    token); `REGISTRATIONS_APPROVAL_REQUIRED` and
//!    `ACCOUNT_ACTIVATION_REQUIRED` decide what the finished row waits for
//!    and answer the matching message page instead of the login — the
//!    provider's own email counts as the proof, so submitting it verbatim
//!    registers confirmed. Failed validation re-renders the form with the
//!    field messages; a success deletes the state row, expired states go
//!    with the hourly `maintenance.tokens` sweep, and a missing, expired
//!    or `pending`-less state is the 400 `{"error":"Invalid state"}` on
//!    both halves.
//!
//! Every answer — JSON, redirect or page — carries `Cache-Control:
//! no-store`, as the rest of `/oauth` does: a state and a code are
//! single-use material. A name `OAUTH_PROVIDERS` does not configure is the
//! 404 `{"error":"Unknown provider"}` on all three paths.
//!
//! The userinfo field names are dotted paths into the provider's JSON
//! document (`"id"`, `"data.user.id"`); a path that reads no text is a
//! failed identification, and an empty name — the `OAUTH_PROVIDERS` default
//! for the three optional ones — simply means the provider does not say.
//!
//! [`HttpClient`]: crate::http_client::HttpClient
//! [`create_local_user`]: crate::models::user::register::create_local_user
//! [`registration_page`]: crate::oauth::registration_page

use axum::{
    Json, Router,
    extract::{Path, State},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};

use crate::{
    api::oauth::authorize,
    extract::{ApiBody, ApiParams},
    state::AppState,
};

use self::{
    form::RegisterForm,
    registration::registered,
    state::{CallbackQuery, PrepareQuery, StateQuery, completed, details, prepared, redirected},
};

mod form;
mod provider;
mod registration;
mod state;

#[cfg(test)]
mod test_support;

/// 404 body when the path or the `provider` parameter names a provider
/// `OAUTH_PROVIDERS` does not configure.
const UNKNOWN_PROVIDER: &str = "Unknown provider";
/// 400 body when the `state` was never issued, has expired, or belongs to
/// another provider.
const INVALID_STATE: &str = "Invalid state";
/// The `grant_type` the provider's token endpoint is told.
const GRANT_TYPE_CODE: &str = "authorization_code";
/// The path prefix all three endpoints, and the URLs they answer, hang off.
const OAUTH_PATH: &str = "/oauth";
/// Where a prepared login is picked up: `/oauth/prepare_request`.
const PREPARE_PATH: &str = "/oauth/prepare_request";
/// The provider leg: `/oauth/{provider}`, the URL `prepare_request` answers.
const PROVIDER_PATH: &str = "/oauth/{provider}";
/// The callback leg, under the same `{provider}` segment.
const CALLBACK_PATH: &str = "/oauth/{provider}/callback";
/// The segment under `{provider}` the provider answers to.
const CALLBACK_SEGMENT: &str = "callback";
/// The page an unlinked provider account is sent to, and the route that
/// serves its form: `/oauth/registration_details`.
const REGISTRATION_DETAILS: &str = "/oauth/registration_details";
/// The form's submission endpoint: `/oauth/register`.
const REGISTER_PATH: &str = "/oauth/register";
/// The error line the form is re-rendered with while `REGISTRATIONS_OPEN`
/// is off, the wording this ticket pins. Pleroma gates the registration
/// the same way; here the form carries no invite token, so a closed
/// instance simply closes external registrations too.
const REGISTRATIONS_CLOSED_FORM: &str = "Registrations are closed";
/// Bytes behind the random password of an externally created account —
/// Pleroma's `create_from_registration` mints the same kind of secret
/// nobody will ever type. The hash exists so the row simply carries an
/// unguessable credential; every password path refuses the account.
const EXTERNAL_PASSWORD_BYTES: usize = 64;
/// The `Accept` both provider endpoints are asked for: the token exchange
/// and the userinfo read are both specified as JSON here.
const ACCEPT_JSON: &str = "application/json";
/// The `Content-Type` of the token exchange body: form parameters, as
/// RFC 6749 §4.1.3 spells them.
const FORM_TYPE: &str = "application/x-www-form-urlencoded";
/// The most bytes read from a provider's userinfo document. An account
/// description is a few kilobytes; the cap is the hostile-peer backstop.
const USERINFO_MAX_BYTES: usize = 1024 * 1024;

/// The external-login router, merged at the top level of [`crate::app`]
/// beside [`crate::api::oauth::router`].
///
/// It answers `/oauth/prepare_request`, `/oauth/registration_details`,
/// `/oauth/register`, `/oauth/{provider}` and `/oauth/{provider}/callback`.
///
/// The static `/oauth` paths — the sibling router's (`/oauth/token`,
/// `/oauth/authorize`, `/oauth/revoke`) and this router's
/// `/oauth/registration_details` and `/oauth/register` — win over the
/// `{provider}` segment: a router tries statics first — so no provider may
/// be named after one.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(PREPARE_PATH, get(prepare_request))
        .route(REGISTRATION_DETAILS, get(registration_details))
        .route(REGISTER_PATH, post(register))
        .route(PROVIDER_PATH, get(request))
        .route(CALLBACK_PATH, get(callback))
}

/// `GET /oauth/prepare_request` — validate the provider and the application
/// parameters, store them for the round trip, and answer the URL the
/// browser should visit next.
async fn prepare_request(
    State(state): State<AppState>,
    ApiParams(query): ApiParams<PrepareQuery>,
) -> Response {
    let response = match prepared(&state, query).await {
        Ok(reply) => Json(reply).into_response(),
        Err(err) => err.into_response(),
    };
    authorize::no_store(response)
}

/// `GET /oauth/{provider}` — the provider's own authorization page.
async fn request(
    State(state): State<AppState>,
    Path(name): Path<String>,
    ApiParams(query): ApiParams<StateQuery>,
) -> Response {
    let response = match redirected(&state, &name, query).await {
        Ok(reply) => reply.into_response(),
        Err(err) => err.into_response(),
    };
    authorize::no_store(response)
}

/// `GET /oauth/{provider}/callback` — the provider's answer: a login, a
/// hand-off to registration, or the app's redirect with an error.
async fn callback(
    State(state): State<AppState>,
    Path(name): Path<String>,
    ApiParams(query): ApiParams<CallbackQuery>,
) -> Response {
    let response = match completed(&state, &name, query).await {
        Ok(reply) => reply.into_response(),
        Err(err) => err.into_response(),
    };
    authorize::no_store(response)
}

/// `GET /oauth/registration_details` — the registration form an unlinked
/// provider account was handed: the state's `pending` account info
/// prefilled, posting to `/oauth/register` with the state.
async fn registration_details(
    State(state): State<AppState>,
    ApiParams(query): ApiParams<StateQuery>,
) -> Response {
    let response = match details(&state, query.state.as_deref()).await {
        Ok(reply) => reply.into_response(),
        Err(err) => err.into_response(),
    };
    authorize::no_store(response)
}

/// `POST /oauth/register` — the completed form: create the account, link
/// the provider identity, and finish the authorization-code flow the state
/// was prepared for.
async fn register(State(state): State<AppState>, ApiBody(form): ApiBody<RegisterForm>) -> Response {
    let response = match registered(&state, form).await {
        Ok(reply) => reply.into_response(),
        Err(err) => err.into_response(),
    };
    authorize::no_store(response)
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, Request, StatusCode, header};
    use tower::ServiceExt as _;

    use super::*;
    use crate::api::{
        oauth::authorize::AuthorizeReply,
        oauth_external::test_support::{header_of, test_state},
    };

    #[tokio::test]
    async fn an_unconfigured_provider_is_a_404_on_every_path() {
        for (uri, status) in [
            (
                "/oauth/prepare_request?provider=gitlab",
                StatusCode::NOT_FOUND,
            ),
            ("/oauth/gitlab?state=whatever", StatusCode::NOT_FOUND),
            (
                "/oauth/gitlab/callback?state=whatever",
                StatusCode::NOT_FOUND,
            ),
        ] {
            let response = router()
                .with_state(test_state(&["github"]))
                .oneshot(Request::get(uri).body(axum::body::Body::empty()).unwrap())
                .await
                .expect("the router answers in process");
            assert_eq!(response.status(), status, "`{uri}` is the 404");
            assert_eq!(
                header_of(&response, header::CACHE_CONTROL).as_deref(),
                Some("no-store"),
                "and the refusal is not cacheable"
            );
        }
    }

    #[tokio::test]
    async fn a_request_without_a_provider_parameter_is_the_unknown_provider_404() {
        let response = router()
            .with_state(test_state(&["github"]))
            .oneshot(
                Request::get("/oauth/prepare_request?response_type=code")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("the router answers in process");
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "no provider named is no provider found"
        );
    }

    #[tokio::test]
    async fn a_stateless_callback_and_redirect_are_the_invalid_state_400() {
        for uri in [
            "/oauth/github",
            "/oauth/github?state=",
            "/oauth/github/callback",
            "/oauth/github/callback?code=c",
        ] {
            let response = router()
                .with_state(test_state(&["github"]))
                .oneshot(Request::get(uri).body(axum::body::Body::empty()).unwrap())
                .await
                .expect("the router answers in process");
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "`{uri}` names no live state"
            );
        }
    }

    #[tokio::test]
    async fn the_sibling_oauth_paths_answer_ahead_of_the_provider_route() {
        // `/oauth/authorize` is the sibling router's static route; the
        // `{provider}` segment must not swallow it. Both refusals run
        // before any query, so the lazy pool stays untouched.
        let state = test_state(&["github"]);
        let app = crate::api::oauth::router(&state)
            .merge(router())
            .with_state(state);
        let authorize_side = app
            .oneshot(
                Request::get("/oauth/authorize?response_type=token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("the merged router answers");
        assert_eq!(
            authorize_side.status(),
            StatusCode::BAD_REQUEST,
            "`/oauth/authorize` still runs its own flow check"
        );
        assert_eq!(
            header_of(&authorize_side, header::CACHE_CONTROL).as_deref(),
            Some("no-store"),
            "and the sibling's header treatment is untouched"
        );
    }

    #[test]
    fn a_location_value_is_what_a_header_can_carry() {
        // The redirects the endpoints answer with are built by
        // `authorize::redirect_to`, which refuses a URL no header can carry.
        let ok = authorize::redirect_to("https://client.example/cb", &[("code", "abc")]);
        assert!(ok.is_ok(), "a plain URL redirects");
        let header = match ok {
            Ok(AuthorizeReply::Found(value)) => value,
            Ok(AuthorizeReply::Page(_)) | Err(_) => {
                panic!("a URL redirect, not a page")
            }
        };
        assert_eq!(
            header,
            HeaderValue::from_static("https://client.example/cb?code=abc"),
            "the Location carries the merged parameters"
        );
    }
}
