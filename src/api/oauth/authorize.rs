//! `GET`/`POST /oauth/authorize`: the authorization-code endpoint
//! (Pleroma's `OAuthController.authorize` and
//! `OAuthController.create_authorization`,
//! `lib/pleroma/web/o_auth/o_auth_controller.ex`).
//!
//! This server renders no consent screen: it lives at
//! `{FRONTEND_BASE_URL}/oauth/authorize`
//! ([`crate::config::Config::frontend_base_url`]).
//! The endpoint is the API behind it.
//!
//! `GET` without `Accept: application/json` is the browser's entry point.
//! It validates the request — `response_type` must name the code flow,
//! `client_id` must belong to a registered application, `redirect_uri`
//! must match one of that application's registered URIs exactly, and the
//! requested scopes (default `read`) must fit inside its registration —
//! and on success answers `302` to the frontend's own
//! `/oauth/authorize` with the original query string, re-encoded. A
//! request that fails validation is the JSON 400 and never a redirect, so
//! nothing is ever bounced to an unvalidated `redirect_uri`.
//!
//! The JSON half (`authorize/json.rs`) is what the frontend then talks
//! to: `GET` with `Accept: application/json` describes the request, and
//! `POST` with a bearer token and a JSON body approves or denies it, but
//! only for a token whose own app is trusted (WBE-205, WBE-666): admins
//! set the `oauth_apps.trusted` flag through
//! `PATCH /api/v1/pleroma/admin/oauth_app/{id}`, or automatically (WBE-822,
//! [`crate::oauth::frontend_trust`]) for an app registered with every
//! `redirect_uri` on the origin of `FRONTEND_BASE_URL`, which is how
//! `WiredFE`'s per-browser app becomes trusted; the `GET` that describes
//! a request never depends on trust. Any other `POST` — in particular
//! the retired `authorization[name]`/`authorization[password]` form post —
//! is a 400.
//!
//! The helpers below also serve the external-login callback
//! ([`crate::api::oauth_external`]), which finishes an authorization on
//! its own: `grant_code` mints the code and
//! `deliver_code` hands it to the client — a 302 to `redirect_uri` with
//! `code` (and `state`), or `{"code": …}` JSON for the
//! `urn:ietf:wg:oauth:2.0:oob` out-of-band URI.
//!
//! Every answer — redirect or JSON — carries `Cache-Control: no-store`.

use axum::{
    Json, Router,
    extract::State,
    http::{
        HeaderMap, HeaderValue, StatusCode, Uri,
        header::{CACHE_CONTROL, LOCATION},
    },
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use url::form_urlencoded;

use super::{NO_STORE, json_media::wants_json};
use crate::{
    error::{AppError, AppResult},
    extract::{ApiBody, ApiParams},
    models::{
        oauth_app::{self, OauthApp},
        oauth_token::{self, CodeBinding, OauthAuthorization},
        user::User,
    },
    oauth::{pkce, scopes::Scopes},
    state::AppState,
};

mod json;

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
/// The out-of-band `redirect_uri` whose client reads the code off the
/// answer instead of a redirect (Pleroma's `@oob_token_redirect_uri`).
pub(crate) const OOB_REDIRECT_URI: &str = "urn:ietf:wg:oauth:2.0:oob";
/// The `error` value of a denied authorization (RFC 6749 §4.1.2.1).
pub(crate) const ACCESS_DENIED: &str = "access_denied";
/// The path both halves are mounted on, and the one the frontend serves
/// its consent screen at.
const AUTHORIZE_PATH: &str = "/oauth/authorize";

/// The `/oauth/authorize` routes, merged into the `/oauth` router by
/// [`super::router`], in the limiter's `authentication` bucket.
///
/// Pleroma throttles the whole path, `GET` and `POST` alike (see
/// [`crate::rate_limit`]).
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new().route(
        AUTHORIZE_PATH,
        get(show)
            .post(create)
            .layer(crate::rate_limit::rate_limit(state, "authentication")),
    )
}

/// Where a finished authorization goes back to: the request's registered
/// `redirect_uri` and the client's own `state`, which the reply echoes.
/// [`crate::api::oauth_external`] recovers both from a stored login state,
/// so the provider callback's success answer is built by exactly the code
/// the MFA verification's is.
#[derive(Debug)]
pub(crate) struct Delivery<'a> {
    /// The app's registered redirect URI verbatim, or the out-of-band
    /// [`OOB_REDIRECT_URI`] spelling.
    pub redirect_uri: &'a str,
    /// The client's `state`, when it sent one.
    pub state: Option<&'a str>,
    /// The validated PKCE challenge a minted code is bound to, when the
    /// client sent one.
    pub code_challenge: Option<&'a str>,
}

/// `GET /oauth/authorize` — validate the request and send the browser on
/// to the frontend's consent screen ([`redirect_to_frontend`]).
/// No authentication: this is where it comes from.
/// `Accept: application/json` gets [`json::describe`]'s JSON instead.
/// Every answer gets the no-store header.
async fn show(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    ApiParams(query): ApiParams<AuthorizeQuery>,
) -> Response {
    let response = if wants_json(&headers) {
        json::describe(&state, &query).await
    } else {
        match redirect_to_frontend(&state, &query, &uri).await {
            Ok(found) => found,
            Err(err) => err.into_response(),
        }
    };
    no_store(response)
}

/// `POST /oauth/authorize` — the frontend's verdict ([`json::approve`])
/// when it carries a bearer token or a JSON body, a 400
/// ([`json::refuse_form`]) for anything else; same answer header
/// treatment as [`show`].
async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiBody(body): ApiBody<Value>,
) -> Response {
    let response = if json::is_json_approval(&headers, &body) {
        json::approve(&state, &headers, body).await
    } else {
        json::refuse_form()
    };
    no_store(response)
}

/// The browser `GET` in order: response type, application, redirect URI,
/// scopes — then the 302 to the frontend's consent screen carrying the
/// request's query.
///
/// # Errors
///
/// The validators' 400s, a database failure, and an
/// [`AppError::Internal`] when the configured frontend URL and the query
/// do not make a header value.
async fn redirect_to_frontend(
    state: &AppState,
    query: &AuthorizeQuery,
    uri: &Uri,
) -> AppResult<Response> {
    require_code_response(query.response_type.as_deref())?;
    let app = load_app(&state.db, query.client_id.as_deref()).await?;
    require_registered_redirect(&app, query.redirect_uri.as_deref().unwrap_or_default())?;
    check_scopes_subset(&requested_scopes(query.scope.as_deref()), &app)?;
    query.require_valid_pkce()?;
    let location = frontend_location(
        &state.config.frontend_base_url,
        uri.query().unwrap_or_default(),
    )?;
    Ok((StatusCode::FOUND, [(LOCATION, location)]).into_response())
}

/// The frontend's consent screen URL: `base` and [`AUTHORIZE_PATH`], then
/// the request's raw `query` decoded and urlencoded again pair by pair,
/// in order, unknown parameters (PKCE and the like) included.
///
/// # Errors
///
/// [`AppError::Internal`] when the URL is not a header value (nothing a
/// configured frontend URL and an urlencoded query can produce).
fn frontend_location(base: &str, query: &str) -> AppResult<HeaderValue> {
    let encoded = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form_urlencoded::parse(query.as_bytes()))
        .finish();
    let mut url = format!("{base}{AUTHORIZE_PATH}");
    if !encoded.is_empty() {
        url.push('?');
        url.push_str(&encoded);
    }
    HeaderValue::from_str(&url).map_err(|err| {
        AppError::Internal(format!(
            "the frontend authorize redirect is not header-safe: {err}"
        ))
    })
}

/// The request parameters of `GET /oauth/authorize`. Every field is
/// optional at the shape level — the endpoint answers the missing ones
/// with its own 400 bodies (Pleroma matches on the keys), so the handler
/// checks each field. Unknown parameters are ignored here and still
/// ride along in the redirect.
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
    /// Opaque client value, echoed by the JSON description.
    #[serde(default)]
    state: Option<String>,
    /// The PKCE challenge (RFC 7636) the code is to be bound to; see
    /// [`pkce::parse_challenge`].
    #[serde(default)]
    code_challenge: Option<String>,
    /// How the challenge was derived; only `S256` is accepted.
    #[serde(default)]
    code_challenge_method: Option<String>,
}

impl AuthorizeQuery {
    /// The request's validated PKCE challenge, `None` when it sent none.
    ///
    /// # Errors
    ///
    /// The 400s of [`pkce::parse_challenge`].
    fn pkce_challenge(&self) -> AppResult<Option<String>> {
        pkce::parse_challenge(
            self.code_challenge.as_deref(),
            self.code_challenge_method.as_deref(),
        )
    }

    /// Checks the PKCE parameters without keeping the challenge: the `GET`
    /// only validates them, the frontend sends them back with its verdict.
    ///
    /// # Errors
    ///
    /// The 400s of [`pkce::parse_challenge`].
    fn require_valid_pkce(&self) -> AppResult<()> {
        self.pkce_challenge().map(|_bound| ())
    }
}

/// What a finished authorization answers with.
#[derive(Debug)]
pub(crate) enum AuthorizeReply {
    /// 302 with this `Location`: the registered redirect URI carrying
    /// `code` (or `error=access_denied`), merged over its query.
    Found(HeaderValue),
    /// 200 `{"code": …}` JSON: the code of an out-of-band client, which
    /// cannot be redirected.
    Code(String),
}

impl IntoResponse for AuthorizeReply {
    fn into_response(self) -> Response {
        match self {
            Self::Found(location) => (StatusCode::FOUND, [(LOCATION, location)]).into_response(),
            Self::Code(code) => Json(json!({ "code": code })).into_response(),
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

/// The scopes a request asks for: parsed from `scope`,
/// [`Scopes::default`] (`read`) when the field is absent or blank — the
/// token endpoint's rule, applied to the authorize request.
pub(crate) fn requested_scopes(scope: Option<&str>) -> Scopes {
    scope
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map_or_else(Scopes::default, Scopes::parse)
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

/// Mints the authorization code for the approved scopes and answers with
/// it (Pleroma's `after_create_authorization`). The external-login
/// callback of a login without a second factor ends here; an MFA-enabled
/// account is refused before it gets this far. The delivery's PKCE
/// challenge, when it has one, binds the code to its verifier.
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
    let binding = CodeBinding {
        redirect_uri: delivery.redirect_uri,
        code_challenge: delivery.code_challenge,
    };
    let authorization = oauth_token::create_authorization_with_challenge(
        &state.db,
        user.id,
        app.id,
        &scopes.to_string(),
        binding,
    )
    .await?;
    deliver_code(&authorization, delivery)
}

/// The answer that hands a finished authorization's code to the client
/// (Pleroma's `after_create_authorization`): `{"code": …}` JSON for the
/// `urn:ietf:wg:oauth:2.0:oob` client, the 302 carrying `code` (and
/// `state`) for every other URI.
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
        return Ok(AuthorizeReply::Code(authorization.token.clone()));
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
/// the redirect holds the live request, and a code is single-use
/// material.
pub(crate) fn no_store(mut response: Response) -> Response {
    let stored: Option<HeaderValue> = response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
    drop(stored);
    response
}

#[cfg(test)]
mod tests;
