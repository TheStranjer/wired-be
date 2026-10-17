//! `POST /oauth/token`: the OAuth 2.0 `password`, `client_credentials` and
//! `refresh_token` grants (Pleroma's `OAuthController.token_exchange`,
//! `lib/pleroma/web/o_auth/o_auth_controller.ex`).
//!
//! The `refresh_token` grant rotates a user-bound pair: it authenticates
//! the application like `/oauth/revoke` does, looks the presented refresh
//! token up (it must belong to that same application, and its owner to an
//! active account, else the shared 400 "Invalid credentials"), mints a
//! fresh pair with the same user and scopes, deletes the old row — so the
//! old access and refresh tokens stop working — and answers the standard
//! password-grant JSON. A refresh token does not expire on its own: an
//! expired access token is still refreshable.
//!
//! The `client_credentials` grant authenticates the application the same
//! way (body fields or HTTP Basic), defaults its `scope` to `read`,
//! bounds it by the app's registration, and mints an app-only token
//! (`user_id = NULL`) that acts as the application itself; its answer
//! carries no `refresh_token` and no `me`, since there is no user behind
//! it.
//!
//! Rendered the way Pleroma's "token.json" view
//! (`lib/pleroma/web/o_auth/token_view.ex`) renders it. A client that
//! already holds an account's credentials (and its own registered
//! `client_id`/`client_secret`) exchanges them for a bearer token pair,
//! with no browser redirect. The application is validated from the client
//! credentials — the body fields, or, preferred per RFC 6749 and Pleroma's
//! `Token.Utils.fetch_client_credentials`, an
//! `Authorization: Basic base64(client_id:client_secret)` header — the
//! requested scopes (default `read`) must fit inside the application's
//! registration, and the user logs in with nickname or email and a
//! password. Unknown apps, unknown users and wrong secrets or passwords
//! all answer the same 400 "Invalid credentials" (Pleroma's way); a
//! deactivated account instead gets its distinct 403
//! "`account_is_disabled`", checked by `check_account_ready`, where the
//! later approval/confirmation/MFA tickets join.
//!
//! The endpoint serves `/oauth/token`, outside `/api` (decision 1), and
//! [`crate::app`] merges this router at the top level. Per RFC 6749 §5.1
//! every answer — success or error — carries `Cache-Control: no-store`
//! and `Pragma: no-cache`.
//!
//! `POST /oauth/revoke` (Pleroma's `token_revoke`, RFC 7009) retires an
//! access token: it authenticates the application exactly like
//! `/oauth/token` does (body fields or HTTP Basic), deletes the token
//! when it belongs to that app and answers `{}`. An unknown token
//! revokes to nothing without an error (RFC 7009 §2.2), a token issued
//! to another application is refused the 400 "Invalid credentials", and
//! a request without the `token` parameter names it. Every answer
//! carries the same no-store headers.
//!
//! The authorization-code half of `/oauth` — `GET`/`POST
//! /oauth/authorize` — lives in [`authorize`].

use std::{fmt, time::Duration};

use axum::{
    Json, Router,
    extract::State,
    http::{
        HeaderMap, HeaderValue,
        header::{AUTHORIZATION, CACHE_CONTROL, PRAGMA},
    },
    response::{IntoResponse as _, Response},
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use subtle::ConstantTimeEq as _;

use crate::{
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        oauth_app::{self, OauthApp},
        oauth_token::{self, NewToken, OauthToken},
        user::{self, User, register},
    },
    oauth::scopes::Scopes,
    state::AppState,
};

pub mod authorize;

/// The password grant type: account credentials in exchange for a
/// user-bound token pair.
const GRANT_TYPE_PASSWORD: &str = "password";
/// The client-credentials grant type: the application authenticates
/// itself and receives an app-only token.
const GRANT_TYPE_CLIENT_CREDENTIALS: &str = "client_credentials";
/// The refresh-token grant type: a live refresh token is exchanged for a
/// fresh pair with the same user and scopes, the old row deleted.
const GRANT_TYPE_REFRESH_TOKEN: &str = "refresh_token";
/// 400 body when the request omits `grant_type` entirely.
const MISSING_GRANT_TYPE: &str = "Missing parameter: grant_type";
/// 400 body when `grant_type` names a grant this endpoint does not speak.
const UNSUPPORTED_GRANT_TYPE: &str = "unsupported_grant_type";
/// 400 body when the revocation request omits the `token` parameter
/// (Pleroma's `Missing parameter: token`).
const MISSING_TOKEN: &str = "Missing parameter: token";
/// 400 body when the refresh-token grant omits the `refresh_token`
/// parameter.
const MISSING_REFRESH_TOKEN: &str = "Missing parameter: refresh_token";
/// 400 body for every credential failure — unknown app, wrong client
/// secret, unknown user, wrong password — which Pleroma deliberately does
/// not tell apart (`render_invalid_credentials_error`).
const INVALID_CREDENTIALS: &str = "Invalid credentials";
/// 400 body when the requested scopes reach outside the app's registration.
const INVALID_SCOPE: &str = "Invalid scope";
/// 403 body message for a deactivated account (Pleroma's
/// `handle_token_exchange_error({:account_status, :deactivated})`).
const ACCOUNT_DISABLED: &str = "Your account is currently disabled";
/// The machine-readable identifier the 403 above carries beside the
/// message.
const ACCOUNT_IS_DISABLED: &str = "account_is_disabled";
/// `token_type` of a successful answer: issued tokens are bearer
/// credentials.
const TOKEN_TYPE_BEARER: &str = "Bearer";
/// `Authorization` scheme carrying client credentials, matched
/// case-insensitively like the bearer scheme in [`crate::auth`].
const BASIC_SCHEME: &str = "Basic";
/// `Cache-Control` value of every answer (RFC 6749 §5.1).
const NO_STORE: &str = "no-store";
/// `Pragma` value of every answer (RFC 6749 §5.1).
const NO_CACHE: &str = "no-cache";

/// The `/oauth` router, merged at the top level of [`crate::app`]: the
/// paths are fixed by OAuth (`/oauth/token`, `/oauth/revoke`,
/// `/oauth/authorize` and the rest in later tickets), not nested under
/// `/api`.
///
/// `POST /oauth/token` and the `/oauth/authorize` half it merges run in
/// the limiter's `authentication` bucket (Pleroma throttles both paths in
/// `router.ex`), so `state` is handed to them for the middleware layer.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/oauth/token",
            post(token).layer(crate::rate_limit::rate_limit(state, "authentication")),
        )
        .route("/oauth/revoke", post(revoke))
        .merge(authorize::router(state))
}

/// `POST /oauth/token` — exchange credentials for a bearer token pair.
/// No authentication: this endpoint is where authentication comes from.
/// Every answer, success or error, gets the no-store headers.
async fn token(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiBody(params): ApiBody<TokenParams>,
) -> Response {
    let response = match exchange(&state, &headers, params).await {
        Ok(body) => body.into_response(),
        Err(err) => err.into_response(),
    };
    no_cache_headers(response)
}

/// The grant [`require_grant`] accepts, and which half of the endpoint
/// then runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grant {
    /// `grant_type=password`: client credentials plus a user's login,
    /// answered with a user-bound token pair.
    Password,
    /// `grant_type=client_credentials`: client credentials alone,
    /// answered with an app-only token.
    ClientCredentials,
    /// `grant_type=refresh_token`: a live refresh token plus client
    /// credentials, answered with a rotated user-bound token pair.
    RefreshToken,
}

/// The token exchange, in order: the grant type, then per grant — the
/// client application, the requested scopes, (for the password grant)
/// the user's credentials and the account's readiness — and the minted
/// token rendered.
async fn exchange(
    state: &AppState,
    headers: &HeaderMap,
    params: TokenParams,
) -> AppResult<Json<TokenResponse>> {
    match require_grant(&params)? {
        Grant::Password => password_grant(state, headers, &params).await,
        Grant::ClientCredentials => client_credentials_grant(state, headers, &params).await,
        Grant::RefreshToken => refresh_token_grant(state, headers, &params).await,
    }
}

/// The password grant: app, scopes, user login, account readiness, then
/// the user-bound token pair.
async fn password_grant(
    state: &AppState,
    headers: &HeaderMap,
    params: &TokenParams,
) -> AppResult<Json<TokenResponse>> {
    let app = validate_app(state, headers, params.body_credentials()).await?;
    let scopes = requested_scopes(params, &app)?;
    let user = authenticate_user(&state.db, params).await?;
    check_account_ready(&user)?;
    let token = issue_token(state, Some(&user), &app, scopes).await?;
    Ok(Json(TokenResponse::new(token, Some(&user))))
}

/// The `client_credentials` grant: the application authenticates itself
/// (no user login) and receives an app-only token, `user_id = NULL`,
/// acting as the application.
async fn client_credentials_grant(
    state: &AppState,
    headers: &HeaderMap,
    params: &TokenParams,
) -> AppResult<Json<TokenResponse>> {
    let app = validate_app(state, headers, params.body_credentials()).await?;
    let scopes = requested_scopes(params, &app)?;
    let token = issue_token(state, None, &app, scopes).await?;
    Ok(Json(TokenResponse::new(token, None)))
}

/// The `refresh_token` grant: rotate a user-bound pair. The application
/// authenticates exactly like on `/oauth/revoke` (body fields or HTTP
/// Basic); the presented refresh token must name a row issued to that
/// same app, owned by an active account — every failure answers the
/// shared 400 [`INVALID_CREDENTIALS`]. The replacement carries the same
/// user and scopes and is minted before the old row goes, so the old
/// access and refresh tokens stop working at the answer. No expiry is
/// applied to the old row: refreshing is precisely what a client does
/// after its access token has lapsed.
async fn refresh_token_grant(
    state: &AppState,
    headers: &HeaderMap,
    params: &TokenParams,
) -> AppResult<Json<TokenResponse>> {
    let Some(refresh_token) = params.refresh_token.as_deref() else {
        return Err(AppError::BadRequest(MISSING_REFRESH_TOKEN.to_owned()));
    };
    let app = validate_app(state, headers, params.body_credentials()).await?;
    let old = oauth_token::get_by_refresh_token(&state.db, refresh_token)
        .await?
        .filter(|found| found.app_id == app.id)
        .ok_or_else(invalid_credentials)?;
    let owner = active_owner(&state.db, &old)
        .await?
        .ok_or_else(invalid_credentials)?;
    let fresh = issue_token(state, Some(&owner), &app, Scopes::parse(&old.scopes)).await?;
    oauth_token::delete_token(&state.db, old.id).await?;
    Ok(Json(TokenResponse::new(fresh, Some(&owner))))
}

/// The active user behind `token`: `None` when the row is app-only
/// (`user_id = NULL`, so it never had a refreshable pair), its user was
/// deleted out from under it, or the account is deactivated — a
/// suspended account cannot rotate its tokens.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn active_owner(pool: &PgPool, token: &OauthToken) -> AppResult<Option<User>> {
    let owner = match token.user_id {
        Some(user_id) => user::get_by_id(pool, user_id).await?,
        None => None,
    };
    Ok(owner.filter(|user| user.is_active.get()))
}

/// The request parameters of `POST /oauth/token`.
///
/// Every field is optional at the shape level: Pleroma answers the
/// missing and the unknown with its own messages (`"Missing parameter:
/// grant_type"`, `"Invalid credentials"`), so the handler checks the
/// fields instead of serde rejecting the body. The `Debug` impl is manual
/// (see below) because two fields are secret material.
#[derive(Deserialize)]
struct TokenParams {
    /// Which grant to run; [`GRANT_TYPE_PASSWORD`],
    /// [`GRANT_TYPE_CLIENT_CREDENTIALS`] and [`GRANT_TYPE_REFRESH_TOKEN`]
    /// are implemented.
    #[serde(default)]
    grant_type: Option<String>,
    /// The account's nickname or email. Required by the password grant.
    #[serde(default)]
    username: Option<String>,
    /// The account's password. Required by the password grant. Renders as
    /// `<redacted>` in `Debug` output.
    #[serde(default)]
    password: Option<String>,
    /// Requested scopes, space- or `+`-separated; absent or blank means
    /// [`Scopes::default`] (`read`).
    #[serde(default)]
    scope: Option<String>,
    /// The public OAuth client id, when not presented through HTTP Basic.
    #[serde(default)]
    client_id: Option<String>,
    /// The OAuth client secret, when not presented through HTTP Basic.
    /// Never in `Debug` output.
    #[serde(default)]
    client_secret: Option<String>,
    /// The refresh token to rotate. Required by the `refresh_token`
    /// grant. Renders as `<redacted>` in `Debug` output.
    #[serde(default)]
    refresh_token: Option<String>,
}

/// [`password`](Self::password), [`client_secret`](Self::client_secret)
/// and [`refresh_token`](Self::refresh_token) are secret material and
/// render as `<redacted>`; everything else shows.
impl fmt::Debug for TokenParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenParams")
            .field("grant_type", &self.grant_type)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("scope", &self.scope)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

impl TokenParams {
    /// The `client_id`/`client_secret` body fields as a pair, `None`
    /// unless both are present.
    fn body_credentials(&self) -> Option<(&str, &str)> {
        Some((self.client_id.as_deref()?, self.client_secret.as_deref()?))
    }
}

/// Checks the grant type and names the grant to run. The password,
/// `client_credentials` and `refresh_token` grants are the ones
/// implemented here; the authorization-code exchange joins in a later
/// ticket.
///
/// # Errors
///
/// 400 [`MISSING_GRANT_TYPE`] when the field is absent, 400
/// [`UNSUPPORTED_GRANT_TYPE`] for any value other than
/// [`GRANT_TYPE_PASSWORD`], [`GRANT_TYPE_CLIENT_CREDENTIALS`] or
/// [`GRANT_TYPE_REFRESH_TOKEN`].
fn require_grant(params: &TokenParams) -> AppResult<Grant> {
    let Some(grant_type) = params.grant_type.as_deref() else {
        return Err(AppError::BadRequest(MISSING_GRANT_TYPE.to_owned()));
    };
    match grant_type {
        GRANT_TYPE_PASSWORD => Ok(Grant::Password),
        GRANT_TYPE_CLIENT_CREDENTIALS => Ok(Grant::ClientCredentials),
        GRANT_TYPE_REFRESH_TOKEN => Ok(Grant::RefreshToken),
        _ => Err(AppError::BadRequest(UNSUPPORTED_GRANT_TYPE.to_owned())),
    }
}

/// The registered application behind the presented client credentials,
/// the `Authorization: Basic` header winning over the `body_credentials`
/// pair (Pleroma's `Token.Utils.fetch_client_credentials`: "Per RFC 6749,
/// HTTP Basic is preferred to body params"). Both `/oauth/token` and
/// `/oauth/revoke` authenticate their application through here.
///
/// # Errors
///
/// 400 [`INVALID_CREDENTIALS`] when no pair is presented, the `client_id`
/// finds no application, or the `client_secret` does not match its row.
async fn validate_app(
    state: &AppState,
    headers: &HeaderMap,
    body_credentials: Option<(&str, &str)>,
) -> AppResult<OauthApp> {
    let basic = basic_credentials(headers);
    let presented = basic
        .as_ref()
        .map(|(id, secret)| (id.as_str(), secret.as_str()))
        .or(body_credentials);
    let Some((client_id, client_secret)) = presented else {
        return Err(invalid_credentials());
    };
    let app = oauth_app::get_by_client_id(&state.db, client_id).await?;
    app.filter(|found| secret_matches(&found.client_secret, client_secret))
        .ok_or_else(invalid_credentials)
}

/// The client credentials pair of an `Authorization: Basic
/// base64(client_id:client_secret)` header: `None` when the header is
/// absent, uses another scheme, is not valid base64 or UTF-8, or holds no
/// `:` separator (Pleroma's `fetch_client_credentials` falls back to the
/// body fields in every one of those cases).
fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case(BASIC_SCHEME) {
        return None;
    }
    let decoded = STANDARD.decode(encoded.trim()).ok()?;
    let text = std::str::from_utf8(&decoded).ok()?;
    let (client_id, client_secret) = text.split_once(':')?;
    Some((client_id.to_owned(), client_secret.to_owned()))
}

/// Whether `presented` is the stored client secret, compared in constant
/// time so a rejection cannot leak how much of it matched.
fn secret_matches(stored: &str, presented: &str) -> bool {
    stored.as_bytes().ct_eq(presented.as_bytes()).into()
}

/// The one 400 every credential failure shares: an unknown application,
/// a wrong secret, an unknown user and a wrong password are not told
/// apart (Pleroma's `render_invalid_credentials_error`).
fn invalid_credentials() -> AppError {
    AppError::BadRequest(INVALID_CREDENTIALS.to_owned())
}

/// The scopes to grant: the requested ones — [`Scopes::default`] (`read`)
/// when the field is absent or blank — bounded by the application's
/// registration.
///
/// # Errors
///
/// 400 [`INVALID_SCOPE`] when a requested scope is not covered by the
/// app's registered scopes ([`Scopes::is_subset_of`]).
fn requested_scopes(params: &TokenParams, app: &OauthApp) -> AppResult<Scopes> {
    let requested = params
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map_or_else(Scopes::default, Scopes::parse);
    if requested.is_subset_of(&Scopes::parse(&app.scopes)) {
        Ok(requested)
    } else {
        Err(AppError::BadRequest(INVALID_SCOPE.to_owned()))
    }
}

/// Verifies the grant's `username` (nickname or email) and `password`
/// against the users table.
///
/// # Errors
///
/// 400 [`INVALID_CREDENTIALS`] when either field is absent, or they do
/// not log in — one answer for every case.
async fn authenticate_user(pool: &PgPool, params: &TokenParams) -> AppResult<User> {
    let username = params.username.as_deref().unwrap_or_default();
    let password = params.password.as_deref().unwrap_or_default();
    register::authenticate_any_status(pool, username, password)
        .await
        .ok_or_else(invalid_credentials)
}

/// Whether an authenticated account may receive a token right now. Today
/// that is the deactivated check alone (Pleroma's `User.account_status`,
/// applied by `do_create_authorization`); the approval, confirmation and
/// MFA checks join here in later tickets.
///
/// # Errors
///
/// 403 [`ACCOUNT_DISABLED`] with identifier [`ACCOUNT_IS_DISABLED`] for a
/// deactivated account.
pub(crate) fn check_account_ready(user: &User) -> Result<(), AppError> {
    if user.is_active.get() {
        Ok(())
    } else {
        Err(AppError::forbidden_with_identifier(
            ACCOUNT_DISABLED,
            ACCOUNT_IS_DISABLED,
        ))
    }
}

/// Mints the access/refresh token pair for the granted scopes, valid for
/// the configured `OAUTH_TOKEN_TTL_SECS`. `user` is the account the
/// token acts as; `None` mints the app-only token of the
/// `client_credentials` grant (`user_id = NULL`).
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn issue_token(
    state: &AppState,
    user: Option<&User>,
    app: &OauthApp,
    scopes: Scopes,
) -> AppResult<OauthToken> {
    // `OAUTH_TOKEN_TTL_SECS` is a `usize`; on any target where it could
    // exceed `u64::MAX` the absurd upper bound fails the insert instead
    // of silently shortening the lifetime.
    let ttl_secs = u64::try_from(state.config.oauth_token_ttl_secs).unwrap_or(u64::MAX);
    oauth_token::create_token(
        &state.db,
        &NewToken {
            user_id: user.map(|user| user.id),
            app_id: app.id,
            scopes: scopes.to_string(),
            ttl: Some(Duration::from_secs(ttl_secs)),
        },
    )
    .await
}

/// The 200 body of a successful exchange: Pleroma's "token.json"
/// (`lib/pleroma/web/o_auth/token_view.ex`), extended with the owner's
/// actor id — which only a user-bound token has. For an app-only
/// `client_credentials` token the `refresh_token` and `me` keys are left
/// out of the JSON entirely.
#[derive(Debug, Serialize)]
struct TokenResponse {
    /// Token type, always `Bearer`.
    token_type: &'static str,
    /// The access token, to present as `Authorization: Bearer`.
    access_token: String,
    /// The refresh token, to exchange for a fresh pair; absent for an
    /// app-only token.
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// The granted scopes, space-separated.
    scope: String,
    /// Issue time, in unix seconds (Pleroma's `format_created_at`).
    created_at: i64,
    /// Seconds of validity left at render time.
    expires_in: i64,
    /// The token row's `UUIDv7` id, hyphenated (decision 4).
    id: String,
    /// The authenticated user's `ActivityPub` actor id; absent for an
    /// app-only token.
    #[serde(skip_serializing_if = "Option::is_none")]
    me: Option<String>,
}

impl TokenResponse {
    /// Renders the freshly minted `token`, owned by `user` — `None` for
    /// an app-only `client_credentials` token, which then serializes
    /// without `refresh_token` and `me`.
    fn new(token: OauthToken, user: Option<&User>) -> Self {
        let owner = user.map(|user| user.ap_id.clone());
        Self {
            token_type: TOKEN_TYPE_BEARER,
            access_token: token.token,
            refresh_token: owner.is_some().then_some(token.refresh_token),
            scope: token.scopes,
            created_at: token.inserted_at.timestamp(),
            expires_in: token
                .valid_until
                .signed_duration_since(Utc::now())
                .num_seconds(),
            id: token.id.to_string(),
            me: owner,
        }
    }
}

/// `POST /oauth/revoke` — retire one of the application's access tokens
/// (Pleroma's `token_revoke`, RFC 7009). No bearer authentication: the
/// application authenticates with its client credentials, like on
/// `/oauth/token`. Every answer, success or error, gets the no-store
/// headers.
async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiBody(params): ApiBody<RevokeParams>,
) -> Response {
    let response = match revoke_token(&state, &headers, &params).await {
        Ok(body) => body.into_response(),
        Err(err) => err.into_response(),
    };
    no_cache_headers(response)
}

/// The revocation, in Pleroma's order: the `token` parameter first (its
/// pattern match precedes the credential check), then the same
/// application authentication as [`token`], then the lookup. A token
/// issued to another application is refused like a credential failure
/// (400 [`INVALID_CREDENTIALS`]); an unknown token revokes to nothing
/// without an error, since its invalidation is already achieved
/// (RFC 7009 §2.2).
///
/// # Errors
///
/// 400 [`MISSING_TOKEN`] when the parameter is absent, 400
/// [`INVALID_CREDENTIALS`] for a failed app authentication or a foreign
/// token; database failures arrive as [`AppError::Database`].
async fn revoke_token(
    state: &AppState,
    headers: &HeaderMap,
    params: &RevokeParams,
) -> AppResult<Json<RevokeResponse>> {
    let Some(token) = params.token.as_deref() else {
        return Err(AppError::BadRequest(MISSING_TOKEN.to_owned()));
    };
    let app = validate_app(state, headers, params.body_credentials()).await?;
    if let Some((found, owner)) = oauth_token::get_with_app(&state.db, token).await? {
        if owner.id != app.id {
            return Err(invalid_credentials());
        }
        oauth_token::delete_token(&state.db, found.id).await?;
    }
    Ok(Json(RevokeResponse))
}

/// The request parameters of `POST /oauth/revoke`: the access token to
/// retire and the client credentials, the latter presented either here
/// or through HTTP Basic. As in [`TokenParams`] every field is optional
/// at the shape level, so the handler answers Pleroma's own
/// missing-parameter message instead of serde rejecting the body. The
/// manual `Debug` impl (see below) redacts the two secret fields.
#[derive(Deserialize)]
struct RevokeParams {
    /// The access token to revoke; absent or null is the 400
    /// [`MISSING_TOKEN`]. Renders as `<redacted>` in `Debug` output.
    #[serde(default)]
    token: Option<String>,
    /// The public OAuth client id, when not presented through HTTP Basic.
    #[serde(default)]
    client_id: Option<String>,
    /// The OAuth client secret, when not presented through HTTP Basic.
    /// Never in `Debug` output.
    #[serde(default)]
    client_secret: Option<String>,
}

/// [`token`](Self::token) and [`client_secret`](Self::client_secret) are
/// secret material and render as `<redacted>`; the client id shows.
impl fmt::Debug for RevokeParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RevokeParams")
            .field("token", &"<redacted>")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl RevokeParams {
    /// The `client_id`/`client_secret` body fields as a pair, `None`
    /// unless both are present.
    fn body_credentials(&self) -> Option<(&str, &str)> {
        Some((self.client_id.as_deref()?, self.client_secret.as_deref()?))
    }
}

/// The 200 body of a successful revocation: the empty JSON object, what
/// Pleroma's `token_revoke` renders (`json(conn, %{})`); the client is
/// meant to ignore the body of a revocation answer (RFC 7009 §2.2).
/// `Serialize` is manual because a derived unit struct would render
/// `null`, not `{}`.
#[derive(Debug)]
struct RevokeResponse;

impl Serialize for RevokeResponse {
    /// Writes the empty JSON object by serializing an empty map.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let empty = std::collections::BTreeMap::<String, serde_json::Value>::new();
        empty.serialize(serializer)
    }
}

/// Stamps the cache headers every token answer carries: issued
/// credentials must never be stored (RFC 6749 §5.1).
fn no_cache_headers(mut response: Response) -> Response {
    let stored_cache_control = response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
    drop(stored_cache_control);
    let stored_pragma = response
        .headers_mut()
        .insert(PRAGMA, HeaderValue::from_static(NO_CACHE));
    drop(stored_pragma);
    response
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, Request, StatusCode, header::AUTHORIZATION};
    use chrono::Duration;
    use serde_json::json;
    use std::sync::Arc;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        error::AppError,
        http_client::HttpClient,
        models::user::{DEFAULT_SCOPE, Flag},
        uploads::UploaderBackend,
        util::id::new_id,
    };

    /// A [`TokenParams`] with everything absent but `grant_type`, which
    /// `grant` sets.
    fn params_with_grant(grant: Option<&str>) -> TokenParams {
        TokenParams {
            grant_type: grant.map(str::to_owned),
            username: None,
            password: None,
            scope: None,
            client_id: None,
            client_secret: None,
            refresh_token: None,
        }
    }

    /// The message an [`AppError`] answers with.
    fn message_of(err: &AppError) -> String {
        err.to_string()
    }

    /// A stored-looking user row, active unless `active` is false.
    fn user_row(active: bool) -> User {
        let now = Utc::now();
        User {
            id: new_id(),
            nickname: "alice".to_owned(),
            ap_id: format!("{TEST_BASE_URL}/users/alice"),
            local: Flag::new(true),
            domain: None,
            actor_type: "Person".to_owned(),
            email: Some("alice@example.org".to_owned()),
            password_hash: Some("$argon2id$v=19$x$y$z".to_owned()),
            name: "alice".to_owned(),
            bio: String::new(),
            raw_bio: String::new(),
            avatar_url: None,
            banner_url: None,
            profile_url: None,
            locked: Flag::new(false),
            bot: Flag::new(false),
            discoverable: Flag::new(false),
            fields: json!([]),
            raw_fields: json!([]),
            inbox: None,
            shared_inbox: None,
            outbox: None,
            followers_url: None,
            following_url: None,
            featured_url: None,
            public_key: None,
            private_key: None,
            is_admin: Flag::new(false),
            is_moderator: Flag::new(false),
            is_active: Flag::new(active),
            hide_followers: Flag::new(false),
            hide_follows: Flag::new(false),
            hide_followers_count: Flag::new(false),
            hide_follows_count: Flag::new(false),
            hide_favorites: Flag::new(true),
            allow_following_move: Flag::new(true),
            skip_thread_containment: Flag::new(false),
            accepts_chat_messages: Flag::new(true),
            show_role: Flag::new(true),
            no_rich_text: Flag::new(false),
            default_scope: DEFAULT_SCOPE.to_owned(),
            sensitive_default: Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
            mfa_settings: user::mfa::MfaSettings::default_value(),
            background_image_url: None,
            birthday: None,
            show_birthday: Flag::new(false),
            also_known_as: Vec::new(),
            moved_to_ap_id: None,
            last_move_at: None,
            email_confirmed: Flag::new(true),
            approval_pending: Flag::new(false),
            registration_reason: None,
            last_active_at: None,
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            last_fetched_at: None,
            inserted_at: now,
            updated_at: now,
            preferred_frontend: None,
            is_suggested: Flag::new(false),
        }
    }

    /// A stored-looking token row for `app_id`, valid for an hour.
    fn token_row(app_id: Uuid, scopes: &str) -> OauthToken {
        let now = Utc::now();
        OauthToken {
            id: new_id(),
            token: "access-secret".to_owned(),
            refresh_token: "refresh-secret".to_owned(),
            user_id: Some(new_id()),
            app_id,
            scopes: scopes.to_owned(),
            valid_until: now + Duration::hours(1),
            inserted_at: now,
            updated_at: now,
        }
    }

    /// A stored-looking app row registered with `scopes`.
    fn app_row(scopes: &str) -> OauthApp {
        let now = Utc::now();
        OauthApp {
            id: new_id(),
            client_name: "Client".to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
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

    /// An `authorization: value` [`HeaderMap`].
    fn headers_with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let previous: Option<HeaderValue> =
            headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        assert!(previous.is_none(), "a fresh map holds no header");
        headers
    }

    #[test]
    fn a_missing_grant_type_is_pleromas_missing_parameter_error() {
        let err = require_grant(&params_with_grant(None)).unwrap_err();
        assert_eq!(
            message_of(&err),
            MISSING_GRANT_TYPE,
            "an absent grant_type is refused by name"
        );
    }

    #[test]
    fn an_unknown_grant_type_is_unsupported() {
        for grant in [
            "authorization_code",
            "urn:ietf:params:oauth:grant-type:device_code",
            "bogus",
        ] {
            let err = require_grant(&params_with_grant(Some(grant))).unwrap_err();
            assert_eq!(
                message_of(&err),
                UNSUPPORTED_GRANT_TYPE,
                "`{grant}` is named as unsupported"
            );
        }
    }

    #[test]
    fn the_password_grant_passes_the_gate() {
        assert_eq!(
            require_grant(&params_with_grant(Some(GRANT_TYPE_PASSWORD))).unwrap(),
            Grant::Password,
            "password selects the user-bound grant"
        );
    }

    #[test]
    fn the_client_credentials_grant_passes_the_gate() {
        assert_eq!(
            require_grant(&params_with_grant(Some(GRANT_TYPE_CLIENT_CREDENTIALS))).unwrap(),
            Grant::ClientCredentials,
            "client_credentials selects the app-only grant"
        );
    }

    #[test]
    fn the_refresh_token_grant_passes_the_gate() {
        assert_eq!(
            require_grant(&params_with_grant(Some(GRANT_TYPE_REFRESH_TOKEN))).unwrap(),
            Grant::RefreshToken,
            "refresh_token selects the rotation grant"
        );
    }

    #[test]
    fn basic_credentials_reads_the_pair_from_the_header() {
        let encoded = STANDARD.encode("client-id:client-secret");
        for scheme in ["Basic", "basic", "BASIC"] {
            assert_eq!(
                basic_credentials(&headers_with(&format!("{scheme} {encoded}"))),
                Some(("client-id".to_owned(), "client-secret".to_owned())),
                "`{scheme}` is the basic scheme"
            );
        }
    }

    #[test]
    fn basic_credentials_falls_back_on_anything_it_cannot_read() {
        // Wrong scheme, lone scheme, bad base64, non-UTF-8 bytes and a
        // payload without the `id:secret` colon are all no-pair cases
        // (Pleroma's with-block falls through to the body params).
        let cases = [
            String::from("Bearer tok3n"),
            String::from("Basic"),
            String::from("Basic not-valid-base64!!!"),
            format!("Basic {}", STANDARD.encode([0xff_u8, 0xfe, 0xfd])),
            format!("Basic {}", STANDARD.encode("just-one-field")),
        ];
        for value in &cases {
            assert_eq!(
                basic_credentials(&headers_with(value)),
                None,
                "`{value:?}` carries no usable pair"
            );
        }
    }

    #[test]
    fn body_credentials_need_both_fields() {
        let mut params = params_with_grant(Some(GRANT_TYPE_PASSWORD));
        assert_eq!(params.body_credentials(), None, "nothing sent");
        params.client_id = Some("id".to_owned());
        assert_eq!(params.body_credentials(), None, "an id without a secret");
        params.client_secret = Some("secret".to_owned());
        assert_eq!(
            params.body_credentials(),
            Some(("id", "secret")),
            "the pair completes"
        );
    }

    #[test]
    fn secret_matches_compares_without_leaking_a_prefix() {
        assert!(secret_matches("client-secret", "client-secret"), "equal");
        assert!(!secret_matches("client-secret", "client-secre"), "shorter");
        assert!(
            !secret_matches("client-secret", "client-sEcRet"),
            "same length, different bytes"
        );
        assert!(!secret_matches("a", "ab"), "a prefix is not a match");
    }

    #[test]
    fn requested_scopes_default_to_read() {
        for blank in [None, Some(""), Some("   ")] {
            let mut params = params_with_grant(Some(GRANT_TYPE_PASSWORD));
            params.scope = blank.map(str::to_owned);
            let scopes = requested_scopes(&params, &app_row("read")).expect("read is a subset");
            assert_eq!(
                scopes.to_string(),
                Scopes::default().to_string(),
                "`{blank:?}` falls back to the default"
            );
        }
    }

    #[test]
    fn requested_scopes_must_fit_inside_the_registration() {
        let mut params = params_with_grant(Some(GRANT_TYPE_PASSWORD));
        params.scope = Some("read:accounts write:statuses".to_owned());
        let scopes = requested_scopes(&params, &app_row("read write")).expect("sub-scopes fit");
        assert_eq!(
            scopes.to_string(),
            "read:accounts write:statuses",
            "the requested subset is granted verbatim"
        );

        params.scope = Some("read write".to_owned());
        let err =
            requested_scopes(&params, &app_row("read")).expect_err("write was not registered");
        assert_eq!(
            message_of(&err),
            INVALID_SCOPE,
            "a broad request is refused"
        );
    }

    #[test]
    fn an_active_account_is_ready_for_a_token() {
        check_account_ready(&user_row(true)).expect("active means ready");
    }

    #[test]
    fn a_deactivated_account_is_refused_with_its_identifier() {
        let err = check_account_ready(&user_row(false)).expect_err("deactivated is not ready");
        let AppError::ForbiddenWithIdentifier(message, identifier) = &err else {
            panic!("the refusal must carry an identifier, got {err}")
        };
        assert_eq!(message, ACCOUNT_DISABLED, "Pleroma's wording");
        assert_eq!(identifier, &ACCOUNT_IS_DISABLED, "and its reason code");
    }

    #[test]
    fn the_response_body_has_pleromas_token_shape() {
        let token = token_row(new_id(), "read write");
        let expected_id = token.id;
        let expected_created = token.inserted_at.timestamp();
        let user = user_row(true);
        let expected_ap_id = user.ap_id.clone();
        let json = serde_json::to_value(TokenResponse::new(token, Some(&user))).unwrap();
        assert_eq!(json["token_type"], "Bearer", "the token type");
        assert_eq!(json["access_token"], "access-secret", "the access token");
        assert_eq!(json["refresh_token"], "refresh-secret", "the refresh token");
        assert_eq!(
            json["scope"], "read write",
            "the granted scopes, space-joined"
        );
        assert_eq!(
            json["created_at"], expected_created,
            "issue time in unix seconds"
        );
        assert!(
            (3540..=3600).contains(&json["expires_in"].as_i64().expect("expires_in is a number"),),
            "an hour of validity, give or take a second of rendering"
        );
        assert_eq!(
            json["id"],
            expected_id.to_string(),
            "the token id, hyphenated"
        );
        assert_eq!(json["me"], expected_ap_id, "the owner's actor id");
        assert_eq!(
            json.as_object().expect("an object").len(),
            8,
            "exactly the documented keys"
        );
    }

    #[test]
    fn an_app_only_response_drops_the_refresh_token_and_me() {
        let token = token_row(new_id(), "read");
        let json = serde_json::to_value(TokenResponse::new(token, None)).unwrap();
        assert_eq!(json["token_type"], "Bearer", "the token type");
        assert_eq!(json["access_token"], "access-secret", "the access token");
        assert_eq!(json["scope"], "read", "the granted scopes");
        assert!(
            json.get("refresh_token").is_none(),
            "an app-only answer carries no refresh token: {json}"
        );
        assert!(json.get("me").is_none(), "and no actor id: {json}");
        assert_eq!(
            json.as_object().expect("an object").len(),
            6,
            "exactly the app-only documented keys"
        );
    }

    #[test]
    fn params_debug_redacts_the_secret_fields() {
        let mut params = params_with_grant(Some(GRANT_TYPE_PASSWORD));
        params.username = Some("alice".to_owned());
        params.password = Some("correct horse battery staple".to_owned());
        params.client_id = Some("client-id".to_owned());
        params.client_secret = Some("client-secret".to_owned());
        params.refresh_token = Some("refresh-secret".to_owned());
        let debug = format!("{params:?}");
        assert!(
            !debug.contains("correct horse battery staple")
                && !debug.contains("client-secret")
                && !debug.contains("refresh-secret"),
            "the secrets must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"password: "<redacted>""#)
                && debug.contains(r#"client_secret: "<redacted>""#)
                && debug.contains(r#"refresh_token: "<redacted>""#),
            "the markers name their fields: {debug}"
        );
        assert!(
            debug.contains("alice") && debug.contains("client-id"),
            "the rest shows"
        );
    }

    #[test]
    fn json_and_form_shapes_deserialize_to_the_same_params() {
        let full: TokenParams = crate::extract::into_params(json!({
            "grant_type": "refresh_token",
            "username": "alice",
            "password": "pw",
            "scope": "read write",
            "client_id": "id",
            "client_secret": "secret",
            "refresh_token": "a-refresh-token",
        }))
        .unwrap();
        assert_eq!(
            full.grant_type.as_deref(),
            Some(GRANT_TYPE_REFRESH_TOKEN),
            "the grant"
        );
        assert_eq!(full.username.as_deref(), Some("alice"), "the username");
        assert_eq!(
            full.scope.as_deref(),
            Some("read write"),
            "the scope string"
        );
        assert_eq!(full.client_id.as_deref(), Some("id"), "the client id");
        assert_eq!(
            full.refresh_token.as_deref(),
            Some("a-refresh-token"),
            "the refresh token"
        );
        let empty: TokenParams = crate::extract::into_params(json!({})).unwrap();
        assert!(
            empty.grant_type.is_none() && empty.username.is_none(),
            "an empty body (or a form without the keys) leaves everything absent"
        );
    }

    #[test]
    fn every_answer_gets_the_no_store_headers() {
        let error_response =
            no_cache_headers(AppError::BadRequest(INVALID_CREDENTIALS.to_owned()).into_response());
        let ok_response = no_cache_headers(Json(json!({"token_type": "Bearer"})).into_response());
        for response in [error_response, ok_response] {
            assert_eq!(
                response.headers().get(CACHE_CONTROL),
                Some(&HeaderValue::from_static(NO_STORE)),
                "cache-control: no-store"
            );
            assert_eq!(
                response.headers().get(PRAGMA),
                Some(&HeaderValue::from_static(NO_CACHE)),
                "pragma: no-cache"
            );
        }
    }

    #[tokio::test]
    async fn a_refresh_without_the_refresh_token_parameter_is_refused_by_name() {
        // The parameter gate runs before the credential check, and the
        // credential check before any database access, so the lazy pool
        // of `test_state` is never consulted.
        let err = refresh_token_grant(
            &test_state(),
            &HeaderMap::new(),
            &params_with_grant(Some(GRANT_TYPE_REFRESH_TOKEN)),
        )
        .await
        .expect_err("no refresh token is no rotation");
        assert_eq!(
            message_of(&err),
            MISSING_REFRESH_TOKEN,
            "the refusal names the missing parameter even without credentials"
        );
    }

    /// [`RevokeParams`] with the three fields as given.
    fn revoke_params(
        token: Option<&str>,
        client_id: Option<&str>,
        secret: Option<&str>,
    ) -> RevokeParams {
        RevokeParams {
            token: token.map(str::to_owned),
            client_id: client_id.map(str::to_owned),
            client_secret: secret.map(str::to_owned),
        }
    }

    #[test]
    fn revoke_body_credentials_need_both_fields() {
        assert_eq!(
            revoke_params(Some("tok"), None, None).body_credentials(),
            None,
            "nothing sent"
        );
        assert_eq!(
            revoke_params(Some("tok"), Some("id"), None).body_credentials(),
            None,
            "an id without a secret"
        );
        assert_eq!(
            revoke_params(Some("tok"), Some("id"), Some("secret")).body_credentials(),
            Some(("id", "secret")),
            "the pair completes"
        );
    }

    #[test]
    fn revoke_params_debug_redacts_the_token_and_client_secret() {
        let params = revoke_params(
            Some("access-secret"),
            Some("client-id"),
            Some("client-secret"),
        );
        let debug = format!("{params:?}");
        assert!(
            !debug.contains("access-secret") && !debug.contains("client-secret"),
            "the secrets must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"token: "<redacted>""#)
                && debug.contains(r#"client_secret: "<redacted>""#),
            "the markers name their fields: {debug}"
        );
        assert!(debug.contains("client-id"), "the public id shows");
    }

    #[test]
    fn revoke_json_and_form_shapes_deserialize_to_the_same_params() {
        let full: RevokeParams = crate::extract::into_params(json!({
            "token": "a-token",
            "client_id": "id",
            "client_secret": "secret",
        }))
        .unwrap();
        assert_eq!(full.token.as_deref(), Some("a-token"), "the token");
        assert_eq!(full.client_id.as_deref(), Some("id"), "the client id");
        // A form body parses to the same string-shaped map (the form
        // keys themselves are covered by `crate::extract`'s tests).
        let form: RevokeParams = crate::extract::into_params(json!({
            "token": "a-token",
            "client_id": "id",
        }))
        .unwrap();
        assert_eq!(form.token.as_deref(), Some("a-token"), "same token");
        assert_eq!(form.client_secret, None, "an absent field stays absent");
    }

    #[test]
    fn a_successful_revoke_answers_the_empty_object() {
        assert_eq!(
            serde_json::to_value(RevokeResponse).unwrap(),
            json!({}),
            "Pleroma renders the empty JSON object"
        );
    }

    #[tokio::test]
    async fn a_revoke_without_the_token_parameter_is_refused_by_name() {
        // The parameter gate runs before the credential check, and the
        // credential check before any database access, so the lazy pool
        // of `test_state` is never consulted.
        let err = revoke_token(
            &test_state(),
            &HeaderMap::new(),
            &revoke_params(None, None, None),
        )
        .await
        .expect_err("no token is no revocation");
        assert_eq!(
            message_of(&err),
            MISSING_TOKEN,
            "the refusal names the missing parameter even without credentials"
        );
    }

    /// The state the router-shaped tests below need: config only, the
    /// lazy pool is never consulted (the grant-type and token gates
    /// fail first).
    fn test_state() -> AppState {
        let config = Arc::new(Config::for_tests());
        let db = PgPool::connect_lazy("postgres://localhost/unused").unwrap();
        let hub = crate::streaming::hub::Hub::new(&db);
        AppState {
            db,
            hub,
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    #[tokio::test]
    async fn the_route_answers_at_oauth_token() {
        use tower::ServiceExt as _;
        let state = test_state();
        let app = router(&state).with_state(state);
        let request = Request::post("/oauth/token")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"grant_type": "bogus"}).to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "the routed endpoint answers the bad grant type"
        );
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&HeaderValue::from_static(NO_STORE)),
            "and the no-store headers"
        );
    }

    #[tokio::test]
    async fn the_revoke_route_answers_at_oauth_revoke() {
        use tower::ServiceExt as _;
        let state = test_state();
        let app = router(&state).with_state(state);
        let request = Request::post("/oauth/revoke")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"client_id": "id", "client_secret": "secret"}).to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "the routed endpoint refuses a body without `token`"
        );
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&HeaderValue::from_static(NO_STORE)),
            "and the no-store headers"
        );
    }
}
