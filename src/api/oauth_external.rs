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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, warn};
use url::form_urlencoded;

use crate::{
    api::{
        accounts::{REGISTERED_AWAITING_APPROVAL, REGISTERED_AWAITING_CONFIRMATION},
        oauth::{
            authorize::{self, AuthorizeReply, Delivery},
            check_account_ready,
        },
    },
    config::{Config, OauthProvider},
    error::{AppError, AppResult},
    extract::{ApiBody, ApiParams},
    http_client::{HttpClient, HttpError, HttpResponse},
    models::{
        oauth_login_state::{self, OauthLoginState},
        user::{
            User,
            register::{FieldErrors, RegisterError, RegisterParams, create_local_user},
        },
        user_identity,
    },
    oauth::{registration_page, scopes::Scopes},
    state::AppState,
    util::token::random_token,
};

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

/// The `GET /oauth/prepare_request` parameters: the provider name beside
/// the `/oauth/authorize` fields whose validation it shares. Every field is
/// optional at the shape level — the endpoint answers the missing ones with
/// its own bodies, exactly as the sibling endpoint does.
#[derive(Debug, Deserialize)]
struct PrepareQuery {
    /// Which configured provider to prepare the login for.
    #[serde(default)]
    provider: Option<String>,
    /// The flow; only `code` is spoken here.
    #[serde(default)]
    response_type: Option<String>,
    /// The registered application's public id.
    #[serde(default)]
    client_id: Option<String>,
    /// Where the finished login is to be delivered; must be registered.
    #[serde(default)]
    redirect_uri: Option<String>,
    /// Requested Mastodon scopes, space- or `+`-separated.
    #[serde(default)]
    scope: Option<String>,
    /// Opaque client value, round-tripped to `redirect_uri`.
    #[serde(default)]
    state: Option<String>,
}

/// The `GET /oauth/{provider}` parameters.
#[derive(Debug, Deserialize)]
struct StateQuery {
    /// The state [`prepare_request`] issued.
    #[serde(default)]
    state: Option<String>,
}

/// The `GET /oauth/{provider}/callback` parameters. `code` and `state` are
/// what this server asks the provider to send; `error` is what a provider
/// that refused the login sends instead.
#[derive(Debug, Deserialize)]
struct CallbackQuery {
    /// The provider's one-time code for the token exchange.
    #[serde(default)]
    code: Option<String>,
    /// The state [`prepare_request`] issued.
    #[serde(default)]
    state: Option<String>,
    /// The provider's own refusal, whatever its text.
    #[serde(default)]
    error: Option<String>,
}

/// The 200 body of `GET /oauth/prepare_request`: the one URL that starts
/// the round trip.
#[derive(Debug, Serialize)]
struct PrepareReply {
    /// `{BASE_URL}/oauth/{provider}?state={state}`.
    url: String,
}

/// The `/oauth/authorize` parameters an `oauth_login_states` row carries
/// across the provider round trip: what to mint the code for and where to
/// deliver it. The `pending` key an unlinked callback stores beside these
/// is the registration flow's document ([`PendingRegistration`]), so it is
/// not a field here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct StoredParams {
    /// The registered application the finished login is for.
    #[serde(default)]
    client_id: String,
    /// Where the code is delivered; validated when it was stored.
    #[serde(default)]
    redirect_uri: String,
    /// The scopes validated at prepare time, granted verbatim.
    #[serde(default)]
    scope: String,
    /// The client's own `state`, echoed back with the code. Absent from the
    /// stored document when the client sent none, rather than a null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<String>,
}

impl StoredParams {
    /// The parameters of a validated prepare request: the application
    /// fields as checked, the scopes in their granted spelling.
    fn of_query(query: &PrepareQuery, scopes: &Scopes) -> Self {
        Self {
            client_id: query.client_id.clone().unwrap_or_default(),
            redirect_uri: query.redirect_uri.clone().unwrap_or_default(),
            scope: scopes.to_string(),
            state: query.state.clone(),
        }
    }

    /// The stored document of a live state, or the 400 `Invalid state` a
    /// row whose parameters cannot be read amounts to: nothing can be
    /// delivered on it.
    fn of_state(stored: &OauthLoginState) -> AppResult<Self> {
        serde_json::from_value(stored.params.clone())
            .map_err(|_unreadable| AppError::BadRequest(INVALID_STATE.to_owned()))
    }

    /// Where the finished (or refused) login goes: the stored redirect URI
    /// and the client's stored state, in the shape [`authorize::grant_code`]
    /// and [`authorize::denied`] answer from.
    fn delivery(&self) -> Delivery<'_> {
        Delivery {
            redirect_uri: self.redirect_uri.as_str(),
            state: self.state.as_deref(),
        }
    }

    /// The `grant_type=authorization_code` body for the provider's token
    /// endpoint: its credentials, the code it just sent, and the callback
    /// URL it saw at authorize time.
    fn token_form(provider: &OauthProvider, callback_uri: &str, code: &str) -> String {
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("grant_type", GRANT_TYPE_CODE),
                ("code", code),
                ("redirect_uri", callback_uri),
                ("client_id", provider.client_id.as_str()),
                ("client_secret", provider.client_secret.as_str()),
            ])
            .finish()
    }
}

/// The prepare half in order: provider, flow, application, redirect URI,
/// scopes — then the stored state and the URL that names it.
async fn prepared(state: &AppState, query: PrepareQuery) -> AppResult<PrepareReply> {
    let provider = configured_provider(state, query.provider.as_deref())?;
    authorize::require_code_response(query.response_type.as_deref())?;
    let app = authorize::load_app(&state.db, query.client_id.as_deref()).await?;
    let redirect_uri = query.redirect_uri.as_deref().unwrap_or_default();
    authorize::require_registered_redirect(&app, redirect_uri)?;
    let scopes = authorize::requested_scopes(query.scope.as_deref());
    authorize::check_scopes_subset(&scopes, &app)?;
    let params = StoredParams::of_query(&query, &scopes);
    let issued = oauth_login_state::create(&state.db, &provider.name, &json!(params)).await?;
    Ok(PrepareReply {
        url: authorize::with_query_params(
            &provider_entry(&state.config, &provider.name),
            &[("state", issued.state.as_str())],
        ),
    })
}

/// The redirect half: the state must be live and issued to this provider,
/// and then it is only a matter of spelling out the provider's own
/// authorization URL.
async fn redirected(state: &AppState, name: &str, query: StateQuery) -> AppResult<AuthorizeReply> {
    let provider = configured_provider(state, Some(name))?;
    let stored = live_state(state, name, query.state.as_deref()).await?;
    let callback_uri = provider_callback(&state.config, &provider.name);
    authorize::redirect_to(
        &provider.authorize_url,
        &provider_params(provider, &callback_uri, &stored.state),
    )
}

/// The callback half in order: state, stored parameters, the provider's
/// answer.
async fn completed(
    state: &AppState,
    name: &str,
    query: CallbackQuery,
) -> AppResult<AuthorizeReply> {
    let provider = configured_provider(state, Some(name))?;
    let stored = live_state(state, name, query.state.as_deref()).await?;
    let params = StoredParams::of_state(&stored)?;
    match identified(state, provider, query).await? {
        Landing::Denied => authorize::denied(&params.delivery()),
        Landing::Linked(user) => granted(state, &user, &params).await,
        Landing::Unlinked(account) => {
            let pending = pending_of(&provider.name, &account);
            if !oauth_login_state::set_pending(&state.db, &stored.state, &pending).await? {
                warn!(
                    provider = provider.name,
                    "oauth login state went away mid-callback"
                );
            }
            authorize::redirect_to(
                &format!("{}{REGISTRATION_DETAILS}", state.config.base_url),
                &[("state", stored.state.as_str())],
            )
        }
    }
}

/// The `GET /oauth/registration_details` half: the state's `pending`
/// account, prefilled into the form.
///
/// # Errors
///
/// 400 `Invalid state` from [`registrable_state`]; any database failure
/// arrives as [`AppError::Database`].
async fn details(state: &AppState, presented: Option<&str>) -> AppResult<AuthorizeReply> {
    let (stored, pending, _) = registrable_state(state, presented).await?;
    Ok(pending_form(&stored, &pending))
}

/// The registration half in order: the state that names the pending
/// account and the stored authorize parameters, the gates, the account,
/// the link, and the login the state was prepared for.
///
/// # Errors
///
/// 400 `Invalid state` (see [`registrable_state`]), or any database or key
/// generation failure of the registration itself as [`AppError`].
async fn registered(state: &AppState, form: RegisterForm) -> AppResult<AuthorizeReply> {
    let (stored, pending, params) = registrable_state(state, form.state.as_deref()).await?;
    if !state.config.registrations_open.get() {
        return Ok(refused_form(
            &form,
            &stored.state,
            REGISTRATIONS_CLOSED_FORM,
        ));
    }
    let account = Submitted::of_form(&form, &pending);
    let user = match create_local_user(state, account.into_register_params(), None).await {
        Ok(user) => user,
        Err(RegisterError::Invalid(errors)) => {
            return Ok(refused_form(
                &form,
                &stored.state,
                &validation_line(&errors),
            ));
        }
        Err(RegisterError::Failed(err)) => return Err(err),
    };
    let linked = user_identity::link(&state.db, user.id, &pending.provider, &pending.uid).await?;
    let _finished = oauth_login_state::delete(&state.db, &stored.state).await?;
    debug!(
        user = %user.id,
        identity = %linked.id,
        provider = %pending.provider,
        "external registration linked a new account and finished the login"
    );
    finish_registration(state, &user, &params).await
}

/// What a finished external registration is answered with: the message
/// page of an account that first waits for staff approval or for its
/// email's confirmation — the same two messages `POST /api/v1/accounts`
/// answers with, approval weighed first — or, for a ready account, the
/// very login the callback gives a linked user.
async fn finish_registration(
    state: &AppState,
    user: &User,
    params: &StoredParams,
) -> AppResult<AuthorizeReply> {
    if user.approval_pending.get() {
        return Ok(AuthorizeReply::Page(registration_page::render_message(
            REGISTERED_AWAITING_APPROVAL,
        )));
    }
    if !user.email_confirmed.get() {
        return Ok(AuthorizeReply::Page(registration_page::render_message(
            REGISTERED_AWAITING_CONFIRMATION,
        )));
    }
    granted(state, user, params).await
}

/// The state the registration page may run on: live under its own key,
/// carrying readable authorize parameters *and* the `pending` account an
/// unlinked callback left. The registration flow does not know (or care)
/// which provider issued it — the row says, and the `pending` document is
/// the write's guarantee that it was this server that stored it.
///
/// # Errors
///
/// 400 `Invalid state` for a missing, unknown or expired state, or one
/// that carries no usable `pending`; the same 400 for unreadable stored
/// parameters. Any database failure arrives as [`AppError::Database`].
async fn registrable_state(
    state: &AppState,
    presented: Option<&str>,
) -> AppResult<(OauthLoginState, PendingRegistration, StoredParams)> {
    let invalid = || AppError::BadRequest(INVALID_STATE.to_owned());
    let Some(presented) = presented.filter(|text| !text.is_empty()) else {
        return Err(invalid());
    };
    let stored = oauth_login_state::get_live(&state.db, presented)
        .await?
        .ok_or_else(invalid)?;
    let params = StoredParams::of_state(&stored)?;
    let pending = PendingRegistration::of_state(&stored).ok_or_else(invalid)?;
    Ok((stored, pending, params))
}

/// The registration form as the `GET` renders it: the provider's account
/// info prefilled — blank where the provider said nothing — and no error.
fn pending_form(stored: &OauthLoginState, pending: &PendingRegistration) -> AuthorizeReply {
    AuthorizeReply::Page(registration_page::render_form(
        &registration_page::RegistrationForm {
            nickname: pending.nickname.as_deref().unwrap_or_default(),
            email: pending.email.as_deref().unwrap_or_default(),
            name: pending.name.as_deref().unwrap_or_default(),
            bio: "",
            state: &stored.state,
            error: None,
        },
    ))
}

/// The registration form as a refused submission re-renders it: what the
/// user typed, kept, with `error` above the form.
fn refused_form(form: &RegisterForm, state: &str, error: &str) -> AuthorizeReply {
    AuthorizeReply::Page(registration_page::render_form(
        &registration_page::RegistrationForm {
            nickname: form.nickname.as_deref().unwrap_or_default(),
            email: form.email.as_deref().unwrap_or_default(),
            name: form.name.as_deref().unwrap_or_default(),
            bio: form.bio.as_deref().unwrap_or_default(),
            state,
            error: Some(error),
        },
    ))
}

/// Every field-validation failure of a refused registration in one line:
/// Pleroma's `register` action joins its changeset errors the same way,
/// `"field message; field message"`.
fn validation_line(errors: &FieldErrors) -> String {
    errors
        .iter()
        .map(|(field, messages)| format!("{field} {}", messages.join(", ")))
        .collect::<Vec<String>>()
        .join("; ")
}

/// The `params.pending` document of an unlinked callback, read back by the
/// registration flow: everything the form prefills beside the two facts
/// that make the identity link — the provider and its account id.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct PendingRegistration {
    /// Provider the round trip ran on; the `user_identities` key.
    provider: String,
    /// The provider account's stable id; the link's other key.
    uid: String,
    /// `nickname_field`, when the provider said it.
    #[serde(default)]
    nickname: Option<String>,
    /// `email_field`, when the provider said it.
    #[serde(default)]
    email: Option<String>,
    /// `name_field`, when the provider said it.
    #[serde(default)]
    name: Option<String>,
}

impl PendingRegistration {
    /// The `pending` document of a live state, or `None` when the state
    /// carries none (the callback never ran on it) or names no account to
    /// link: reading a provider account out of nothing is not this flow's
    /// business.
    fn of_state(stored: &OauthLoginState) -> Option<Self> {
        let raw = stored.params.get("pending")?;
        let pending: Self = serde_json::from_value(raw.clone()).ok()?;
        (!pending.provider.is_empty() && !pending.uid.is_empty()).then_some(pending)
    }
}

/// The `POST /oauth/register` form. The state rides hidden; every field
/// the flow keeps is optional at the shape level — blank submissions fall
/// back to the provider's info the way Pleroma's
/// `create_from_registration` reads the form above the stored
/// registration, and an absent nickname fails validation like a blank one.
#[derive(Debug, Deserialize)]
struct RegisterForm {
    /// The state [`registration_details`] carried through the form.
    #[serde(default)]
    state: Option<String>,
    /// Desired handle.
    #[serde(default)]
    nickname: Option<String>,
    /// Contact email.
    #[serde(default)]
    email: Option<String>,
    /// Display name.
    #[serde(default)]
    name: Option<String>,
    /// Profile note as source text.
    #[serde(default)]
    bio: Option<String>,
}

/// The submission's fields, resolved against the provider's `pending`
/// info: what [`RegisterForm`] says wins, the provider stands in where the
/// form left a field blank.
#[derive(Debug)]
struct Submitted {
    /// The handle to register: the form's, the provider's, or blank —
    /// which [`create_local_user`] refuses like any other blank.
    nickname: String,
    /// The email to register, or `None` when neither says one.
    email: Option<String>,
    /// The display name, or `None` when neither says one (then the
    /// nickname stands in, as registration always does).
    name: Option<String>,
    /// The profile note, verbatim from the form.
    bio: Option<String>,
    /// Whether the provider vouched for the registered address: its
    /// `pending` email equals the resolved one, case-insensitively. That
    /// is the ticket's proof — an `ACCOUNT_ACTIVATION_REQUIRED` instance
    /// registers such an account confirmed, and no letter goes out.
    provider_email_verified: bool,
}

impl Submitted {
    /// Resolve `form`'s fields against `pending`'s. The nickname falls back
    /// to the blank that validation refuses; the rest fall back to nothing.
    fn of_form(form: &RegisterForm, pending: &PendingRegistration) -> Self {
        let nickname =
            first_text(form.nickname.as_deref(), pending.nickname.as_deref()).unwrap_or_default();
        let email = first_text(form.email.as_deref(), pending.email.as_deref());
        let provider_email_verified = email.as_deref().is_some_and(|registered| {
            pending.email.as_deref().is_some_and(|said| {
                !said.trim().is_empty() && said.trim().eq_ignore_ascii_case(registered)
            })
        });
        Self {
            nickname,
            email,
            name: first_text(form.name.as_deref(), pending.name.as_deref()),
            bio: form.bio.clone(),
            provider_email_verified,
        }
    }

    /// The [`RegisterParams`] of a resolved submission: the password is a
    /// fresh random token nobody will ever type (see
    /// [`EXTERNAL_PASSWORD_BYTES`]), and no registration reason or invite
    /// belongs to an external signup.
    fn into_register_params(self) -> RegisterParams {
        RegisterParams {
            nickname: self.nickname,
            email: self.email,
            password: random_token(EXTERNAL_PASSWORD_BYTES),
            name: self.name,
            bio: self.bio,
            reason: None,
            provider_email_verified: self.provider_email_verified,
        }
    }
}

/// The first non-blank text of the two, trimmed: what the form said wins,
/// the provider's `pending` value stands in when the form left the field
/// blank — Pleroma's `create_from_registration` reads its submission above
/// the stored registration info the same way.
fn first_text(submitted: Option<&str>, stored: Option<&str>) -> Option<String> {
    submitted
        .into_iter()
        .chain(stored)
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Where one callback's identification landed.
#[derive(Debug)]
enum Landing {
    /// The provider refused, or its answer could not be used: send the app
    /// `error=access_denied`.
    Denied,
    /// The provider account is linked to this local user, who is now
    /// logged in. The row is boxed: it is the widest thing in the flow's
    /// answer, and `Denied` and `Unlinked` are the ones that pay for it.
    Linked(Box<User>),
    /// Nobody here knows the provider account; its document is the
    /// registration form's to work with.
    Unlinked(ProviderAccount),
}

/// The provider's answer, in order: a code (its own `error` parameter
/// counts as the absence of one), the token exchange, the userinfo read,
/// then the identity lookup.
async fn identified(
    state: &AppState,
    provider: &OauthProvider,
    query: CallbackQuery,
) -> AppResult<Landing> {
    let Some(code) = offered_code(query.error.as_deref(), query.code.as_deref()) else {
        return Ok(Landing::Denied);
    };
    let callback_uri = provider_callback(&state.config, &provider.name);
    let Some(access_token) = exchanged(state, provider, &callback_uri, code).await else {
        return Ok(Landing::Denied);
    };
    let Some(account) = read_account(&state.http_client, provider, &access_token).await else {
        return Ok(Landing::Denied);
    };
    let found = user_identity::find_user(&state.db, &provider.name, &account.uid).await?;
    Ok(found.map_or(Landing::Unlinked(account), |user| {
        Landing::Linked(Box::new(user))
    }))
}

/// The code the provider offered: `None`, with the reason logged, when it
/// sent an `error` parameter instead of a code, or an empty one.
fn offered_code<'a>(error: Option<&'a str>, code: Option<&'a str>) -> Option<&'a str> {
    if let Some(refusal) = error {
        warn!(provider_error = refusal, "external OAuth provider refused");
        return None;
    }
    code.filter(|text| !text.is_empty())
}

/// The login a finished flow earned, from the callback's linked user or a
/// completed registration: the stored application, redirect URI and
/// scopes re-checked the way the consent submission checks its round-tripped
/// form, then the readiness gate, then the very same code and delivery.
///
/// # Errors
///
/// 403 `account_is_disabled` from
/// [`crate::api::oauth`]'s account gate, a 400 refusal of the stored
/// application, redirect URI or scopes, or any database failure as
/// [`AppError::Database`].
async fn granted(
    state: &AppState,
    user: &User,
    params: &StoredParams,
) -> AppResult<AuthorizeReply> {
    let app = authorize::load_app(&state.db, Some(params.client_id.as_str())).await?;
    authorize::require_registered_redirect(&app, &params.redirect_uri)?;
    let scopes = Scopes::parse(&params.scope);
    authorize::check_scopes_subset(&scopes, &app)?;
    check_account_ready(user)?;
    authorize::grant_code(state, user, &app, &scopes, &params.delivery()).await
}

/// The state a redirect or a callback may run on: live, and issued to this
/// very provider.
///
/// # Errors
///
/// 400 `Invalid state` for a missing, unknown, expired or foreign state;
/// any database failure arrives as [`AppError::Database`].
async fn live_state(
    state: &AppState,
    name: &str,
    presented: Option<&str>,
) -> AppResult<OauthLoginState> {
    let invalid = || AppError::BadRequest(INVALID_STATE.to_owned());
    let Some(presented) = presented.filter(|text| !text.is_empty()) else {
        return Err(invalid());
    };
    oauth_login_state::get_valid(&state.db, presented, name)
        .await?
        .ok_or_else(invalid)
}

/// The configured provider a request names, or the 404 saying it does not
/// exist. The reference lives as long as the state's configuration.
fn configured_provider<'a>(
    state: &'a AppState,
    name: Option<&str>,
) -> AppResult<&'a OauthProvider> {
    state
        .config
        .oauth_provider(name.unwrap_or_default())
        .ok_or_else(|| AppError::NotFoundMessage(UNKNOWN_PROVIDER.to_owned()))
}

/// `{BASE_URL}/oauth/{provider}`: the entry point `prepare_request` answers.
fn provider_entry(config: &Config, name: &str) -> String {
    format!("{}{OAUTH_PATH}/{name}", config.base_url)
}

/// `{BASE_URL}/oauth/{provider}/callback`: the `redirect_uri` the provider
/// is told to answer to, and the URL its code is exchanged against.
fn provider_callback(config: &Config, name: &str) -> String {
    format!("{}/{}", provider_entry(config, name), CALLBACK_SEGMENT)
}

/// The parameters of the provider's authorization URL, in Pleroma's
/// `Ueberauth` spelling: the configured application, this instance's
/// callback, the code flow, the provider's own scopes and the state that
/// ties the answer back to the prepared request.
const fn provider_params<'a>(
    provider: &'a OauthProvider,
    callback_uri: &'a str,
    state: &'a str,
) -> [(&'a str, &'a str); 5] {
    [
        ("client_id", provider.client_id.as_str()),
        ("redirect_uri", callback_uri),
        ("response_type", authorize::RESPONSE_TYPE_CODE),
        ("scope", provider.scopes.as_str()),
        ("state", state),
    ]
}

/// `POST`s the code to the provider's token endpoint and reads its
/// `access_token`, `None` when the exchange failed in any way.
async fn exchanged(
    state: &AppState,
    provider: &OauthProvider,
    callback_uri: &str,
    code: &str,
) -> Option<String> {
    let body = StoredParams::token_form(provider, callback_uri, code);
    let outcome = state
        .http_client
        .post_bytes(
            &provider.token_url,
            &[("content-type", FORM_TYPE), ("accept", ACCEPT_JSON)],
            body.into_bytes(),
        )
        .await;
    let document = json_document("token", &provider.token_url, outcome)?;
    text_at(&document, "access_token").or_else(|| {
        warn!(
            provider = provider.name,
            "provider token answer carries no access_token"
        );
        None
    })
}

/// `GET`s the provider's userinfo document with the bearer token and reads
/// the account out of it, `None` when the read or the account failed.
async fn read_account(
    http: &HttpClient,
    provider: &OauthProvider,
    access_token: &str,
) -> Option<ProviderAccount> {
    let bearer = format!("Bearer {access_token}");
    let outcome = http
        .get_limited(
            &provider.userinfo_url,
            &[("authorization", bearer.as_str()), ("accept", ACCEPT_JSON)],
            USERINFO_MAX_BYTES,
        )
        .await;
    let document = json_document("userinfo", &provider.userinfo_url, outcome)?;
    ProviderAccount::of_document(provider, &document).or_else(|| {
        warn!(
            provider = provider.name,
            uid_field = provider.uid_field,
            "provider userinfo names no account id"
        );
        None
    })
}

/// Reads a provider answer as a JSON document, logging (never sending) the
/// reason it is not one.
fn json_document(role: &str, url: &str, outcome: Result<HttpResponse, HttpError>) -> Option<Value> {
    let response = match outcome.and_then(HttpResponse::error_for_status) {
        Err(err) => {
            warn!(call = role, %url, reason = %err, "external OAuth provider call failed");
            return None;
        }
        Ok(response) => response,
    };
    match response.json::<Value>() {
        Ok(document) => Some(document),
        Err(err) => {
            warn!(call = role, %url, reason = %err, "external OAuth provider answered no JSON");
            None
        }
    }
}

/// One provider account, as its userinfo document described it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderAccount {
    /// The stable provider id `uid_field` names; the `user_identities` key.
    uid: String,
    /// `nickname_field`, when the provider says it.
    nickname: Option<String>,
    /// `email_field`, when the provider says it.
    email: Option<String>,
    /// `name_field`, when the provider says it.
    name: Option<String>,
}

impl ProviderAccount {
    /// The account `document` describes under `provider`'s configured field
    /// names. Without a `uid_field` value there is no account to speak of;
    /// the three optional fields are simply absent.
    fn of_document(provider: &OauthProvider, document: &Value) -> Option<Self> {
        Some(Self {
            uid: text_at(document, &provider.uid_field)?,
            nickname: text_at(document, &provider.nickname_field),
            email: text_at(document, &provider.email_field),
            name: text_at(document, &provider.name_field),
        })
    }
}

/// The `params.pending` document of an unlinked callback: everything the
/// registration form needs to prefill, under the state that earned it.
fn pending_of(provider: &str, account: &ProviderAccount) -> Value {
    json!({
        "provider": provider,
        "uid": account.uid,
        "nickname": account.nickname,
        "email": account.email,
        "name": account.name,
    })
}

/// The text at the dotted path `path` of `document`: nested objects are dug
/// through key by key, a number reads as its own text (GitHub numbers its
/// account ids), and anything else — an absent key, an empty string, a list
/// or an object — is no value.
fn text_at(document: &Value, path: &str) -> Option<String> {
    let found = path
        .split('.')
        .filter(|key| !key.is_empty())
        .try_fold(document, |step, key| step.get(key))?;
    match found {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::String(_) | Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderName, HeaderValue, Request, StatusCode, header};
    use chrono::Utc;
    use std::sync::Arc;
    use tower::ServiceExt as _;

    use super::*;
    use crate::{config::BASE_URL_VAR, http_client::HttpClient, uploads::UploaderBackend};

    /// The provider the fixtures describe.
    fn provider(name: &str) -> OauthProvider {
        OauthProvider {
            name: name.to_owned(),
            client_id: format!("{name}-client-id"),
            client_secret: format!("{name}-client-secret"),
            authorize_url: format!("https://{name}.example/authorize"),
            token_url: format!("https://{name}.example/token"),
            userinfo_url: format!("https://{name}.example/userinfo"),
            scopes: "read:user user:email".to_owned(),
            uid_field: "id".to_owned(),
            nickname_field: "login".to_owned(),
            email_field: "email".to_owned(),
            name_field: "name".to_owned(),
        }
    }

    /// The state the route-shaped tests need: config with one provider per
    /// name, and a lazy pool those gates never consult.
    fn test_state(names: &[&str]) -> AppState {
        let mut config = Config::for_tests();
        config.oauth_providers = names.iter().map(|name| provider(name)).collect();
        let config = Arc::new(config);
        let db = sqlx::PgPool::connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no server");
        let hub = crate::streaming::hub::Hub::new(&db);
        AppState {
            db,
            hub,
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// The named header value of a response, as text.
    fn header_of(response: &Response, name: HeaderName) -> Option<String> {
        response
            .headers()
            .get(name)?
            .to_str()
            .ok()
            .map(str::to_owned)
    }

    /// A stored-parameters document with the fields `keep` overrides.
    fn stored_params() -> StoredParams {
        StoredParams {
            client_id: "client-id".to_owned(),
            redirect_uri: "https://client.example/callback".to_owned(),
            scope: "read".to_owned(),
            state: Some("client-state".to_owned()),
        }
    }

    /// The decoded query parameters of a URL, in order.
    fn query_pairs(url: &str) -> Vec<(String, String)> {
        let (_, query) = url.split_once('?').expect("a query string");
        form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect()
    }

    #[test]
    fn a_validated_query_stores_the_app_parameters_verbatim() {
        let query: PrepareQuery = crate::extract::into_params(json!({
            "provider": "github",
            "response_type": "code",
            "client_id": "client-id",
            "redirect_uri": "https://client.example/callback",
            "scope": "read write",
            "state": "client-state",
        }))
        .expect("every field is optional at the shape level");
        let params = StoredParams::of_query(&query, &Scopes::parse("read write"));
        assert_eq!(params.client_id, "client-id", "the application");
        assert_eq!(
            params.redirect_uri, "https://client.example/callback",
            "the delivery target"
        );
        assert_eq!(params.scope, "read write", "the granted scopes");
        assert_eq!(params.state.as_deref(), Some("client-state"), "the echo");

        let round_trip = serde_json::from_value::<StoredParams>(json!(params))
            .expect("the stored document reads back");
        assert_eq!(round_trip, params, "serialize and deserialize round-trip");

        let empty: PrepareQuery = crate::extract::into_params(json!({})).unwrap();
        let defaults = StoredParams::of_query(&empty, &Scopes::default());
        assert_eq!(defaults.client_id, "", "an absent app id stores empty");
        assert_eq!(defaults.scope, "read", "no scope asked is the default");
        assert_eq!(defaults.state, None, "and no client state echoes");
    }

    #[test]
    fn the_delivery_carries_the_stored_target_and_state() {
        let params = stored_params();
        let delivery = params.delivery();
        assert_eq!(
            delivery.redirect_uri, "https://client.example/callback",
            "the stored redirect URI"
        );
        assert_eq!(delivery.state, Some("client-state"), "the stored echo");
        let silent = StoredParams {
            state: None,
            ..params
        };
        assert_eq!(silent.delivery().state, None, "no echo when none was sent");
    }

    #[test]
    fn stored_parameters_read_back_out_of_a_state_row() {
        let row = OauthLoginState {
            state: "a-state".to_owned(),
            provider: "github".to_owned(),
            params: json!({"client_id": "c", "redirect_uri": "r", "scope": "read"}),
            valid_until: Utc::now(),
        };
        let params = StoredParams::of_state(&row).expect("the document is readable");
        assert_eq!(params.client_id, "c", "the stored application");
        assert_eq!(params.redirect_uri, "r", "the stored target");
        assert_eq!(params.scope, "read", "the stored scopes");
        assert_eq!(params.state, None, "an unstored echo is absent");

        // A `pending` key from an unlinked callback rides along unread: it
        // is [`PendingRegistration`]'s document, not this one.
        let pending = OauthLoginState {
            params: json!({"client_id": "c", "redirect_uri": "r", "pending": {"uid": "1"}}),
            ..row
        };
        assert_eq!(
            StoredParams::of_state(&pending)
                .expect("an extra key does not break the read")
                .redirect_uri,
            "r",
            "the authorize parameters still come through"
        );

        for broken in [
            json!(null),
            json!("nope"),
            json!([1, 2]),
            json!({"scope": 7}),
        ] {
            let bad = OauthLoginState {
                params: broken.clone(),
                ..pending.clone()
            };
            let err = StoredParams::of_state(&bad)
                .expect_err("a document of that shape is no parameters");
            assert_eq!(err.to_string(), INVALID_STATE, "refused as a bad state");
        }
    }

    #[test]
    fn the_token_form_carries_the_provider_credentials_and_the_code() {
        let form = StoredParams::token_form(
            &provider("github"),
            "https://test.example/oauth/github/callback",
            "the code",
        );
        assert_eq!(
            form,
            "grant_type=authorization_code&code=the+code&redirect_uri=https%3A%2F%2Ftest.example%2Foauth%2Fgithub%2Fcallback&client_id=github-client-id&client_secret=github-client-secret",
            "five urlencoded fields, in order"
        );
        assert_eq!(
            query_pairs(&format!("x?{form}")).len(),
            5,
            "and it parses back to exactly those five fields"
        );
    }

    #[test]
    fn the_provider_redirect_url_names_the_flow_and_the_callback() {
        let config = Config::for_tests();
        let github = provider("github");
        let callback_uri = provider_callback(&config, "github");
        let url = authorize::with_query_params(
            &github.authorize_url,
            &provider_params(&github, &callback_uri, "issued-state"),
        );
        let (base, _) = url.split_once('?').expect("a query string");
        assert_eq!(
            base, "https://github.example/authorize",
            "the configured endpoint"
        );
        assert_eq!(
            query_pairs(&url),
            vec![
                ("client_id".to_owned(), "github-client-id".to_owned()),
                ("redirect_uri".to_owned(), callback_uri),
                (
                    "response_type".to_owned(),
                    authorize::RESPONSE_TYPE_CODE.to_owned()
                ),
                ("scope".to_owned(), "read:user user:email".to_owned()),
                ("state".to_owned(), "issued-state".to_owned()),
            ],
            "the client id, our callback, the code flow, the provider's scopes, our state"
        );
    }

    #[test]
    fn a_provider_redirect_url_merges_into_an_existing_query() {
        let config = Config::for_tests();
        let mut github = provider("github");
        github.authorize_url = "https://github.example/authorize?foo=1".to_owned();
        let url = authorize::with_query_params(
            &github.authorize_url,
            &provider_params(&github, &provider_callback(&config, "github"), "s"),
        );
        assert_eq!(
            url,
            "https://github.example/authorize?foo=1&client_id=github-client-id&redirect_uri=https%3A%2F%2Ftest.example%2Foauth%2Fgithub%2Fcallback&response_type=code&scope=read%3Auser+user%3Aemail&state=s",
            "the operator's own parameters are kept and the values urlencoded"
        );
    }

    #[test]
    fn entry_and_callback_urls_hang_off_the_base_url() {
        let config = Config::from_lookup(|key| match key {
            crate::config::DATABASE_URL_VAR => Some("postgres://localhost/wired".to_owned()),
            BASE_URL_VAR => Some("https://social.example:4000/".to_owned()),
            _ => None,
        })
        .expect("a valid config");
        assert_eq!(
            provider_entry(&config, "github"),
            "https://social.example:4000/oauth/github",
            "the entry point follows BASE_URL, port included"
        );
        assert_eq!(
            provider_callback(&config, "github"),
            "https://social.example:4000/oauth/github/callback",
            "and so does the provider's answer target"
        );
    }

    #[test]
    fn userinfo_fields_are_dug_out_by_dotted_path() {
        let document = json!({
            "id": 4242,
            "login": "octocat",
            "name": "",
            "email": null,
            "profile": {"bio": "  hi  ", "tags": ["a"]},
            "flags": true,
        });
        assert_eq!(
            text_at(&document, "id").as_deref(),
            Some("4242"),
            "a number reads as its own text"
        );
        assert_eq!(
            text_at(&document, "login").as_deref(),
            Some("octocat"),
            "a string reads as itself"
        );
        assert_eq!(
            text_at(&document, "profile.bio").as_deref(),
            Some("  hi  "),
            "a nested string is dug out by its dotted path, verbatim"
        );
        for quiet in ["name", "email", "missing", "profile.tags", "flags", "", "."] {
            assert_eq!(
                text_at(&document, quiet),
                None,
                "`{quiet}` names no usable text"
            );
        }
    }

    #[test]
    fn the_account_is_the_configured_fields_of_the_document() {
        let mut keyed = provider("github");
        keyed.uid_field = "data.user.id".to_owned();
        keyed.nickname_field = "data.user.login".to_owned();
        keyed.email_field = String::new();
        keyed.name_field = "absent".to_owned();
        let document = json!({"data": {"user": {"id": 7, "login": "octo"}}});
        let account = ProviderAccount::of_document(&keyed, &document).expect("the nested id");
        assert_eq!(account.uid, "7", "the dotted uid_field");
        assert_eq!(account.nickname.as_deref(), Some("octo"), "the dotted name");
        assert_eq!(account.email, None, "an empty field name says nothing");
        assert_eq!(account.name, None, "an absent field says nothing");

        assert!(
            ProviderAccount::of_document(&keyed, &json!({"login": "octo"})).is_none(),
            "without a uid there is no account"
        );
    }

    #[test]
    fn the_pending_document_carries_every_field() {
        let account = ProviderAccount {
            uid: "4242".to_owned(),
            nickname: Some("octocat".to_owned()),
            email: None,
            name: Some("The Octocat".to_owned()),
        };
        assert_eq!(
            pending_of("github", &account),
            json!({
                "provider": "github",
                "uid": "4242",
                "nickname": "octocat",
                "email": null,
                "name": "The Octocat",
            }),
            "the registration form reads a null as 'the provider did not say'"
        );
    }

    #[test]
    fn an_error_parameter_is_a_code_the_provider_never_sent() {
        assert_eq!(
            offered_code(None, Some("the-code")),
            Some("the-code"),
            "a plain code is offered"
        );
        assert_eq!(offered_code(None, None), None, "and nothing otherwise");
        assert_eq!(
            offered_code(None, Some("")),
            None,
            "an empty code is nothing"
        );
        assert_eq!(
            offered_code(Some("access_denied"), Some("the-code")),
            None,
            "a refusal wins over a code sent beside it"
        );
    }

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
    async fn the_registration_pages_refuse_a_stateless_request_as_400() {
        let get = router()
            .with_state(test_state(&["github"]))
            .oneshot(
                Request::get("/oauth/registration_details")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("the router answers in process");
        assert_eq!(get.status(), StatusCode::BAD_REQUEST, "no state, no form");
        assert_eq!(
            header_of(&get, header::CONTENT_TYPE).as_deref(),
            Some("application/json"),
            "the refusal is JSON"
        );
        let post = router()
            .with_state(test_state(&["github"]))
            .oneshot(
                Request::post("/oauth/register")
                    .header(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/x-www-form-urlencoded"),
                    )
                    .body(axum::body::Body::from("state=&nickname=octo"))
                    .unwrap(),
            )
            .await
            .expect("the router answers in process");
        assert_eq!(
            post.status(),
            StatusCode::BAD_REQUEST,
            "an empty state submits to nothing"
        );
    }

    #[test]
    fn the_pending_document_reads_back_out_of_a_state_row() {
        let row = |params: Value| OauthLoginState {
            state: "a-state".to_owned(),
            provider: "github".to_owned(),
            params,
            valid_until: Utc::now(),
        };
        let pending = PendingRegistration::of_state(&row(json!({
            "client_id": "c",
            "pending": {
                "provider": "github",
                "uid": "4242",
                "nickname": "octocat",
                "email": null,
                "name": "The Octocat",
            },
        })))
        .expect("the callback's document reads");
        assert_eq!(pending.provider, "github", "the link's provider");
        assert_eq!(pending.uid, "4242", "the link's account id");
        assert_eq!(pending.nickname.as_deref(), Some("octocat"), "the prefill");
        assert_eq!(pending.email, None, "a null is the provider saying nothing");
        assert_eq!(
            pending.name.as_deref(),
            Some("The Octocat"),
            "and the rest comes through"
        );

        for unusable in [
            json!({"client_id": "c"}),
            json!({"pending": null}),
            json!({"pending": "nope"}),
            json!({"pending": {"provider": "github", "uid": ""}}),
            json!({"pending": {"provider": "", "uid": "4242"}}),
            json!({"pending": {"uid": "4242"}}),
            json!({"pending": {"provider": 7, "uid": "4242"}}),
        ] {
            assert!(
                PendingRegistration::of_state(&row(unusable.clone())).is_none(),
                "`{unusable}` names no account to register: it is no state",
            );
        }
    }

    #[test]
    fn submitted_fields_prefer_the_form_and_fall_back_to_the_provider() {
        let pending = PendingRegistration {
            provider: "github".to_owned(),
            uid: "4242".to_owned(),
            nickname: Some("octocat".to_owned()),
            email: Some("OctoCat@GitHub.Example".to_owned()),
            name: None,
        };
        let form: RegisterForm = crate::extract::into_params(json!({
            "state": "s",
            "nickname": "  chosen  ",
            "email": "",
            "bio": " hi ",
        }))
        .expect("every field is optional");
        let submitted = Submitted::of_form(&form, &pending);
        assert_eq!(submitted.nickname, "chosen", "the form's, trimmed");
        assert_eq!(
            submitted.email.as_deref(),
            Some("OctoCat@GitHub.Example"),
            "blank falls back to the provider's"
        );
        assert_eq!(submitted.name, None, "nothing says a name");
        assert_eq!(
            submitted.bio.as_deref(),
            Some(" hi "),
            "the bio is the form's"
        );
        assert!(
            submitted.provider_email_verified,
            "the fallback email IS the provider's: the address is vouched for"
        );

        let mismatch: RegisterForm = crate::extract::into_params(json!({
            "state": "s",
            "email": "someone-else@example.org",
        }))
        .expect("the optional fields may all be absent");
        let changed = Submitted::of_form(&mismatch, &pending);
        assert!(
            !changed.provider_email_verified,
            "a different email needs the confirmation click after all"
        );

        let nothing = PendingRegistration {
            email: None,
            ..pending
        };
        assert!(
            !Submitted::of_form(&form, &nothing).provider_email_verified,
            "a provider that said no email vouched for nothing"
        );
        let both_blank = Submitted::of_form(
            &crate::extract::into_params(json!({"state": "s", "nickname": "  "}))
                .expect("a bare submission"),
            &PendingRegistration {
                provider: "github".to_owned(),
                uid: "4242".to_owned(),
                nickname: None,
                email: Some("   ".to_owned()),
                name: None,
            },
        );
        assert_eq!(both_blank.nickname, "", "no nickname anywhere is the blank");
        assert_eq!(both_blank.email, None, "and a blank provider email is none");
    }

    #[test]
    fn the_external_password_is_a_fresh_random_secret() {
        let pending = PendingRegistration {
            provider: "github".to_owned(),
            uid: "4242".to_owned(),
            nickname: Some("octocat".to_owned()),
            email: None,
            name: None,
        };
        let form: RegisterForm =
            crate::extract::into_params(json!({"state": "s"})).expect("all optional");
        let first = Submitted::of_form(&form, &pending).into_register_params();
        let second = Submitted::of_form(&form, &pending).into_register_params();
        assert_eq!(first.password.len(), 86, "64 random bytes, base64url");
        assert_ne!(
            first.password, second.password,
            "and never the same secret twice"
        );
        assert!(
            first
                .password
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "URL-safe by construction: {}",
            first.password
        );
        assert_eq!(
            first.nickname, "octocat",
            "the provider's nickname stands in"
        );
        assert_eq!(first.email, None, "no email is no email");
        assert!(first.reason.is_none(), "an external signup gives no reason");
        assert!(
            !first.provider_email_verified,
            "nothing was vouched for when the provider said no email"
        );
    }

    #[test]
    fn the_field_errors_render_as_one_line_above_the_form() {
        let mut errors = FieldErrors::new();
        let _new = errors.insert("nickname", vec!["has already been taken".to_owned()]);
        assert_eq!(
            validation_line(&errors),
            "nickname has already been taken",
            "Pleroma's `field message` spelling"
        );
        let _prior = errors.insert(
            "email",
            vec!["can't be blank".to_owned(), "has invalid format".to_owned()],
        );
        assert_eq!(
            validation_line(&errors),
            "email can't be blank, has invalid format; nickname has already been taken",
            "every field, messages joined, fields ordered and separated"
        );
    }

    #[test]
    fn the_refused_form_keeps_the_submission_and_names_the_error() {
        let form: RegisterForm = crate::extract::into_params(json!({
            "state": "s",
            "nickname": "kept",
            "email": "a&b@example.org",
            "bio": "</textarea><p>x</p>",
        }))
        .expect("a submission");
        let AuthorizeReply::Page(html) =
            refused_form(&form, "s", "nickname has already been taken")
        else {
            panic!("a refused registration re-renders a page, never a redirect")
        };
        assert!(html.contains("value=\"kept\""), "what the user typed stays");
        assert!(
            html.contains("value=\"a&amp;b@example.org\""),
            "escaped, as ever"
        );
        assert!(
            !html.contains("</textarea><p>x</p>"),
            "a bio cannot close its textarea: {html}"
        );
        assert!(
            html.contains("role=\"alert\">nickname has already been taken</p>"),
            "the error line stands above the form"
        );
        assert!(
            html.contains("<input type=\"hidden\" name=\"state\" value=\"s\">"),
            "and the state rides along for the retry"
        );
    }

    #[test]
    fn the_pending_form_prefills_the_provider_and_blanks_the_rest() {
        let row = OauthLoginState {
            state: "issued".to_owned(),
            provider: "github".to_owned(),
            params: json!({"client_id": "c"}),
            valid_until: Utc::now(),
        };
        let pending = PendingRegistration {
            provider: "github".to_owned(),
            uid: "4242".to_owned(),
            nickname: Some("octocat".to_owned()),
            email: None,
            name: Some("a\"b".to_owned()),
        };
        let AuthorizeReply::Page(html) = pending_form(&row, &pending) else {
            panic!("a form is a page")
        };
        assert!(
            html.contains("name=\"nickname\" value=\"octocat\""),
            "the provider's nickname prefills"
        );
        assert!(
            html.contains("name=\"email\" value=\"\""),
            "a provider that said no email renders a blank field"
        );
        assert!(
            html.contains("name=\"name\" value=\"a&quot;b\""),
            "and the name is escaped in its attribute"
        );
        assert!(
            html.contains("<input type=\"hidden\" name=\"state\" value=\"issued\">"),
            "the state goes out hidden"
        );
        assert!(
            !html.contains("role=\"alert\""),
            "a fresh form has no error"
        );
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
    fn the_prepare_reply_is_the_one_url_key() {
        let json = serde_json::to_value(PrepareReply {
            url: "https://test.example/oauth/github?state=s".to_owned(),
        })
        .expect("a one-field struct serializes");
        assert_eq!(
            json,
            json!({"url": "https://test.example/oauth/github?state=s"}),
            "exactly the `url` key the ticket pins"
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
