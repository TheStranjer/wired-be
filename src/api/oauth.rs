//! `POST /oauth/token`: the OAuth 2.0 `password`, `client_credentials`,
//! `refresh_token` and `authorization_code` grants (Pleroma's
//! `OAuthController.token_exchange`,
//! `lib/pleroma/web/o_auth/o_auth_controller.ex`).
//!
//! The `authorization_code` grant redeems the `?code=` a browser got from
//! `POST /oauth/authorize`: the application authenticates as on every
//! other grant, the code must name one of *its own* unused, unlapsed
//! authorizations — the redemption is one statement, so a replay is
//! refused and a foreign app cannot burn the code — and the pair it
//! answers acts as the consenting account with exactly the scopes that
//! authorization approved (Pleroma's `Token.exchange_token`).
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
//! all answer the same 400 "Invalid credentials" (Pleroma's way); an
//! account that is not ready to log in instead gets its distinct 403 from
//! `check_account_ready`, with the machine-readable identifier Pleroma
//! names the refusal by: `account_is_disabled` for a deactivated account,
//! `awaiting_approval` while staff have not approved it
//! (`REGISTRATIONS_APPROVAL_REQUIRED`), `missing_confirmed_email` while
//! its address is not (`ACCOUNT_ACTIVATION_REQUIRED`), and
//! `password_reset_required` for an account on a forced password reset
//! (see [`crate::api::password_reset`]). An MFA-enabled account gets its own
//! 403 beside all of these (WBE-132, Pleroma's
//! `handle_token_exchange_error({:mfa_required, user, auth, _})`): no
//! token, but the `mfa_required` body carrying a five-minute
//! `mfa_token` whose [`challenge`](mfa) endpoint then runs the second
//! factor and answers the very token JSON.
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
//! /oauth/authorize` — lives in [`authorize`]; the second factor both
//! login halves defer to lives in [`mfa`].

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};

use crate::{error::AppResult, extract::ApiBody, state::AppState};

use self::{
    grants::{
        MfaRequiredResponse, authorization_code_grant, client_credentials_grant, password_grant,
        refresh_token_grant,
    },
    issue::{TokenResponse, issue_token},
    params::{TokenParams, require_grant, validate_app},
    revoke::{no_cache_headers, revoke},
};

pub(crate) use self::issue::check_account_ready;

pub mod authorize;
mod grants;
pub mod issue;
pub mod json_media;
pub mod mfa;
mod params;
mod revoke;
#[cfg(test)]
mod test_support;

/// The password grant type: account credentials in exchange for a
/// user-bound token pair.
const GRANT_TYPE_PASSWORD: &str = "password";
/// The client-credentials grant type: the application authenticates
/// itself and receives an app-only token.
const GRANT_TYPE_CLIENT_CREDENTIALS: &str = "client_credentials";
/// The refresh-token grant type: a live refresh token is exchanged for a
/// fresh pair with the same user and scopes, the old row deleted.
const GRANT_TYPE_REFRESH_TOKEN: &str = "refresh_token";
/// The authorization-code grant type: the `?code=` of a completed
/// `/oauth/authorize` consent is exchanged for a pair acting as the
/// consenting account.
const GRANT_TYPE_AUTHORIZATION_CODE: &str = "authorization_code";
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
/// 400 body when the authorization-code grant omits the `code`
/// parameter, spelled like the refresh grant's refusal of its own
/// credential.
const MISSING_CODE: &str = "Missing parameter: code";
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

/// The `/oauth` router, merged at the top level of [`crate::app`].
///
/// The paths are fixed by OAuth (`/oauth/token`, `/oauth/revoke`,
/// `/oauth/authorize`, `/oauth/mfa/challenge` and the rest in later
/// tickets), not nested under `/api`.
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
        .merge(mfa::router(state))
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
        Ok(reply) => reply.into_response(),
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
    /// `grant_type=authorization_code`: an unused authorization code from
    /// `/oauth/authorize` plus client credentials, answered with a pair
    /// acting as the account that granted the code.
    AuthorizationCode,
}

/// The token exchange, in order: the grant type, then per grant — the
/// client application, the requested scopes, (for the password grant)
/// the user's credentials, the account's readiness and the MFA gate —
/// and the answer rendered: the minted token pair, or the challenge a
/// refused login gets in its place.
async fn exchange(
    state: &AppState,
    headers: &HeaderMap,
    params: TokenParams,
) -> AppResult<TokenReply> {
    match require_grant(&params)? {
        Grant::Password => password_grant(state, headers, &params).await,
        Grant::ClientCredentials => client_credentials_grant(state, headers, &params).await,
        Grant::RefreshToken => refresh_token_grant(state, headers, &params).await,
        Grant::AuthorizationCode => authorization_code_grant(state, headers, &params).await,
    }
}

/// What a token exchange answers with when nothing failed: the minted
/// pair, or — the Pleroma `{:mfa_required, user, auth, _}` path — the
/// 403 challenge a password grant to an MFA-enabled account gets
/// instead of a token.
#[derive(Debug)]
enum TokenReply {
    /// 200: Pleroma's `token.json`.
    Issued(Json<TokenResponse>),
    /// 403: Pleroma's `mfa_response.json`.
    MfaRequired(Json<MfaRequiredResponse>),
}

impl IntoResponse for TokenReply {
    fn into_response(self) -> Response {
        match self {
            Self::Issued(body) => body.into_response(),
            Self::MfaRequired(body) => (StatusCode::FORBIDDEN, body).into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, Request, header::CACHE_CONTROL};
    use serde_json::json;

    use super::{
        grants::{MFA_REQUIRED, SUPPORTED_CHALLENGE_TYPES},
        test_support::token_row,
        *,
    };
    use crate::{test_support::lazy_state, util::id::new_id};

    #[test]
    fn the_mfa_challenge_answer_is_a_403() {
        let reply = TokenReply::MfaRequired(Json(MfaRequiredResponse {
            error: MFA_REQUIRED,
            mfa_token: "challenge-token".to_owned(),
            supported_challenge_types: SUPPORTED_CHALLENGE_TYPES,
        }));
        let response = reply.into_response();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "Pleroma's forbidden challenge answer"
        );
    }

    #[test]
    fn an_issued_answer_still_carries_200() {
        let reply = TokenReply::Issued(Json(TokenResponse::new(token_row(new_id(), "read"), None)));
        let response = reply.into_response();
        assert_eq!(response.status(), StatusCode::OK, "the grant succeeded");
    }

    #[tokio::test]
    async fn the_route_answers_at_oauth_token() {
        use tower::ServiceExt as _;
        let state = lazy_state();
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
        let state = lazy_state();
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
