//! Registration's request flow: the `POST /api/v1/accounts` handler and
//! the email-confirmation link. The request parameters and the answer
//! types are [`registration`]'s, and [`load_invite`] is the invite gate
//! the registration modes weigh.
//!
//! [`registration`]: super::registration
//!
//! `POST /api/v1/accounts` is open-mode registration: Pleroma's
//! `AccountController.create` (`lib/pleroma/web/mastodon_api/controllers/
//! account_controller.ex`) over `Registration.register_user`
//! (`lib/pleroma/web/registration.ex`). Like Pleroma's `OAuthPlug` the
//! route authenticates with [`AppAuth`] — any valid bearer token,
//! app-only (`client_credentials`) or user-bound — plus the
//! `write:accounts` scope, and a caller who already holds a user token
//! may register another account (Pleroma permits it). The gate behind
//! that: while `REGISTRATIONS_OPEN` is true the route registers, and the
//! body's invite `token` — if it carries one — is optional and ignored,
//! never checked and never spent. While it is false and `INVITES_ENABLED`
//! is true a valid invite `token` is REQUIRED: the unknown, spent,
//! expired or exhausted token (and the absent one) is the 400
//! `{"error":"Invalid token"}`, and the accepted token is consumed in
//! the same transaction that creates the user (see
//! [`crate::models::invite_token`]). An invited user skips approval — the
//! mode leaves its row active — but not email confirmation. While registrations are closed
//! and invites are disabled the route answers the 403 `{"error":
//! "Registration is disabled"}`; Pleroma's 2.10 `create` action carries
//! no such string (closed registrations there mean "return with an invite
//! token"), so this endpoint's ticket pins the wording. The body is
//! `username`, `email`, `password`, `agreement` (which must be `true`,
//! else 422), `locale`, `reason`, `fullname` (the display name), `bio`
//! and the invite `token`; `locale` and `confirm` are accepted and
//! ignored, as Pleroma reads neither on this path. The user lands through
//! [`create_local_user`]
//! — every validation failure arrives as its 422 whose `error` value is
//! the JSON string of the field map — and `reason` is stored in
//! `users.registration_reason` for the approval flow.
//!
//! What a success then answers is decided by the two registration modes,
//! read back off the finished row: with
//! `REGISTRATIONS_APPROVAL_REQUIRED` the account awaits staff approval and
//! the answer is 200 `REGISTERED_AWAITING_APPROVAL` beside the
//! `awaiting_approval` identifier; with `ACCOUNT_ACTIVATION_REQUIRED` the
//! account is unconfirmed, its letter is queued, and the answer is 200
//! `REGISTERED_AWAITING_CONFIRMATION` beside `missing_confirmed_email`.
//! Both at once reports the approval, the wait that ends last. Neither —
//! the default — mints a fresh user token for the issuing app carrying the
//! presenting token's scopes (Pleroma's
//! `OAuthController.login(user, app, …)`) and answers the token JSON:
//! `token_type`, `access_token`, `scope`, `created_at` (unix seconds), the
//! token row's `id`, `expires_in` and `refresh_token`. A pending
//! registration gets no credentials at all: minting a token for an account
//! the login gate refuses would only hand out a credential that cannot be
//! used.
//!
//! `GET /api/account/confirm_email/{user_id}/{token}` is the link that
//! confirmation letter carries: [`email_confirmation_router`], which
//! [`crate::api`] mounts outside the versioned API, answers it in
//! `confirm_email`.

use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header::LOCATION},
    response::{IntoResponse as _, Response},
};
use chrono::Utc;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::oauth::issue::{TokenResponse, issue_token},
    auth::{AppAuth, require_scope},
    config::Config,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        invite_token::{self, InviteToken},
        user::{confirmation, register::create_local_user},
    },
    oauth::scopes::{Scopes, WRITE_ACCOUNTS},
    state::AppState,
};

use super::registration::{CreateAccountParams, RegistrationAnswer, RegistrationMessage};

/// The 403 body message `POST /api/v1/accounts` answers while
/// `REGISTRATIONS_OPEN` is false and `INVITES_ENABLED` is false. Pleroma's
/// 2.10 `create` action has no such string (closed registrations there route
/// to invite tokens), so this endpoint's ticket pins this wording.
const REGISTRATIONS_CLOSED: &str = "Registration is disabled";
/// The 400 body message `POST /api/v1/accounts` answers while
/// `REGISTRATIONS_OPEN` is false and `INVITES_ENABLED` is true and the
/// submitted invite `token` is absent, unknown, spent, expired or
/// exhausted (Pleroma's `Registration.create_user_with_invite` wording;
/// this endpoint's ticket pins the one message for every refusal).
const INVALID_TOKEN: &str = invite_token::INVALID_INVITE_MESSAGE;

/// The 422 body message `POST /api/v1/accounts` answers when `agreement`
/// is present but not true (Mastodon's signup terms wording; Pleroma's
/// schema requires only that the field is there).
const AGREEMENT_REQUIRED: &str = "You have to accept the terms to sign up";

/// `POST /api/v1/accounts` — register a local account and receive a
/// bearer token for it (Pleroma's `.create`, which ends in
/// `OAuthController.login(user, app, …)` + the token view), or the message
/// saying what the new account first has to wait for.
///
/// The gates run in Pleroma's plug order: [`AppAuth`] first rejects a
/// missing or invalid credential (401) — an app-only `client_credentials`
/// token satisfies it exactly as a user token does — then the
/// `write:accounts` scope check (403), then the registration/invite gate
/// (403 [`REGISTRATIONS_CLOSED`] or the invite check, [`load_invite`]),
/// then the agreement (422). The account itself is built by
/// [`create_local_user`], whose errors this endpoint's ticket maps as
/// implemented there: a 422 whose `error` value is the JSON string of the
/// field map; an invited registration spends its token inside that same
/// write's transaction.
///
/// A success then answers what the finished row says, through
/// [`RegistrationMessage::for_user`]: an account awaiting staff approval
/// gets 200 [`REGISTERED_AWAITING_APPROVAL`] and no token, an unconfirmed
/// one 200 [`REGISTERED_AWAITING_CONFIRMATION`] and no token, and both at
/// once the approval message. Only a ready account is issued credentials —
/// [`issue_token`] mints them for the presenting token's app with the
/// presenting token's scopes, valid for `OAUTH_TOKEN_TTL_SECS`, and the
/// answer is the `/oauth/token` body without its `me` key.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AppAuth`] without a valid token;
/// [`AppError::Forbidden`] from [`require_scope`] without `write:accounts`
/// or while registrations are closed without invites;
/// [`AppError::BadRequest`] from [`load_invite`] for a refused invite;
/// [`AppError::Unprocessable`] when `agreement` is absent (the
/// extractor's) or not true, and [`create_local_user`]'s field-map 422s
/// for every invalid parameter. Any database or key-generation failure
/// arrives through `?` as [`AppError::Database`] or [`AppError::Internal`].
pub(super) async fn create_account(
    State(state): State<AppState>,
    AppAuth(app, token): AppAuth,
    ApiBody(params): ApiBody<CreateAccountParams>,
) -> AppResult<Json<RegistrationAnswer>> {
    require_scope(&token, WRITE_ACCOUNTS)?;
    let invite = load_invite(&state, params.token.as_deref()).await?;
    if !params.agreement {
        return Err(AppError::Unprocessable(AGREEMENT_REQUIRED.to_owned()));
    }
    let user = create_local_user(&state, params.into_register_params(), invite.as_ref()).await?;
    let answer = if let Some(message) = RegistrationMessage::for_user(&user) {
        RegistrationAnswer::Pending(message)
    } else {
        // The presenting token's scopes, carried over as granted.
        let scopes = Scopes::parse(&token.scopes);
        let issued = issue_token(&state, Some(&user), &app, scopes).await?;
        RegistrationAnswer::Ready(TokenResponse::new(issued, Some(&user)).without_me())
    };
    debug!(
        user = %user.id,
        app = %app.id,
        invited = invite.is_some(),
        tokened = matches!(answer, RegistrationAnswer::Ready(_)),
        "registered a new local account"
    );
    Ok(Json(answer))
}

/// `GET /api/account/confirm_email/{user_id}/{token}` — the link a
/// confirmation email carries (Pleroma's `TokenController.confirm_email`,
/// `lib/pleroma/web/o_auth/token_controller.ex`): clicking it confirms the
/// address of the unconfirmed local account whose id and token the link
/// names, clearing the token and the send stamp with it, and nothing else.
///
/// The route is open — no token, no scope — because it is what a registrant
/// who cannot log in yet has to click. Pleroma's answer is `redirect(conn,
/// to: "/")` on the success path and its `errors` fallback on every other;
/// this API-only server serves no page at `/` (decision 1), so the ticket
/// pins one answer for both: 302 with `Location: {FRONTEND_BASE_URL}/`
/// ([`Config::frontend_base_url`]). The row changes only when [`confirm`] matches — a malformed id, an unknown
/// account, an already-confirmed address or a stale token all redirect
/// without confirming anything, so the reply leaks nothing about which of
/// them arrived.
///
/// [`confirm`]: crate::models::user::confirmation::confirm
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn confirm_email(
    State(state): State<AppState>,
    Path((user_id, token)): Path<(String, String)>,
) -> AppResult<Response> {
    let confirmed = match Uuid::parse_str(&user_id) {
        Ok(id) => confirmation::confirm(&state.db, id, &token).await?,
        Err(_unparsable) => false,
    };
    debug!(account = %user_id, confirmed, "visited an email-confirmation link");
    Ok(redirect_home(&state.config))
}

/// The one answer both halves of the confirmation flow give: a 302 to the
/// root of [`Config::frontend_base_url`], the page the frontend is served on
/// (this server serves nothing at its own `/`).
///
/// Assembled by hand rather than through axum's `Redirect`, because
/// `Redirect::to` answers 303 `See Other` and Pleroma's
/// `redirect(conn, to: "/")` — which this ticket pins — is a 302.
///
/// [`Config::frontend_base_url`]: Config::frontend_base_url
fn redirect_home(config: &Config) -> Response {
    (
        StatusCode::FOUND,
        [(LOCATION, format!("{}/", config.frontend_base_url))],
    )
        .into_response()
}

/// The invite the registration gate hands the user-creation write, or the
/// refusal it owes: with `REGISTRATIONS_OPEN` the invite path is off and
/// the submitted `token` is optional and ignored — `Ok(None)`, whatever
/// it holds, and it is never checked or spent; with registrations closed
/// and `INVITES_ENABLED` off nobody registers — 403
/// [`REGISTRATIONS_CLOSED`]; with registrations closed and invites on a
/// valid invite is REQUIRED — the absent or blank text, an unknown token,
/// and a row [`is_valid`] denies on today's date (spent, expired,
/// exhausted) all answer the 400 [`INVALID_TOKEN`], this endpoint's
/// ticket pinning one message for every refusal.
///
/// [`is_valid`]: InviteToken::is_valid
///
/// # Errors
///
/// [`AppError::Forbidden`] while closed without invites;
/// [`AppError::BadRequest`] for every refused invite; any database
/// failure arrives through `?` as [`AppError::Database`].
async fn load_invite(state: &AppState, submitted: Option<&str>) -> AppResult<Option<InviteToken>> {
    if state.config.registrations_open.get() {
        return Ok(None);
    }
    if !state.config.invites_enabled.get() {
        return Err(AppError::Forbidden(REGISTRATIONS_CLOSED.to_owned()));
    }
    let invalid = || AppError::BadRequest(INVALID_TOKEN.to_owned());
    let Some(text) = submitted.map(str::trim).filter(|text| !text.is_empty()) else {
        return Err(invalid());
    };
    let Some(invite) = invite_token::get_by_token(&state.db, text).await? else {
        return Err(invalid());
    };
    if !invite.is_valid(Utc::now().date_naive()) {
        return Err(invalid());
    }
    Ok(Some(invite))
}
