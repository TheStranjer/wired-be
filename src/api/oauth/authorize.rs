//! `GET`/`POST /oauth/authorize`: the authorization-code endpoint
//! (Pleroma's `OAuthController.authorize` and
//! `OAuthController.create_authorization`,
//! `lib/pleroma/web/o_auth/o_auth_controller.ex`).
//!
//! The browser half of OAuth, in two steps. The `GET` validates the
//! request — `response_type` must name the code flow, `client_id` must
//! belong to a registered application, `redirect_uri` must match one of
//! that application's registered URIs exactly, and the requested scopes
//! (default `read`) must fit inside its registration — and on success
//! renders the login-and-consent page built by
//! [`crate::oauth::authorize_page`]. The `POST` runs the same checks on
//! the submitted form, authenticates the `authorization[name]`/
//! `authorization[password]` pair with [`register::authenticate`] and,
//! on success, mints a one-time authorization code through
//! [`oauth_token::create_authorization`]. A bad login does not fail the
//! request: the page re-renders with an error line at status 200, as
//! Pleroma does. An MFA-enabled login (WBE-132) mints the same
//! authorization but hands out no code: the challenge [`mfa`] links to
//! it takes the browser to `/oauth/mfa` instead, and the verification
//! delivers the parked code from there.
//!
//! The success reply follows the client: the `urn:ietf:wg:oauth:2.0:oob`
//! out-of-band URI gets an HTML page showing the code, anything else
//! gets a 302 redirect to `redirect_uri` with `code` (and `state`, when
//! the client sent one) merged into its query. A submission that carries
//! `authorization[deny]` skips authentication and redirects with
//! `error=access_denied` instead (and `state`), never minting a code.
//!
//! Every answer — page, redirect or JSON error — carries
//! `Cache-Control: no-store`. `force_login` is accepted and inert: this
//! API-only server has no browser session, so every visit already
//! renders a fresh form (Pleroma's existing-token short-circuit joins
//! whichever ticket gives `/oauth` sessions). `lang` sets the page's
//! language attribute.

use std::fmt;

use axum::{
    Router,
    extract::State,
    http::{
        HeaderValue, StatusCode,
        header::{CACHE_CONTROL, LOCATION},
    },
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;
use url::form_urlencoded;

use super::{NO_STORE, mfa};
use crate::{
    error::{AppError, AppResult},
    extract::{ApiBody, ApiParams},
    models::{
        oauth_app::{self, OauthApp},
        oauth_token::{self, OauthAuthorization},
        user::{User, register},
    },
    oauth::{
        authorize_page::{self, ConsentPage},
        scopes::Scopes,
    },
    state::AppState,
};

/// The only response type this endpoint speaks (RFC 6749 §4.1.2).
pub(crate) const RESPONSE_TYPE_CODE: &str = "code";
/// 400 body when `response_type` is absent or names another flow; the
/// RFC 6749 §4.2.2.1 error code, spelled the way this endpoint's token
/// sibling spells `unsupported_grant_type`.
pub(crate) const UNSUPPORTED_RESPONSE_TYPE: &str = "unsupported_response_type";
/// 400 body when `client_id` names no registered application.
pub(crate) const INVALID_CLIENT: &str = "Invalid client";
/// 400 body when `redirect_uri` is not one of the app's registered URIs.
pub(crate) const INVALID_REDIRECT_URI: &str = "Invalid redirect_uri";
/// 400 body when the requested scopes reach outside the registration.
pub(crate) const INVALID_SCOPE: &str = "Invalid scope";
/// The error line of a re-rendered consent page after a failed login —
/// Pleroma's `FallbackController` flash "Invalid Username/Password",
/// shown here by re-rendering `authorize` the way Pleroma does, at
/// status 200 per the ticket.
const INVALID_LOGIN: &str = "Invalid Username/Password";
/// The out-of-band `redirect_uri` whose client reads the code off the
/// page instead of a redirect (Pleroma's `@oob_token_redirect_uri`).
pub(crate) const OOB_REDIRECT_URI: &str = "urn:ietf:wg:oauth:2.0:oob";
/// The `error` value of a denied authorization (RFC 6749 §4.1.2.1).
pub(crate) const ACCESS_DENIED: &str = "access_denied";
/// The path both halves are mounted on, and the consent form posts to.
const AUTHORIZE_PATH: &str = "/oauth/authorize";

/// The `/oauth/authorize` routes, merged into the `/oauth` router by
/// [`super::router`].
pub fn router() -> Router<AppState> {
    Router::new().route(AUTHORIZE_PATH, get(show).post(create))
}

/// Where a finished authorization goes back to: the request's registered
/// `redirect_uri` and the client's own `state`, which the reply echoes.
/// [`crate::api::oauth_external`] recovers both from a stored login state,
/// so the provider callback's success answer is built by exactly the code
/// the consent form's is.
#[derive(Debug)]
pub(crate) struct Delivery<'a> {
    /// The app's registered redirect URI verbatim, or the out-of-band
    /// [`OOB_REDIRECT_URI`] spelling.
    pub redirect_uri: &'a str,
    /// The client's `state`, when it sent one.
    pub state: Option<&'a str>,
}

/// `GET /oauth/authorize` — validate the request and render the
/// login-and-consent page. No authentication: this is where it comes
/// from. Every answer gets the no-store header.
async fn show(
    State(state): State<AppState>,
    ApiParams(query): ApiParams<AuthorizeQuery>,
) -> Response {
    let response = match consent(&state, query).await {
        Ok(page) => Html(page).into_response(),
        Err(err) => err.into_response(),
    };
    no_store(response)
}

/// `POST /oauth/authorize` — the consent form submission; same answer
/// header treatment as [`show`].
async fn create(State(state): State<AppState>, ApiBody(form): ApiBody<AuthorizeForm>) -> Response {
    let response = match authorize_submission(&state, form).await {
        Ok(reply) => reply.into_response(),
        Err(err) => err.into_response(),
    };
    no_store(response)
}

/// The `GET` half in order: response type, application, redirect URI,
/// scopes — then the rendered consent page carrying the request's
/// `state`, `lang` and scope list.
async fn consent(state: &AppState, query: AuthorizeQuery) -> AppResult<String> {
    require_code_response(query.response_type.as_deref())?;
    let app = load_app(&state.db, query.client_id.as_deref()).await?;
    let redirect_uri = query.redirect_uri.as_deref().unwrap_or_default();
    require_registered_redirect(&app, redirect_uri)?;
    let scopes = requested_scopes(query.scope.as_deref());
    check_scopes_subset(&scopes, &app)?;
    // `force_login` only matters once the endpoint can auto-approve an
    // existing session; until then every visit already renders a form.
    tracing::trace!(
        client_id = query.client_id.as_deref().unwrap_or_default(),
        force_login = query.force_login.as_deref().unwrap_or_default(),
        "authorization request validated",
    );
    Ok(authorize_page::render_consent(&ConsentPage {
        app_name: &app.client_name,
        client_id: query.client_id.as_deref().unwrap_or_default(),
        redirect_uri,
        state: query.state.as_deref(),
        scopes: scopes.names(),
        lang: query.lang.as_deref(),
        error: None,
    }))
}

/// The `POST` half in order: application, redirect URI, the denial
/// short-circuit, scopes, the login — then the MFA detour or the minted
/// code.
async fn authorize_submission(state: &AppState, form: AuthorizeForm) -> AppResult<AuthorizeReply> {
    let attrs = form.authorization;
    let app = load_app(&state.db, attrs.client_id.as_deref()).await?;
    let redirect_uri = attrs.redirect_uri.as_deref().unwrap_or_default();
    require_registered_redirect(&app, redirect_uri)?;
    if attrs.deny.is_some() {
        return denied(&attrs.delivery());
    }
    let scopes = submitted_scopes(attrs.scope.as_ref());
    check_scopes_subset(&scopes, &app)?;
    match authenticate_attrs(&state.db, &attrs).await {
        Some(user) if mfa::required(&user) => mfa_detour(state, &user, &app, &scopes, &attrs).await,
        Some(user) => grant_code(state, &user, &app, &scopes, &attrs.delivery()).await,
        None => Ok(AuthorizeReply::Page(consent_html(
            &app,
            &attrs,
            &scopes,
            Some(INVALID_LOGIN),
        ))),
    }
}

/// The MFA detour of a valid login (Pleroma's
/// `handle_create_authorization_error({:mfa_required, user, auth, _})`):
/// the authorization is created exactly as a direct approval would, the
/// challenge links it, and the browser goes to the code page instead of
/// receiving the code.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`]; a redirect
/// URI that cannot become a header value is a 400
/// [`INVALID_REDIRECT_URI`].
async fn mfa_detour(
    state: &AppState,
    user: &User,
    app: &OauthApp,
    scopes: &Scopes,
    attrs: &AuthorizationAttrs,
) -> AppResult<AuthorizeReply> {
    let challenge = mfa::start_challenge(state, user, app, scopes).await?;
    let scope_text = scopes.to_string();
    let mut params: Vec<(&str, &str)> = vec![
        ("mfa_token", challenge.as_str()),
        ("client_id", app.client_id.as_str()),
        ("scope", scope_text.as_str()),
        (
            "redirect_uri",
            attrs.redirect_uri.as_deref().unwrap_or_default(),
        ),
    ];
    if let Some(client_state) = attrs.state.as_deref() {
        params.push(("state", client_state));
    }
    redirect_to(mfa::MFA_PATH, &params)
}

/// The request parameters of `GET /oauth/authorize`. Every field is
/// optional at the shape level — the endpoint answers the missing ones
/// with its own 400 bodies (Pleroma matches on the keys), so the handler
/// checks each field.
#[derive(Debug, Deserialize)]
struct AuthorizeQuery {
    /// The flow to run; only [`RESPONSE_TYPE_CODE`] is implemented.
    #[serde(default)]
    response_type: Option<String>,
    /// The registered application's public id.
    #[serde(default)]
    client_id: Option<String>,
    /// Where the code is to be delivered; must be registered verbatim.
    #[serde(default)]
    redirect_uri: Option<String>,
    /// Requested scopes, space- or `+`-separated; absent or blank means
    /// [`Scopes::default`] (`read`).
    #[serde(default)]
    scope: Option<String>,
    /// Opaque client value, round-tripped through the form to the
    /// redirect.
    #[serde(default)]
    state: Option<String>,
    /// Fresh-login demand; accepted and inert (see the module docs).
    #[serde(default)]
    force_login: Option<String>,
    /// Page language; sets the `lang` attribute of the consent page.
    #[serde(default)]
    lang: Option<String>,
}

/// The request body of `POST /oauth/authorize`: the consent form, whose
/// fields arrive nested under `authorization` (`authorization[name]`
/// and friends, structured out of the bracketed keys by the
/// [`crate::extract`] form parser).
#[derive(Debug, Default, Deserialize)]
struct AuthorizeForm {
    /// The submitted form; absent means every field absent.
    #[serde(default)]
    authorization: AuthorizationAttrs,
}

/// The `authorization[...]` fields of the consent form. All optional:
/// the handler tells a failed check from a missing field and answers
/// each the way the flow prescribes. The `Debug` impl is manual (see
/// below) because `password` is secret material.
#[derive(Default, Deserialize)]
struct AuthorizationAttrs {
    /// The account nickname or email to log in with.
    #[serde(default)]
    name: Option<String>,
    /// That account's password. Never in `Debug` output.
    #[serde(default)]
    password: Option<String>,
    /// The application being authorized, round-tripped from the form.
    #[serde(default)]
    client_id: Option<String>,
    /// The delivery target, round-tripped from the form.
    #[serde(default)]
    redirect_uri: Option<String>,
    /// The client state, round-tripped from the form.
    #[serde(default)]
    state: Option<String>,
    /// The approved scopes: the checked `authorization[scope][]` boxes
    /// (a list; a lone scalar also arrives this way from JSON bodies).
    #[serde(default)]
    scope: Option<Value>,
    /// The Deny button's field: present, whatever its value, denies.
    #[serde(default)]
    deny: Option<Value>,
}

impl AuthorizationAttrs {
    /// The delivery target this submission names: its own `redirect_uri`
    /// and `state`, which [`grant_code`] and [`denied`] answer with.
    fn delivery(&self) -> Delivery<'_> {
        Delivery {
            redirect_uri: self.redirect_uri.as_deref().unwrap_or_default(),
            state: self.state.as_deref(),
        }
    }
}

/// [`password`](Self::password) is secret material and renders as
/// `<redacted>`; everything else shows.
impl fmt::Debug for AuthorizationAttrs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizationAttrs")
            .field("name", &self.name)
            .field("password", &"<redacted>")
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("state", &self.state)
            .field("scope", &self.scope)
            .field("deny", &self.deny)
            .finish()
    }
}

/// What a validated `POST /oauth/authorize` answers with.
#[derive(Debug)]
pub(crate) enum AuthorizeReply {
    /// 302 with this `Location`: the registered redirect URI carrying
    /// `code` (or `error=access_denied`), merged over its query.
    Found(HeaderValue),
    /// 200 `text/html` page: the re-rendered consent form or the
    /// out-of-band code.
    Page(String),
}

impl IntoResponse for AuthorizeReply {
    fn into_response(self) -> Response {
        match self {
            Self::Found(location) => (StatusCode::FOUND, [(LOCATION, location)]).into_response(),
            Self::Page(page) => Html(page).into_response(),
        }
    }
}

/// Checks `response_type`: the authorization-code flow is the only one
/// this endpoint runs. [`crate::api::oauth_external`] gates
/// `GET /oauth/prepare_request` with it, since it names the same flow.
///
/// # Errors
///
/// 400 [`UNSUPPORTED_RESPONSE_TYPE`] when the parameter is absent or
/// names anything else.
pub(crate) fn require_code_response(response_type: Option<&str>) -> AppResult<()> {
    if response_type == Some(RESPONSE_TYPE_CODE) {
        return Ok(());
    }
    Err(AppError::BadRequest(UNSUPPORTED_RESPONSE_TYPE.to_owned()))
}

/// The registered application behind `client_id`.
///
/// # Errors
///
/// 400 [`INVALID_CLIENT`] when no id is presented or it belongs to no
/// application; database failures arrive as [`AppError::Database`].
pub(crate) async fn load_app(pool: &PgPool, client_id: Option<&str>) -> AppResult<OauthApp> {
    let Some(presented) = client_id else {
        return Err(invalid_client());
    };
    let app = oauth_app::get_by_client_id(pool, presented).await?;
    app.ok_or_else(invalid_client)
}

/// The one 400 for a missing and an unknown client id alike.
pub(crate) fn invalid_client() -> AppError {
    AppError::BadRequest(INVALID_CLIENT.to_owned())
}

/// Checks `redirect_uri` against the app's registrations: an exact
/// string match against one whitespace-separated entry of the stored
/// `redirect_uris` (Pleroma's `redirect_uri in String.split(...)`).
///
/// # Errors
///
/// 400 [`INVALID_REDIRECT_URI`] when the URI is not registered.
pub(crate) fn require_registered_redirect(app: &OauthApp, redirect_uri: &str) -> AppResult<()> {
    let registered = app
        .redirect_uris
        .split_whitespace()
        .any(|entry| entry == redirect_uri);
    if registered {
        return Ok(());
    }
    Err(AppError::BadRequest(INVALID_REDIRECT_URI.to_owned()))
}

/// The scopes a `GET` asks for: parsed from `scope`, [`Scopes::default`]
/// (`read`) when the field is absent or blank — the token endpoint's
/// rule, applied to the consent page.
pub(crate) fn requested_scopes(scope: Option<&str>) -> Scopes {
    scope
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map_or_else(Scopes::default, Scopes::parse)
}

/// The scopes a `POST` was approved for: the submitted
/// `authorization[scope][]` values verbatim. No default here — the form
/// rendered the requested scopes checked, so an empty submission is the
/// user approving nothing, not a client that asked for nothing.
fn submitted_scopes(scope: Option<&Value>) -> Scopes {
    match scope {
        Some(Value::Array(items)) => {
            let joined = items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<&str>>()
                .join(" ");
            Scopes::parse(&joined)
        }
        Some(Value::String(text)) => Scopes::parse(text),
        Some(Value::Bool(_) | Value::Null | Value::Number(_) | Value::Object(_)) | None => {
            Scopes::parse("")
        }
    }
}

/// Checks the requested scopes against the registration.
///
/// # Errors
///
/// 400 [`INVALID_SCOPE`] when any requested scope is not covered by the
/// app's registered scopes ([`Scopes::is_subset_of`]).
pub(crate) fn check_scopes_subset(requested: &Scopes, app: &OauthApp) -> AppResult<()> {
    if requested.is_subset_of(&Scopes::parse(&app.scopes)) {
        return Ok(());
    }
    Err(AppError::BadRequest(INVALID_SCOPE.to_owned()))
}

/// The submitted login, verified.
///
/// A missing name or password, a wrong pair, an unknown or remote
/// account and a deactivated account all answer `None`:
/// [`register::authenticate`] is the strict gate, and every one of those
/// cases re-renders the form with the same error line — Pleroma does not
/// tell them apart on this page either.
async fn authenticate_attrs(pool: &PgPool, attrs: &AuthorizationAttrs) -> Option<User> {
    let name = attrs.name.as_deref()?;
    let password = attrs.password.as_deref()?;
    register::authenticate(pool, name, password).await
}

/// Mints the authorization code for the approved scopes and answers with
/// it (Pleroma's `after_create_authorization`). An MFA-enabled account
/// never gets here: [`authorize_submission`] parks the authorization
/// through [`mfa::start_challenge`] and detours to the code page instead.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`]; a redirect
/// URI that cannot become a header value is a 400
/// [`INVALID_REDIRECT_URI`].
pub(crate) async fn grant_code(
    state: &AppState,
    user: &User,
    app: &OauthApp,
    scopes: &Scopes,
    delivery: &Delivery<'_>,
) -> AppResult<AuthorizeReply> {
    let authorization =
        oauth_token::create_authorization(&state.db, user.id, app.id, &scopes.to_string()).await?;
    deliver_code(&authorization, delivery)
}

/// The answer that hands a finished authorization's code to the client
/// (Pleroma's `after_create_authorization`): the out-of-band page for
/// the `urn:ietf:wg:oauth:2.0:oob` client, the 302 carrying `code` (and
/// `state`) for every other URI. [`crate::api::oauth::mfa`] delivers
/// through here too, which is what makes a completed MFA verification's
/// answer *exactly* this one.
///
/// # Errors
///
/// A redirect URI that cannot become a header value is a 400
/// [`INVALID_REDIRECT_URI`].
pub(crate) fn deliver_code(
    authorization: &OauthAuthorization,
    delivery: &Delivery<'_>,
) -> AppResult<AuthorizeReply> {
    if delivery.redirect_uri == OOB_REDIRECT_URI {
        return Ok(AuthorizeReply::Page(authorize_page::render_oob(
            &authorization.token,
        )));
    }
    let mut params: Vec<(&str, &str)> = vec![("code", authorization.token.as_str())];
    if let Some(client_state) = delivery.state {
        params.push(("state", client_state));
    }
    redirect_to(delivery.redirect_uri, &params)
}

/// The denial reply: `error=access_denied` (and `state`) on the
/// registered URI; no code was, and none is, minted. Pleroma's callback
/// redirects its provider failures the same way, so
/// [`crate::api::oauth_external`] shares it.
///
/// # Errors
///
/// A redirect URI that cannot become a header value is a 400
/// [`INVALID_REDIRECT_URI`].
pub(crate) fn denied(delivery: &Delivery<'_>) -> AppResult<AuthorizeReply> {
    let mut params: Vec<(&str, &str)> = vec![("error", ACCESS_DENIED)];
    if let Some(client_state) = delivery.state {
        params.push(("state", client_state));
    }
    redirect_to(delivery.redirect_uri, &params)
}

/// A 302 to `uri` with `params` in its query, the `Location` validated
/// as a header value on the way.
pub(crate) fn redirect_to(uri: &str, params: &[(&str, &str)]) -> AppResult<AuthorizeReply> {
    let url = with_query_params(uri, params);
    let location = HeaderValue::from_str(&url)
        .map_err(|_header_error| AppError::BadRequest(INVALID_REDIRECT_URI.to_owned()))?;
    Ok(AuthorizeReply::Found(location))
}

/// The consent page as the `POST` re-renders it: the same document the
/// `GET` served, its round-trip fields taken from the submitted form and
/// `error` shown above the request text.
fn consent_html(
    app: &OauthApp,
    attrs: &AuthorizationAttrs,
    scopes: &Scopes,
    error: Option<&str>,
) -> String {
    authorize_page::render_consent(&ConsentPage {
        app_name: &app.client_name,
        client_id: attrs.client_id.as_deref().unwrap_or_default(),
        redirect_uri: attrs.redirect_uri.as_deref().unwrap_or_default(),
        state: attrs.state.as_deref(),
        scopes: scopes.names(),
        lang: None,
        error,
    })
}

/// `uri` with `params` added to its query, Pleroma's
/// `UriHelper.modify_uri_params`: existing pairs are kept in order, a
/// pair whose key is re-added takes the new value, and a fragment stays
/// at the end. Values are urlencoded, so a `state` holding `&`, spaces
/// or `=` survives the round trip.
pub(crate) fn with_query_params(uri: &str, params: &[(&str, &str)]) -> String {
    let (uri, fragment) = uri
        .split_once('#')
        .map_or((uri, ""), |(base, frag)| (base, frag));
    let (base, existing) = uri
        .split_once('?')
        .map_or((uri, ""), |(path, query)| (path, query));
    let mut kept: Vec<(String, String)> = form_urlencoded::parse(existing.as_bytes())
        .into_owned()
        .collect();
    kept.retain(|(key, _)| !params.iter().any(|&(new_key, _)| new_key == key.as_str()));
    let query = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            kept.iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .extend_pairs(params.iter().copied())
        .finish();
    let mut url = String::with_capacity(base.len() + query.len() + 2);
    url.push_str(base);
    if !query.is_empty() {
        url.push('?');
        url.push_str(&query);
    }
    if !fragment.is_empty() {
        url.push('#');
        url.push_str(fragment);
    }
    url
}

/// Stamps the `Cache-Control: no-store` every authorize answer carries:
/// the consent page holds the live redirect target and state, and a code
/// is single-use material.
pub(crate) fn no_store(mut response: Response) -> Response {
    let stored: Option<HeaderValue> = response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
    drop(stored);
    response
}

#[cfg(test)]
mod tests {
    use axum::http::{Request, header::CONTENT_TYPE};
    use chrono::Utc;
    use serde_json::json;
    use std::sync::Arc;
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use super::*;
    use crate::{config::Config, http_client::HttpClient, uploads::UploaderBackend};

    /// The redirect URI the [`app_row`] fixture registers.
    const REGISTERED: &str = "https://client.example/callback";

    /// A stored-looking app row: `scopes` registered, `REGISTERED` and
    /// the out-of-band URI as redirect URIs.
    fn app_row(scopes: &str) -> OauthApp {
        let now = Utc::now();
        OauthApp {
            id: Uuid::now_v7(),
            client_name: "Client".to_owned(),
            redirect_uris: format!("{REGISTERED} {OOB_REDIRECT_URI}"),
            scopes: scopes.to_owned(),
            website: None,
            client_id: "client-id".to_owned(),
            client_secret: "client-secret".to_owned(),
            trusted: false,
            user_id: None,
            inserted_at: now,
            updated_at: now,
        }
    }

    /// The message an [`AppError`] answers with.
    fn message_of(err: &AppError) -> String {
        err.to_string()
    }

    #[test]
    fn the_code_response_type_is_the_only_one_accepted() {
        require_code_response(Some(RESPONSE_TYPE_CODE)).expect("code is the code flow");
        for rejected in [None, Some(""), Some("token"), Some("CODE")] {
            let err = require_code_response(rejected).unwrap_err();
            assert_eq!(
                message_of(&err),
                UNSUPPORTED_RESPONSE_TYPE,
                "`{rejected:?}` names no implemented flow"
            );
        }
    }

    #[test]
    fn redirect_uris_must_match_a_registered_entry_exactly() {
        let app = app_row("read");
        require_registered_redirect(&app, REGISTERED).expect("a space-separated entry");
        require_registered_redirect(&app, OOB_REDIRECT_URI).expect("the out-of-band uri");
        let newlined = OauthApp {
            redirect_uris: format!("{REGISTERED}\n{OOB_REDIRECT_URI}"),
            ..app_row("read")
        };
        require_registered_redirect(&newlined, REGISTERED).expect("newline-separated too");
        for presented in [
            "",
            &format!("{REGISTERED}/extra"),
            "https://client.example",
            &format!("{REGISTERED}?x=1"),
            &format!("{REGISTERED} "),
        ] {
            let err = require_registered_redirect(&app, presented).unwrap_err();
            assert_eq!(
                message_of(&err),
                INVALID_REDIRECT_URI,
                "`{presented:?}` is not the registered spelling"
            );
        }
    }

    #[test]
    fn requested_scopes_default_to_read() {
        for blank in [None, Some(""), Some("   ")] {
            let scopes = requested_scopes(blank);
            assert_eq!(
                scopes.to_string(),
                Scopes::default().to_string(),
                "`{blank:?}` falls back to the default"
            );
        }
        assert_eq!(
            requested_scopes(Some("read+write")).to_string(),
            "read write",
            "an encoded query separator parses"
        );
    }

    #[test]
    fn submitted_scopes_read_the_checkbox_list() {
        let checked = submitted_scopes(Some(&json!(["read", "write"])));
        assert_eq!(checked.to_string(), "read write", "one per box");
        let lone = submitted_scopes(Some(&json!("admin:read")));
        assert_eq!(lone.to_string(), "admin:read", "a lone scalar counts too");
        assert_eq!(
            submitted_scopes(Some(&json!([]))).to_string(),
            "",
            "unchecking every box approves nothing"
        );
        assert_eq!(
            submitted_scopes(None).to_string(),
            "",
            "and a form without the field"
        );
        assert_eq!(
            submitted_scopes(Some(&json!(7))).to_string(),
            "",
            "a value that names no scope approves nothing"
        );
    }

    #[test]
    fn scopes_must_fit_inside_the_registration() {
        let app = app_row("read write");
        check_scopes_subset(&Scopes::parse("read:accounts"), &app).expect("sub-scope of read");
        check_scopes_subset(&Scopes::parse(""), &app).expect("approving nothing fits");
        let err = check_scopes_subset(&Scopes::parse("read follow"), &app).unwrap_err();
        assert_eq!(
            message_of(&err),
            INVALID_SCOPE,
            "unregistered follow is refused"
        );
    }

    #[test]
    fn query_deserializes_every_wire_field() {
        let query: AuthorizeQuery = crate::extract::into_params(json!({
            "response_type": "code",
            "client_id": "client-id",
            "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
            "scope": "read write",
            "state": "st",
            "force_login": "true",
            "lang": "de",
        }))
        .unwrap();
        assert_eq!(
            query.response_type.as_deref(),
            Some(RESPONSE_TYPE_CODE),
            "the flow"
        );
        assert_eq!(query.redirect_uri.as_deref(), Some(OOB_REDIRECT_URI));
        assert_eq!(query.state.as_deref(), Some("st"), "the round trip");
        assert_eq!(query.force_login.as_deref(), Some("true"), "login demand");
        assert_eq!(query.lang.as_deref(), Some("de"), "language");
    }

    #[test]
    fn form_deserializes_the_nested_authorization_object() {
        // `extract::parse_form` turns the posted `authorization[name]`
        // keys into exactly this shape (its own tests cover that step);
        // this test covers the struct reading it back.
        let form: AuthorizeForm = crate::extract::into_params(json!({
            "authorization": {
                "name": "alice",
                "password": "pw",
                "client_id": "cid",
                "redirect_uri": "cb",
                "state": "st",
                "scope": ["read", "write"],
                "deny": "Deny",
            }
        }))
        .unwrap();
        let attrs = form.authorization;
        assert_eq!(attrs.name.as_deref(), Some("alice"), "the login name");
        assert_eq!(attrs.password.as_deref(), Some("pw"), "the password");
        assert_eq!(attrs.client_id.as_deref(), Some("cid"), "the client");
        assert_eq!(
            attrs.scope,
            Some(json!(["read", "write"])),
            "the checked boxes"
        );
        assert!(attrs.deny.is_some(), "the deny marker survives");

        let empty: AuthorizeForm = crate::extract::into_params(json!({})).unwrap();
        assert_eq!(
            empty.authorization.client_id, None,
            "an absent form reads as all-absent fields"
        );
    }

    #[test]
    fn form_debug_redacts_the_password() {
        let form: AuthorizeForm = crate::extract::into_params(json!({
            "authorization": { "name": "alice", "password": "s3cr3t" }
        }))
        .unwrap();
        let debug = format!("{form:?}");
        assert!(!debug.contains("s3cr3t"), "the password must not leak");
        assert!(
            debug.contains(r#"password: "<redacted>""#),
            "the marker names the field: {debug}"
        );
        assert!(debug.contains("alice"), "the rest shows");
    }

    #[test]
    fn query_params_are_merged_and_replaced() {
        let appended = with_query_params(REGISTERED, &[("code", "abc")]);
        assert_eq!(appended, format!("{REGISTERED}?code=abc"), "a bare uri");

        let merged = with_query_params(
            &format!("{REGISTERED}?foo=1&state=old&code=stale"),
            &[("code", "new"), ("state", "x")],
        );
        assert_eq!(
            merged,
            format!("{REGISTERED}?foo=1&code=new&state=x"),
            "existing pairs are kept, replaced keys take the new value"
        );

        let kept_fragment =
            with_query_params(&format!("{REGISTERED}?x=1#frag"), &[("code", "abc")]);
        assert_eq!(
            kept_fragment,
            format!("{REGISTERED}?x=1&code=abc#frag"),
            "the fragment stays at the end"
        );

        let encoded = with_query_params(REGISTERED, &[("state", "a b&c=d")]);
        assert_eq!(
            encoded,
            format!("{REGISTERED}?state=a+b%26c%3Dd"),
            "values are urlencoded"
        );

        assert_eq!(
            with_query_params(&format!("{REGISTERED}?"), &[]),
            REGISTERED,
            "an emptied query drops the question mark"
        );
    }

    #[test]
    fn a_found_reply_is_a_302_with_a_location() {
        let location = format!("{REGISTERED}?code=abc");
        let reply = AuthorizeReply::Found(HeaderValue::from_str(&location).unwrap());
        let response = reply.into_response();
        assert_eq!(response.status(), StatusCode::FOUND, "302");
        assert_eq!(
            response.headers().get(LOCATION),
            Some(&HeaderValue::from_str(&location).unwrap()),
            "the exact Location"
        );
    }

    #[test]
    fn a_page_reply_is_200_html() {
        let response = AuthorizeReply::Page("<p>ok</p>".to_owned()).into_response();
        assert_eq!(response.status(), StatusCode::OK, "the ticket's 200");
        assert_eq!(
            response.headers().get(CONTENT_TYPE),
            Some(&HeaderValue::from_static("text/html; charset=utf-8")),
            "and its content type"
        );
    }

    #[test]
    fn no_store_sets_and_replaces_the_cache_control() {
        let mut response = Html("x".to_owned()).into_response();
        let absent: Option<HeaderValue> = response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("max-age=60"));
        assert!(absent.is_none(), "an html answer starts without one");
        let response = no_store(response);
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&HeaderValue::from_static(NO_STORE)),
            "no-store wins"
        );
    }

    /// The state the route-shaped tests below need: config only, the
    /// lazy pool is never consulted (the validation gates that fail here
    /// run before any query).
    fn test_state() -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: PgPool::connect_lazy("postgres://localhost/unused").unwrap(),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
        }
    }

    #[tokio::test]
    async fn get_answers_at_oauth_authorize_and_refuses_other_flows() {
        let app = router().with_state(test_state());
        let request = Request::get("/oauth/authorize?response_type=token&client_id=whatever")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "the routed endpoint answers and rejects the flow"
        );
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&HeaderValue::from_static(NO_STORE)),
            "and stamps no-store"
        );
    }

    #[tokio::test]
    async fn post_without_an_authorization_object_is_an_invalid_client() {
        let app = router().with_state(test_state());
        let request = Request::post(AUTHORIZE_PATH)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(axum::body::Body::from("name=alice"))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "a form with no authorization fields cannot name a client"
        );
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&HeaderValue::from_static(NO_STORE)),
            "even the rejection is not cacheable"
        );
    }
}
