//! Bearer authentication: the `FromRequestParts<AppState>` extractors and
//! the scope guards every authenticated endpoint builds on
//! (`docs/ARCHITECTURE.md` decision 9).
//!
//! Where Pleroma runs these as plugs (`lib/pleroma/web/plugs/o_auth_plug.ex`
//! fetches the token, `ensure_authenticated_plug.ex` demands a user,
//! `o_auth_scopes_plug.ex` checks scopes, and `authentication_plug.ex`
//! covers password auth), here it is one module of axum extractors. Every
//! extractor reads the token from the `Authorization: Bearer <token>`
//! header — the scheme is matched case-insensitively — falling back to
//! the `access_token` query parameter, which Pleroma and Mastodon
//! accept for `GET` and the streaming `WebSocket`; when both are
//! present the header wins. A token is valid when
//! [`get_with_app`](crate::models::oauth_token::get_with_app) finds its
//! row, `valid_until` lies in the future, and — for user-bound tokens —
//! the owner is still active and approved; [`AuthUser`] reports a
//! deactivated owner as its own 403 and an unapproved one as 403
//! `awaiting_approval`, every other extractor lumps both in with the
//! invalid tokens. An account whose email is merely unconfirmed keeps
//! working here: confirmation refuses a *login*, not a credential that
//! was already issued.
//!
//! The extractors and their rejections (all bodies are the `{"error": …}`
//! shape of decision 8, worded as Pleroma's fallback renders them):
//!
//! - [`AuthUser`]: requires a valid user token. No token at all: 401
//!   "Invalid credentials"; unknown, expired or deactivated: 401 "The
//!   access token is invalid" (403 "Your account is currently disabled"
//!   for the deactivated owner, 403 "Your account is awaiting approval."
//!   with `identifier` `awaiting_approval` for the unapproved one); an
//!   app-only token: 403 "This API requires an authenticated user".
//! - [`MaybeAuthUser`]: no token yields `None`, a valid user token the
//!   user, an app-only token `None` — but an invalid, expired or
//!   deactivated token is a 401, never a silently ignored one. The
//!   presented token rides along, and
//!   [`MaybeAuthUser::require_user_scope`] is the gate of the routes
//!   Pleroma runs with `OAuthScopesPlug … fallback:
//!   :proceed_unauthenticated`: an authenticated request needs the
//!   scope, an anonymous one proceeds.
//! - [`AppAuth`] and [`AuthContext`]: require a valid token, app-only or
//!   user-bound alike (Pleroma's `OAuthPlug` assigns `token` and `app`;
//!   `POST /api/v1/accounts` is the endpoint that asks for this rather
//!   than a user).
//!
//! [`require_scope`] and the [`AuthUser::require`] /
//! [`AuthUser::require_any`] helpers enforce Mastodon scopes through
//! [`Scopes::contains`], answering 403 with Pleroma's
//! "Insufficient permissions: …." wording.

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, Uri, header::AUTHORIZATION, request::Parts},
};
use chrono::Utc;
use url::form_urlencoded;

use crate::{
    error::{AppError, AppResult},
    models::{
        oauth_app::OauthApp,
        oauth_token::{self, OauthToken},
        user::{self, AccountStatus, User},
    },
    oauth::scopes::Scopes,
    state::AppState,
};

/// 401 body when a token was required but none was presented at all
/// (Pleroma's `EnsureAuthenticatedPlug` "Invalid credentials.").
const NO_TOKEN: &str = "Invalid credentials";
/// 401 body when a presented token does not resolve to a live
/// credential (Pleroma's OAuth fallback wording).
const INVALID_TOKEN: &str = "The access token is invalid";
/// 403 body when a user-bound credential is required but the token is
/// app-only (Pleroma: "This API requires an authenticated user").
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// 403 body for a valid token whose user account is deactivated
/// (Pleroma's disabled-account wording).
const DISABLED: &str = "Your account is currently disabled";
/// 403 body for an account staff have not approved yet, and the identifier
/// every refusal of it carries: Pleroma's
/// `handle_token_exchange_error({:account_status, :approval_pending})` and
/// the matching `create` answer of
/// `lib/pleroma/web/mastodon_api/controllers/account_controller.ex`.
///
/// [`check_account_ready`](crate::api::oauth::check_account_ready) and this
/// module's bearer rejection share the pair.
pub(crate) const AWAITING_APPROVAL: &str = "Your account is awaiting approval.";
/// The machine-readable identifier of [`AWAITING_APPROVAL`].
pub(crate) const AWAITING_APPROVAL_ID: &str = "awaiting_approval";
/// 403 body for a login whose account has not confirmed its email address
/// (Pleroma's `confirmation_pending` refusal), with its identifier. Shared
/// with [`check_account_ready`](crate::api::oauth::check_account_ready).
pub(crate) const EMAIL_UNCONFIRMED: &str = "Your login is missing a confirmed e-mail address";
/// The machine-readable identifier of [`EMAIL_UNCONFIRMED`].
pub(crate) const EMAIL_UNCONFIRMED_ID: &str = "missing_confirmed_email";
/// `Authorization` scheme the header token is expected under, matched
/// case-insensitively.
const BEARER_SCHEME: &str = "Bearer";
/// Query parameter holding the bearer token (Pleroma's `OAuthPlug`
/// accepts it for `GET` and the streaming `WebSocket`).
const ACCESS_TOKEN_PARAM: &str = "access_token";

/// A resolved bearer credential: what a valid token acts as, handed to
/// the handlers that need the whole picture.
#[derive(Debug)]
pub struct AuthContext {
    /// The `oauth_tokens` row the request presented.
    pub token: OauthToken,
    /// The `users` row the token is bound to; `None` for an app-only
    /// (`client_credentials`) token.
    pub user: Option<User>,
    /// The `oauth_apps` row that issued the token.
    pub app: OauthApp,
}

/// Extractor for endpoints that require a user token: `AuthUser(user,
/// token)`.
///
/// Rejections are those listed in the [module docs](self) for
/// [`AuthUser`]: a deactivated account answers 403 "Your account is
/// currently disabled", an app-only token 403 "This API requires an
/// authenticated user".
#[derive(Debug)]
pub struct AuthUser(
    /// The authenticated user.
    pub User,
    /// The token the request authenticated with.
    pub OauthToken,
);

/// Extractor for endpoints that answer anonymously:
/// `MaybeAuthUser(user, token)`.
///
/// The user is `None` when the request carried no token or an app-only
/// one. An invalid, expired or deactivated token is still a 401 "The
/// access token is invalid" — it is never silently ignored. The token
/// rides along so a scope-gated endpoint with optional auth can check
/// what a *presented* credential grants (see
/// [`require_user_scope`](Self::require_user_scope)); handlers that
/// only need the viewer destructure it away.
#[derive(Debug)]
pub struct MaybeAuthUser(
    /// The user behind a valid user token; `None` when no (or no
    /// user-bound) token was presented.
    pub Option<User>,
    /// The valid token the request presented; `None` when it presented
    /// no token at all.
    pub Option<OauthToken>,
);

/// Extractor for endpoints that any valid token satisfies — app-only or
/// user-bound alike, e.g. `POST /api/v1/accounts`: `AppAuth(app, token)`.
/// Rejections are the 401s listed in the [module docs](self).
#[derive(Debug)]
pub struct AppAuth(
    /// The application the presented token was issued for.
    pub OauthApp,
    /// The valid token.
    pub OauthToken,
);

/// `AuthContext` from a valid token (app-only or user-bound); see
/// [`AuthUser`] for the exact rejections, which are those of
/// [`AppAuth`].
impl FromRequestParts<AppState> for AuthContext {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        require_valid(parts, state).await
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(raw) = bearer_string(parts) else {
            return Err(AppError::Unauthorized(NO_TOKEN.to_owned()));
        };
        match resolve(state, &raw).await? {
            Auth::Valid(ctx) => {
                let AuthContext { token, user, .. } = *ctx;
                user.map_or_else(
                    || Err(AppError::Forbidden(USER_REQUIRED.to_owned())),
                    |user| Ok(Self(user, token)),
                )
            }
            Auth::Deactivated => Err(AppError::Forbidden(DISABLED.to_owned())),
            Auth::ApprovalPending => Err(AppError::forbidden_with_identifier(
                AWAITING_APPROVAL,
                AWAITING_APPROVAL_ID,
            )),
            Auth::Invalid => Err(AppError::Unauthorized(INVALID_TOKEN.to_owned())),
        }
    }
}

impl FromRequestParts<AppState> for MaybeAuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(raw) = bearer_string(parts) else {
            return Ok(Self(None, None));
        };
        match resolve(state, &raw).await? {
            Auth::Valid(ctx) => {
                let AuthContext { token, user, .. } = *ctx;
                Ok(Self(user, Some(token)))
            }
            Auth::Deactivated | Auth::ApprovalPending | Auth::Invalid => {
                Err(AppError::Unauthorized(INVALID_TOKEN.to_owned()))
            }
        }
    }
}

impl MaybeAuthUser {
    /// The scope check of an optionally-authenticated route (Pleroma's
    /// `OAuthScopesPlug` with `fallback: :proceed_unauthenticated`): a
    /// request that presented a valid *user* token must grant `needed`,
    /// while an anonymous request and an app-only token (which assigns no
    /// user) proceed unauthenticated.
    ///
    /// # Errors
    ///
    /// [`AppError::Forbidden`] with Pleroma's
    /// "Insufficient permissions: \<needed\>." wording when the request is
    /// authenticated and its token does not cover `needed`.
    pub fn require_user_scope(&self, needed: &str) -> Result<(), AppError> {
        match (&self.0, &self.1) {
            (Some(_), Some(token)) => require_scope(token, needed),
            _ => Ok(()),
        }
    }
}

impl FromRequestParts<AppState> for AppAuth {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ctx = require_valid(parts, state).await?;
        Ok(Self(ctx.app, ctx.token))
    }
}

impl AuthUser {
    /// Requires the token to grant the scope `needed` through the
    /// Mastodon hierarchy ([`Scopes::contains`]: a `read` token passes
    /// `read:statuses`, a `read:statuses` token never passes `read`).
    ///
    /// # Errors
    ///
    /// [`AppError::Forbidden`] with Pleroma's
    /// "Insufficient permissions: \<needed\>." wording when it does not.
    pub fn require(&self, needed: &str) -> Result<(), AppError> {
        require_scope(&self.1, needed)
    }

    /// Requires the token to grant **any** of `needed` (same hierarchy).
    ///
    /// # Errors
    ///
    /// [`AppError::Forbidden`] naming every scope of `needed` joined
    /// with `" | "` (Pleroma's `|`-mode scopes plug wording) when the
    /// token grants none of them.
    pub fn require_any(&self, needed: &[&str]) -> Result<(), AppError> {
        require_any_scope(&self.1, needed)
    }
}

/// Resolves the raw bearer string `raw` into the credential behind it.
///
/// For the callers that read a token from somewhere a [`Parts`] cannot be
/// built from: the streaming WebSocket takes it from its query string *or* the
/// `Sec-WebSocket-Protocol` header, where the first protocol value is the
/// token (Pleroma's `find_access_token`,
/// `lib/pleroma/web/mastodon_api/websocket_handler.ex`), and rejects the
/// upgrade with its own 401 instead of an extractor's rejection.
///
/// `None` is every credential the extractors refuse — no such row, past
/// `valid_until`, a deactivated owner, or an app-only token that names no
/// user to act as.
///
/// # Errors
///
/// [`AppError::Database`] from the two lookups `resolve` runs.
pub async fn credential(state: &AppState, raw: &str) -> AppResult<Option<AuthContext>> {
    match resolve(state, raw).await? {
        Auth::Valid(ctx) if ctx.user.is_some() => Ok(Some(*ctx)),
        Auth::Valid(_) | Auth::Deactivated | Auth::ApprovalPending | Auth::Invalid => Ok(None),
    }
}

/// Checks that `token` grants the scope `needed` (see
/// [`AuthUser::require`] for the hierarchy).
///
/// # Errors
///
/// [`AppError::Forbidden`] with the message
/// `"Insufficient permissions: {needed}."` —
/// `o_auth_scopes_plug.ex`'s text — when the token's scopes do not
/// cover it.
pub fn require_scope(token: &OauthToken, needed: &str) -> Result<(), AppError> {
    require_any_scope(token, &[needed])
}

/// [`require_scope`] against a set of alternatives: granted when any one
/// of `needed` is covered; a single scope is exactly [`require_scope`]'s
/// message, several join with `" | "` as Pleroma's `|` operator does.
fn require_any_scope(token: &OauthToken, needed: &[&str]) -> Result<(), AppError> {
    let granted = Scopes::parse(&token.scopes);
    if needed.iter().any(|scope| granted.contains(scope)) {
        return Ok(());
    }
    Err(AppError::Forbidden(format!(
        "Insufficient permissions: {}.",
        needed.join(" | ")
    )))
}

/// What looking a bearer string up in the database came to.
#[derive(Debug)]
enum Auth {
    /// The token exists, is unexpired and belongs to no deactivated
    /// account.
    Valid(Box<AuthContext>),
    /// The token is live but its user's account is deactivated;
    /// [`AuthUser`] reports this as 403 [`DISABLED`], every other
    /// extractor treats it like an invalid token.
    Deactivated,
    /// The token is live but its user's account still awaits staff
    /// approval; [`AuthUser`] reports this as 403 [`AWAITING_APPROVAL`]
    /// with [`AWAITING_APPROVAL_ID`], every other extractor treats it
    /// like an invalid token. A credential minted before the mode was
    /// switched on (or issued to the account by an app token) is worth
    /// nothing while approval is pending.
    ApprovalPending,
    /// No row carries this token, or the row is past `valid_until`.
    Invalid,
}

/// The bearer identity a token-requiring extractor shares: the raw
/// string of [`bearer_string`] resolved, with 401 [`NO_TOKEN`] when no
/// token was presented and 401 [`INVALID_TOKEN`] when it does not
/// resolve (a deactivated or unapproved owner included).
async fn require_valid(parts: &Parts, state: &AppState) -> AppResult<AuthContext> {
    let Some(raw) = bearer_string(parts) else {
        return Err(AppError::Unauthorized(NO_TOKEN.to_owned()));
    };
    match resolve(state, &raw).await? {
        Auth::Valid(ctx) => Ok(*ctx),
        Auth::Deactivated | Auth::ApprovalPending | Auth::Invalid => {
            Err(AppError::Unauthorized(INVALID_TOKEN.to_owned()))
        }
    }
}

/// Looks the access token `raw` up with its app and user and rules it
/// valid: the row must exist, `valid_until` must lie in the future
/// (Pleroma's `Token.expired?`), and a bound user must still be active and
/// approved — [`account_status`](User::account_status) is what decides, and
/// an unconfirmed address is not a refusal here (only a login is refused
/// for that, in [`crate::api::oauth`]).
async fn resolve(state: &AppState, raw: &str) -> AppResult<Auth> {
    let Some((token, app)) = oauth_token::get_with_app(&state.db, raw).await? else {
        return Ok(Auth::Invalid);
    };
    if token.valid_until <= Utc::now() {
        return Ok(Auth::Invalid);
    }
    let user = match token.user_id {
        Some(user_id) => user::get_by_id(&state.db, user_id).await?,
        None => None,
    };
    let status = user
        .as_ref()
        .map_or(AccountStatus::Active, User::account_status);
    match status {
        AccountStatus::Deactivated => Ok(Auth::Deactivated),
        AccountStatus::ApprovalPending => Ok(Auth::ApprovalPending),
        AccountStatus::Active | AccountStatus::ConfirmationPending => {
            Ok(Auth::Valid(Box::new(AuthContext { token, user, app })))
        }
    }
}

/// Looks up the bearer token the request presents: the
/// `Authorization: Bearer <token>` header first ("header wins"), then the
/// `access_token` query parameter.
pub(crate) fn bearer_string(parts: &Parts) -> Option<String> {
    bearer_from_headers(&parts.headers).or_else(|| bearer_from_query(&parts.uri))
}

/// The token of an `Authorization: Bearer <token>` header, the scheme
/// matched case-insensitively and any whitespace around the token
/// dropped; `None` for an absent or unparsable header, a different
/// scheme, or an empty token.
pub(crate) fn bearer_from_headers(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case(BEARER_SCHEME) {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// The `access_token` query parameter of `uri`, percent-decoded; the
/// first occurrence wins and an empty value counts as no token.
pub(crate) fn bearer_from_query(uri: &Uri) -> Option<String> {
    let query = uri.query()?;
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| *key == ACCESS_TOKEN_PARAM)
        .map(|(_, value)| value.into_owned())
        .filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{HeaderValue, Request, StatusCode, Uri, header::AUTHORIZATION},
        response::IntoResponse as _,
    };
    use chrono::Duration;
    use uuid::Uuid;

    use super::*;

    /// A stored-looking token row granting `scopes`, minted right now.
    fn token_with(scopes: &str) -> OauthToken {
        let now = Utc::now();
        OauthToken {
            id: Uuid::now_v7(),
            token: "access-secret".to_owned(),
            refresh_token: "refresh-secret".to_owned(),
            user_id: None,
            app_id: Uuid::now_v7(),
            scopes: scopes.to_owned(),
            valid_until: now + Duration::days(1),
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

    /// `target` as a [`Uri`].
    fn parse_uri(target: &str) -> Uri {
        target.parse().unwrap()
    }

    #[test]
    fn require_scope_accepts_granted_broader_and_follow_aliases() {
        require_scope(&token_with("read"), "read:statuses").expect("`read` covers `read:statuses`");
        require_scope(&token_with("read:statuses"), "read:statuses")
            .expect("an exact grant passes");
        require_scope(&token_with("follow"), "read:follows")
            .expect("Mastodon's `follow` covers `read:follows`");
    }

    #[test]
    fn require_scope_denies_with_pleroma_wording() {
        let err = require_scope(&token_with("read:accounts"), "read")
            .expect_err("a narrow grant never covers `read`");
        assert_eq!(
            err.to_string(),
            "Insufficient permissions: read.",
            "the message names the missing scope, period included"
        );
        assert_eq!(
            err.into_response().status(),
            StatusCode::FORBIDDEN,
            "a scope denial is a 403"
        );
    }

    #[test]
    fn require_any_scope_passes_on_one_match_and_lists_all_misses() {
        require_any_scope(&token_with("read:accounts"), &["read:accounts", "read"])
            .expect("the first alternative suffices");
        require_any_scope(&token_with("read"), &["read:accounts", "read"])
            .expect("a broader grant covers the second");
        let err = require_any_scope(&token_with("write"), &["read:accounts", "read"])
            .expect_err("`write` grants neither");
        assert_eq!(
            err.to_string(),
            "Insufficient permissions: read:accounts | read.",
            "the denial joins the missing scopes with the `|` operator"
        );
    }

    #[test]
    fn require_user_scope_passes_everything_without_a_user_token() {
        // The gated arm — a user and its token — needs a stored user row,
        // so the endpoint tests in `tests/api/accounts.rs` drive it over HTTP;
        // what can be built from nothing is the gate's pass-through side.
        MaybeAuthUser(None, None)
            .require_user_scope("read:accounts")
            .expect("an anonymous request proceeds unauthenticated");
        MaybeAuthUser(None, Some(token_with("write")))
            .require_user_scope("read:accounts")
            .expect("an app-only token names no user, so the gate does not bind");
    }

    /// The [`AppState`] the credential lookups run on, over a slot's pool.
    fn app_state(pool: &sqlx::PgPool) -> AppState {
        use std::sync::Arc;

        use crate::{config::Config, http_client::HttpClient, uploads::UploaderBackend};

        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// A fresh local user named `nickname`, active unless `active` says so.
    async fn reader(pool: &sqlx::PgPool, nickname: &str, active: bool) -> User {
        use crate::models::user::{Flag, NewUser, insert};

        insert(
            pool,
            &NewUser {
                is_active: Flag::new(active),
                ..NewUser::local(nickname, &format!("https://test.example/users/{nickname}"))
            },
        )
        .await
        .expect("user fixture inserts")
    }

    /// A token minted by a fresh app for `owner` (`None` for an app-only
    /// credential), with the default lifetime.
    async fn token(pool: &sqlx::PgPool, owner: Option<&User>, scopes: &str) -> OauthToken {
        use crate::models::{
            oauth_app::{NewOauthApp, create as create_app},
            oauth_token::{NewToken, create_token},
        };

        let app = create_app(
            pool,
            &NewOauthApp {
                client_name: "Credential Client".to_owned(),
                redirect_uris: "https://client.example/callback".to_owned(),
                scopes: scopes.to_owned(),
                website: None,
                user_id: owner.map(|found| found.id),
            },
        )
        .await
        .expect("app fixture inserts");
        create_token(
            pool,
            &NewToken {
                user_id: owner.map(|found| found.id),
                app_id: app.id,
                scopes: scopes.to_owned(),
                ttl: None,
            },
        )
        .await
        .expect("token fixture inserts")
    }

    /// Moves `token`'s row an hour into the past: the state a lapsed
    /// credential has, which no lookup should honour.
    async fn lapse(pool: &sqlx::PgPool, id: Uuid) {
        let _lapsed = sqlx::query(
            "UPDATE oauth_tokens SET valid_until = now() - interval '1 hour' WHERE id = $1",
        )
        .bind(id)
        .execute(pool)
        .await
        .expect("the expiry update runs");
    }

    #[tokio::test]
    async fn credential_resolves_a_live_user_token() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let app = app_state(db.pool());
        let owner = reader(&app.db, "credential-reader", true).await;
        let live = token(&app.db, Some(&owner), "read").await;

        let resolved = credential(&app, &live.token).await.unwrap();

        assert_eq!(
            resolved.and_then(|ctx| ctx.user.map(|found| found.id)),
            Some(owner.id),
            "a live user token resolves to its user"
        );
    }

    #[tokio::test]
    async fn credential_refuses_every_token_that_names_no_live_user() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let app = app_state(db.pool());
        let owner = reader(&app.db, "credential-owner", true).await;
        let dormant = reader(&app.db, "credential-dormant", false).await;
        let unknown = "no-such-token".to_owned();
        let lapsed = token(&app.db, Some(&owner), "read").await;
        lapse(&app.db, lapsed.id).await;
        let app_only = token(&app.db, None, "read").await;
        let dormant_token = token(&app.db, Some(&dormant), "read").await;

        for (raw, who) in [
            (unknown.as_str(), "an unknown string"),
            (lapsed.token.as_str(), "an expired token"),
            (app_only.token.as_str(), "an app-only token"),
            (dormant_token.token.as_str(), "a deactivated owner's token"),
        ] {
            assert!(
                credential(&app, raw).await.unwrap().is_none(),
                "{who} credentials nobody"
            );
        }
    }

    #[test]
    fn bearer_from_headers_matches_the_scheme_case_insensitively() {
        for scheme in ["Bearer", "bearer", "BEARER"] {
            let headers = headers_with(&format!("{scheme} tok3n"));
            assert_eq!(
                bearer_from_headers(&headers).as_deref(),
                Some("tok3n"),
                "`{scheme}` is the bearer scheme"
            );
        }
    }

    #[test]
    fn bearer_from_headers_needs_a_token_after_the_scheme() {
        for value in [
            "Basic dXNlcjpwYXNz",
            "Token abc123",
            "Bearer",
            "Bearer   ",
            "",
        ] {
            assert_eq!(
                bearer_from_headers(&headers_with(value)),
                None,
                "`{value:?}` carries no bearer token"
            );
        }
        assert_eq!(
            bearer_from_headers(&HeaderMap::new()),
            None,
            "no header, no token"
        );
    }

    #[test]
    fn bearer_from_headers_trims_the_token_but_keeps_its_body() {
        assert_eq!(
            bearer_from_headers(&headers_with("Bearer \ttab-token \t")).as_deref(),
            Some("tab-token"),
            "surrounding whitespace is dropped"
        );
        assert_eq!(
            bearer_from_headers(&headers_with("Bearer first second")).as_deref(),
            Some("first second"),
            "the rest of the value is the token, Pleroma's regex aside"
        );
    }

    #[test]
    fn bearer_from_query_reads_the_access_token_parameter() {
        assert_eq!(
            bearer_from_query(&parse_uri("/x?access_token=q4ry")).as_deref(),
            Some("q4ry"),
            "a lone parameter"
        );
        assert_eq!(
            bearer_from_query(&parse_uri("/x?a=1&access_token=late")).as_deref(),
            Some("late"),
            "any position works"
        );
        assert_eq!(
            bearer_from_query(&parse_uri("/x?access_token=a%2Bb")).as_deref(),
            Some("a+b"),
            "the value is percent-decoded"
        );
        assert_eq!(
            bearer_from_query(&parse_uri("/x?access_token=one&access_token=two")).as_deref(),
            Some("one"),
            "the first occurrence wins"
        );
    }

    #[test]
    fn bearer_from_query_without_a_usable_parameter_is_none() {
        for target in ["/x", "/x?other=1", "/x?access_token="] {
            assert_eq!(
                bearer_from_query(&parse_uri(target)),
                None,
                "`{target}` presents no token"
            );
        }
    }

    #[test]
    fn bearer_string_prefers_the_header_over_the_query() {
        let request = Request::builder()
            .uri("/x?access_token=from-query")
            .header(AUTHORIZATION, "Bearer from-header")
            .body(Body::empty())
            .unwrap();
        let (parts, _) = request.into_parts();
        assert_eq!(
            bearer_string(&parts).as_deref(),
            Some("from-header"),
            "the header wins"
        );
    }

    #[test]
    fn bearer_string_falls_back_to_the_query_without_a_bearer_header() {
        for authorization in [None, Some("Basic dXNlcjpwYXNz"), Some("Bearer   ")] {
            let mut builder = Request::builder().uri("/x?access_token=from-query");
            if let Some(value) = authorization {
                builder = builder.header(AUTHORIZATION, value);
            }
            let (parts, _) = builder.body(Body::empty()).unwrap().into_parts();
            assert_eq!(
                bearer_string(&parts).as_deref(),
                Some("from-query"),
                "with authorization {authorization:?} the query still speaks"
            );
        }
        let (parts, _) = Request::get("/x").body(Body::empty()).unwrap().into_parts();
        assert_eq!(bearer_string(&parts), None, "neither source, no token");
    }
}
