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
use sqlx::PgConnection;

use super::{
    MISSING_CODE, MISSING_REFRESH_TOKEN, TokenReply,
    issue::{TokenResponse, authenticate_user, check_account_ready, issue_token, issue_token_on},
    mfa,
    params::{TokenParams, invalid_credentials, requested_scopes, validate_app},
};
use crate::{
    error::{AppError, AppResult},
    models::{
        oauth_app::OauthApp,
        oauth_token::{self, OauthToken, REFRESH_GRACE},
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
    let app = validate_app(state, headers, params.client.pair()).await?;
    let scopes = requested_scopes(params, &app)?;
    let user = authenticate_user(&state.db, params).await?;
    check_account_ready(&user)?;
    if mfa::required(&user) {
        let challenge = mfa::start_challenge(state, &user, &app, &scopes).await?;
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
    let app = validate_app(state, headers, params.client.pair()).await?;
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
/// §4.1.3) — and, when the row is bound to a PKCE challenge, only with the
/// `code_verifier` that hashes to it (RFC 7636) — and flips it used, so a
/// replay, an expired code, another app's code, a missing or different
/// `redirect_uri` and a missing or wrong `code_verifier` all answer the shared 400
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
    let app = validate_app(state, headers, params.client.pair()).await?;
    let authorized = oauth_token::consume_authorization_with_verifier(
        &state.db,
        code,
        app.id,
        params.redirect_uri.as_deref(),
        params.code_verifier.as_deref(),
    )
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
/// token a client takes to `POST /oauth/mfa/challenge`.
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
/// in the transaction that deletes the old row, so the old access and
/// refresh tokens stop working at the answer.
///
/// A refresh token works exactly once, and only within [`REFRESH_GRACE`] of
/// the pair's `valid_until` (refreshing is precisely what a client does after
/// its access token has lapsed, but not forever): one transaction deletes the
/// old row with [`consume_refresh_token`], checks the owner and mints the
/// replacement, so two concurrent requests with one token cannot both get a
/// pair. A refusal — unknown, foreign or lapsed token, deactivated owner, or
/// an account not ready to log in — rolls the transaction back, so the old
/// pair survives it.
///
/// [`consume_refresh_token`]: oauth_token::consume_refresh_token
pub(super) async fn refresh_token_grant(
    state: &AppState,
    headers: &HeaderMap,
    params: &TokenParams,
) -> AppResult<TokenReply> {
    let Some(refresh_token) = params.refresh_token.as_deref() else {
        return Err(AppError::BadRequest(MISSING_REFRESH_TOKEN.to_owned()));
    };
    let app = validate_app(state, headers, params.client.pair()).await?;
    let mut tx = state.db.begin().await?;
    match rotate_pair(state, &mut tx, &app, refresh_token).await {
        Ok((fresh, owner)) => {
            tx.commit().await?;
            Ok(TokenReply::Issued(Json(TokenResponse::new(
                fresh,
                Some(&owner),
            ))))
        }
        Err(refusal) => {
            tx.rollback().await?;
            Err(refusal)
        }
    }
}

/// The body of [`refresh_token_grant`] on its transaction's connection:
/// consumes the refresh token of `app`, checks the owner and mints the pair
/// that replaces the consumed one. Nothing is durable until the caller
/// commits, so any error here leaves the old pair in place once it rolls
/// back.
///
/// # Errors
///
/// 400 [`INVALID_CREDENTIALS`](super::INVALID_CREDENTIALS) for an unknown,
/// foreign or lapsed refresh token and for an owner that is gone or
/// deactivated, [`check_account_ready`]'s 403 for an account that is not
/// ready to log in, and [`AppError::Database`] for a database failure.
async fn rotate_pair(
    state: &AppState,
    conn: &mut PgConnection,
    app: &OauthApp,
    refresh_token: &str,
) -> AppResult<(OauthToken, User)> {
    let old = oauth_token::consume_refresh_token(&mut *conn, refresh_token, app.id, REFRESH_GRACE)
        .await?
        .ok_or_else(invalid_credentials)?;
    let owner = active_owner(&mut *conn, &old)
        .await?
        .ok_or_else(invalid_credentials)?;
    check_account_ready(&owner)?;
    let fresh = issue_token_on(
        &mut *conn,
        state,
        Some(&owner),
        app,
        Scopes::parse(&old.scopes),
    )
    .await?;
    Ok((fresh, owner))
}

/// The active user behind `token`, read on the caller's connection so the
/// refresh transaction needs no second one: `None` when the row is app-only
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
async fn active_owner(conn: &mut PgConnection, token: &OauthToken) -> AppResult<Option<User>> {
    let Some(user_id) = token.user_id else {
        return Ok(None);
    };
    let owner = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(conn)
        .await?;
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
