//! The grant bodies of `POST /oauth/token`: what each grant type
//! [`require_grant`](super::require_grant) names does once its
//! parameters pass the gates of [`super::params`] — the password grant
//! with its MFA detour, the app-only `client_credentials` grant, the
//! rotating `refresh_token` grant and the one-time
//! `authorization_code` redemption — and the 403 challenge body a
//! password grant to an MFA-enabled account answers with instead of a
//! token.

use axum::{Json, http::HeaderMap};
use serde::Serialize;
use sqlx::PgPool;

use super::{
    MISSING_CODE, MISSING_REFRESH_TOKEN, TokenReply,
    issue::{TokenResponse, authenticate_user, check_account_ready, issue_token},
    mfa,
    params::{TokenParams, invalid_credentials, requested_scopes, validate_app},
};
use crate::{
    error::{AppError, AppResult},
    models::{
        oauth_token::{self, OauthToken},
        user::{self, User},
    },
    oauth::scopes::Scopes,
    state::AppState,
};

/// The password grant: app, scopes, user login, account readiness, the
/// MFA gate, then the user-bound token pair. An MFA-enabled account
/// gets no token: its grant's scopes are parked on a pending
/// authorization, a five-minute challenge row links it, and the answer
/// is the 403 `mfa_required` body (Pleroma's
/// `handle_token_exchange_error({:mfa_required, user, auth, _})`).
pub(super) async fn password_grant(
    state: &AppState,
    headers: &HeaderMap,
    params: &TokenParams,
) -> AppResult<TokenReply> {
    let app = validate_app(state, headers, params.body_credentials()).await?;
    let scopes = requested_scopes(params, &app)?;
    let user = authenticate_user(&state.db, params).await?;
    check_account_ready(&user)?;
    if mfa::required(&user) {
        let challenge = mfa::start_challenge(state, &user, &app, &scopes, "").await?;
        return Ok(TokenReply::MfaRequired(Json(MfaRequiredResponse {
            error: MFA_REQUIRED,
            mfa_token: challenge,
            supported_challenge_types: SUPPORTED_CHALLENGE_TYPES,
        })));
    }
    let token = issue_token(state, Some(&user), &app, scopes).await?;
    Ok(TokenReply::Issued(Json(TokenResponse::new(
        token,
        Some(&user),
    ))))
}

/// The `client_credentials` grant: the application authenticates itself
/// (no user login) and receives an app-only token, `user_id = NULL`,
/// acting as the application.
pub(super) async fn client_credentials_grant(
    state: &AppState,
    headers: &HeaderMap,
    params: &TokenParams,
) -> AppResult<TokenReply> {
    let app = validate_app(state, headers, params.body_credentials()).await?;
    let scopes = requested_scopes(params, &app)?;
    let token = issue_token(state, None, &app, scopes).await?;
    Ok(TokenReply::Issued(Json(TokenResponse::new(token, None))))
}

/// The `authorization_code` grant: the code a completed
/// `/oauth/authorize` consent handed the client (Pleroma's
/// `token_exchange` clause for `grant_type=authorization_code`). The
/// application authenticates exactly like on the other grants; the code
/// is redeemed by [`consume_authorization`], whose one statement takes
/// only an unused, unexpired row of *this* application — and, when the row
/// stores a redirect URI, only with that same `redirect_uri` (RFC 6749
/// §4.1.3) — and flips it used, so a replay, an expired code, another app's
/// code and a missing or different `redirect_uri` all answer the shared 400
/// [`INVALID_CREDENTIALS`](super::INVALID_CREDENTIALS). The pair carries the authorization's
/// approved scopes — not a `scope` the client now asks for — and acts as
/// the account that consented, which still has to exist and be ready to
/// log in: staff can deactivate or hold an account between the consent
/// and the exchange.
///
/// [`consume_authorization`]: oauth_token::consume_authorization
///
/// # Errors
///
/// 400 [`MISSING_CODE`] when the `code` field is absent, 400
/// [`INVALID_CREDENTIALS`](super::INVALID_CREDENTIALS) for a failed app authentication, a spent,
/// lapsed or foreign code, or a vanished owner, and
/// [`check_account_ready`]'s 403 for an account that stopped being ready.
/// Database failures arrive as [`AppError::Database`].
pub(super) async fn authorization_code_grant(
    state: &AppState,
    headers: &HeaderMap,
    params: &TokenParams,
) -> AppResult<TokenReply> {
    let Some(code) = params.code.as_deref() else {
        return Err(AppError::BadRequest(MISSING_CODE.to_owned()));
    };
    let app = validate_app(state, headers, params.body_credentials()).await?;
    let authorized =
        oauth_token::consume_authorization(&state.db, code, app.id, params.redirect_uri.as_deref())
            .await?
            .ok_or_else(invalid_credentials)?;
    let owner = user::get_by_id(&state.db, authorized.user_id)
        .await?
        .ok_or_else(invalid_credentials)?;
    check_account_ready(&owner)?;
    let scopes = Scopes::parse(&authorized.scopes);
    let token = issue_token(state, Some(&owner), &app, scopes).await?;
    Ok(TokenReply::Issued(Json(TokenResponse::new(
        token,
        Some(&owner),
    ))))
}

/// 403 body message when the password grant meets an MFA-enabled
/// account (Pleroma's `mfa_response.json` `error` value).
pub(super) const MFA_REQUIRED: &str = "mfa_required";

/// The challenge types the 403 above advertises, comma-joined as
/// Pleroma's `MFA.supported_methods` renders them — `totp` is the only
/// method this server speaks.
pub(super) const SUPPORTED_CHALLENGE_TYPES: &str = "totp";

/// The 403 body of a refused password grant: Pleroma's
/// `MFAView.render("mfa_response.json")`, carrying the fresh challenge
/// token a client takes to `POST /oauth/mfa/challenge` (or the browser
/// to `/oauth/mfa`).
#[derive(Debug, Serialize)]
pub(super) struct MfaRequiredResponse {
    /// The machine-readable error, always [`MFA_REQUIRED`].
    pub(super) error: &'static str,
    /// The 43-character `mfa_token` of the pending challenge.
    pub(super) mfa_token: String,
    /// The challenge types this server accepts, [`SUPPORTED_CHALLENGE_TYPES`].
    pub(super) supported_challenge_types: &'static str,
}

/// The `refresh_token` grant: rotate a user-bound pair. The application
/// authenticates exactly like on `/oauth/revoke` (body fields or HTTP
/// Basic); the presented refresh token must name a row issued to that
/// same app, owned by an active account — every failure answers the
/// shared 400 [`INVALID_CREDENTIALS`](super::INVALID_CREDENTIALS). An account that is active but not
/// ready to log in — still awaiting approval, or still unconfirmed — gets
/// [`check_account_ready`]'s 403 with its identifier instead: rotating a
/// pair is a login, and the registration modes gate it like the password
/// grant. The replacement carries the same user and scopes and is minted
/// before the old row goes, so the old access and refresh tokens stop
/// working at the answer. No expiry is applied to the old row: refreshing
/// is precisely what a client does after its access token has lapsed.
pub(super) async fn refresh_token_grant(
    state: &AppState,
    headers: &HeaderMap,
    params: &TokenParams,
) -> AppResult<TokenReply> {
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
    check_account_ready(&owner)?;
    let fresh = issue_token(state, Some(&owner), &app, Scopes::parse(&old.scopes)).await?;
    oauth_token::delete_token(&state.db, old.id).await?;
    Ok(TokenReply::Issued(Json(TokenResponse::new(
        fresh,
        Some(&owner),
    ))))
}

/// The active user behind `token`: `None` when the row is app-only
/// (`user_id = NULL`, so it never had a refreshable pair), its user was
/// deleted out from under it, or the account is deactivated — a
/// suspended account cannot rotate its tokens. The readiness the
/// *registration modes* add is checked after this, by
/// [`check_account_ready`], so a pending account keeps the shared 400 for
/// a deactivated one and gets its own 403 identifier otherwise.
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::oauth::{
        GRANT_TYPE_AUTHORIZATION_CODE, GRANT_TYPE_REFRESH_TOKEN,
        test_support::{message_of, params_with_grant, test_state},
    };

    #[test]
    fn the_mfa_required_body_has_pleromas_challenge_shape() {
        let json = serde_json::to_value(MfaRequiredResponse {
            error: MFA_REQUIRED,
            mfa_token: "challenge-token".to_owned(),
            supported_challenge_types: SUPPORTED_CHALLENGE_TYPES,
        })
        .unwrap();
        assert_eq!(
            json,
            json!({
                "error": "mfa_required",
                "mfa_token": "challenge-token",
                "supported_challenge_types": "totp",
            }),
            "the three keys Pleroma's `mfa_response.json` renders"
        );
        assert_eq!(
            json.as_object().expect("an object").len(),
            3,
            "no token keys leak into the challenge body"
        );
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

    #[tokio::test]
    async fn a_code_grant_without_the_code_parameter_is_refused_by_name() {
        // Same order as the refresh grant: the parameter gate first, so
        // the lazy pool of `test_state` is never consulted.
        let err = authorization_code_grant(
            &test_state(),
            &HeaderMap::new(),
            &params_with_grant(Some(GRANT_TYPE_AUTHORIZATION_CODE)),
        )
        .await
        .expect_err("no code is no exchange");
        assert_eq!(
            message_of(&err),
            MISSING_CODE,
            "the refusal names the missing parameter even without credentials"
        );
    }
}
