//! Password reset over HTTP (`docs/ARCHITECTURE.md` decision 1): the
//! request endpoint, the token check and the completion call.
//!
//! The reference is Pleroma's `Pleroma.Web.OAuth.PasswordController`
//! (`lib/pleroma/web/o_auth/password_controller.ex`) and its
//! `Pleroma.PasswordResetToken` (`lib/pleroma/password_reset_token.ex`),
//! whose routes the ticket mirrors: `POST /auth/password` asks for a reset,
//! `GET /api/v1/pleroma/password_reset/{token}` checks the emailed token and
//! `POST /api/v1/pleroma/password_reset` completes the reset.
//!
//! The request endpoint answers 204 to every well-formed call, matched or
//! not: telling a caller whether an account exists is an account probe,
//! which is why Pleroma's `request/2` discards the result of
//! `Registration.password_reset` and renders `:no_content` either way. Only
//! a local, active account with an email gets a token
//! ([`find_password_reset_target`](user::find_password_reset_target)) and a
//! queued [`password_reset`](templates::password_reset) letter whose link
//! is [`frontend_link`] — `{FRONTEND_BASE_URL}/password_reset/{token}`, the
//! frontend's reset screen; this server renders no reset page. The endpoint
//! counts every call into the `password_reset` rate-limit bucket.
//!
//! The two `/api/v1/pleroma` routes always answer JSON, whatever the
//! request's `Accept`: the token check answers 200 `{"nickname"}` or 404
//! `{"error": "Invalid Token"}` — used, lapsed or never minted, the three
//! are deliberately not told apart — and the completion `POST` answers 204
//! or 422 `{"error"}`. Completion validates token, password length and
//! confirmation, then lands the new Argon2id hash, the token spend, the
//! deletion of the account's other tokens, the clearing of
//! `password_reset_pending` and the revocation of every one of the account's
//! OAuth tokens as one transaction ([`crate::models::password_reset`]).

use std::fmt;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use tracing::debug;
use uuid::Uuid;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    extract::ApiBody,
    mailer::{self, templates},
    models::{
        oauth_token, password_reset,
        user::{self, User},
    },
    state::AppState,
    util::password::hash_password,
};

/// The message of a token that names no live reset.
const INVALID_TOKEN: &str = "Invalid Token";
/// The message of a submitted password under `MIN_PASSWORD_LENGTH`.
const PASSWORD_TOO_SHORT: &str = "Password is too short";
/// The message of a submission whose confirmation differs from its
/// password.
const PASSWORD_MISMATCH: &str = "Passwords do not match";
/// The `error` body message a login of a pending-reset account is refused
/// with (see [`crate::api::oauth::check_account_ready`]).
pub(crate) const PASSWORD_RESET_REQUIRED: &str = "Password reset is required";
/// The machine-readable identifier that 403 carries beside the message.
pub(crate) const PASSWORD_RESET_REQUIRED_ID: &str = "password_reset_required";

/// The link a reset letter and the admin's reset endpoint hand out.
///
/// It is `{FRONTEND_BASE_URL}/password_reset/{token}`, the frontend's reset
/// screen (WBE-198 for the admin path), which reads the token with `GET
/// /api/v1/pleroma/password_reset/{token}` and completes it with `POST
/// /api/v1/pleroma/password_reset`.
#[must_use]
pub fn frontend_link(config: &Config, token: &str) -> String {
    format!("{}/password_reset/{token}", config.frontend_base_url)
}

/// The reset endpoints the frontend calls.
///
/// [`auth_router`] mounts `POST /auth/password` at the top level (Pleroma
/// routes it outside `/api`, `router.ex`'s `:pleroma_html` scope);
/// [`pleroma_router`] carries the two `/api/v1/pleroma/password_reset`
/// paths for [`crate::api::v1`] to merge.
pub fn auth_router(state: &AppState) -> Router<AppState> {
    Router::new().route(
        "/auth/password",
        post(request).layer(crate::rate_limit::rate_limit(state, "password_reset")),
    )
}

/// The `/api/v1/pleroma` half of the surface: the token page and the
/// completion form, relative to the `/api/v1` nest of [`crate::api::v1`].
pub fn pleroma_router() -> Router<AppState> {
    Router::new()
        .route("/pleroma/password_reset/{token}", get(reset_page))
        .route("/pleroma/password_reset", post(do_reset))
}

/// The body of `POST /auth/password`: `email` or `nickname`, either of
/// which may carry the identifier (Pleroma reads `params["email"] ||
/// params["nickname"]`). Every other key is accepted and ignored, and
/// both fields are optional — a call naming neither still answers 204.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ResetRequest {
    /// The account's email address, when the caller names it that way.
    email: Option<String>,
    /// The account's nickname, when the caller names it that way.
    nickname: Option<String>,
}

/// `POST /auth/password` — ask for a reset letter. A local, active account
/// matching `email` or `nickname` gets a fresh token and a queued email;
/// every caller gets the same 204 with an empty body, whatever the body
/// held.
async fn request(
    State(state): State<AppState>,
    ApiBody(params): ApiBody<ResetRequest>,
) -> AppResult<StatusCode> {
    let identifier = params
        .email
        .as_deref()
        .or(params.nickname.as_deref())
        .filter(|text| !text.is_empty());
    if let Some(identifier) = identifier
        && let Some(target) = user::find_password_reset_target(&state.db, identifier).await?
    {
        enqueue_reset_mail(&state, &target).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Mints `target`'s reset token and queues the letter carrying its link.
/// The address exists by construction ([`find_password_reset_target`]
/// excludes the accounts without one).
async fn enqueue_reset_mail(state: &AppState, target: &User) -> AppResult<()> {
    let Some(address) = target.email.as_deref() else {
        return Ok(());
    };
    let issued = password_reset::create_for_user(&state.db, target.id).await?;
    let link = frontend_link(&state.config, &issued.token);
    debug!(account = %target.nickname, "password reset: token issued and letter queued");
    mailer::deliver_later(
        state,
        templates::password_reset(&state.config, address, &link),
    )
    .await
}

/// `GET /api/v1/pleroma/password_reset/{token}` — the frontend's token check:
/// 200 `{"nickname": ...}` for a token that is unused and unexpired, 404
/// `{"error": "Invalid Token"}` otherwise, whatever the request's `Accept`.
async fn reset_page(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    match live_reset(&state, &token).await? {
        Some((_, target)) => Ok(Json(json!({"nickname": target.nickname}))),
        None => Err(AppError::NotFoundMessage(INVALID_TOKEN.to_owned())),
    }
}

/// One field of the completion form, in either of the shapes the endpoint
/// accepts: nested under `data` (what a Pleroma form posts as
/// `data[password]` &c.) or flat.
#[derive(Default, Deserialize)]
#[serde(default)]
struct ResetFields {
    /// The token text, hidden input of the form.
    token: String,
    /// The new password.
    password: String,
    /// The repeated new password.
    password_confirmation: String,
}

/// `password` and `password_confirmation` are secret material and render
/// as `<redacted>`; the token shows (it is spent or kept either way, and
/// the page echoed it in the form).
impl fmt::Debug for ResetFields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResetFields")
            .field("token", &self.token)
            .field("password", &"<redacted>")
            .field("password_confirmation", &"<redacted>")
            .finish()
    }
}

/// The body of `POST /api/v1/pleroma/password_reset`: the `data[token]`/
/// `data[password]`/`data[password_confirmation]` fields Pleroma's form
/// posts, with the flat `token`/`password`/`password_confirmation` spellings
/// accepted beside them (per-field, the nested value wins when non-empty —
/// an empty password is refused for length either way).
#[derive(Default, Deserialize)]
#[serde(default)]
struct ResetSubmission {
    /// The `data[...]` object of a Pleroma form submission.
    data: ResetFields,
    /// Flat counterparts, for clients that post the fields bare.
    token: String,
    /// See [`ResetFields::password`].
    password: String,
    /// See [`ResetFields::password_confirmation`].
    password_confirmation: String,
}

/// `data` is shown; the flat `password` and `password_confirmation` render
/// redacted like their nested counterparts. The flat `token` shows.
impl fmt::Debug for ResetSubmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResetSubmission")
            .field("data", &self.data)
            .field("token", &self.token)
            .field("password", &"<redacted>")
            .field("password_confirmation", &"<redacted>")
            .finish()
    }
}

impl ResetSubmission {
    /// The submitted token: the nested field when non-empty, else the flat
    /// one.
    fn token(&self) -> &str {
        nested_or(&self.data.token, &self.token)
    }

    /// The submitted password, in the same precedence.
    fn password(&self) -> &str {
        nested_or(&self.data.password, &self.password)
    }

    /// The submitted confirmation, in the same precedence.
    fn password_confirmation(&self) -> &str {
        nested_or(
            &self.data.password_confirmation,
            &self.password_confirmation,
        )
    }
}

/// `nested` when it carries anything, `flat` otherwise.
const fn nested_or<'a>(nested: &'a str, flat: &'a str) -> &'a str {
    if nested.is_empty() { flat } else { nested }
}

/// `POST /api/v1/pleroma/password_reset` — complete the reset: 204 on
/// success, 422 `{"error": message}` on any refusal, whatever the request's
/// `Accept` or body type (JSON or form).
async fn do_reset(
    State(state): State<AppState>,
    ApiBody(submission): ApiBody<ResetSubmission>,
) -> AppResult<StatusCode> {
    let outcome = reset_attempt(
        &state,
        submission.token(),
        submission.password(),
        submission.password_confirmation(),
    )
    .await?;
    match outcome {
        ResetOutcome::Changed => Ok(StatusCode::NO_CONTENT),
        ResetOutcome::Refused(message) => Err(AppError::Unprocessable(message.to_owned())),
    }
}

/// How a completion attempt ended, before it is rendered as JSON.
#[derive(Debug, PartialEq, Eq)]
enum ResetOutcome {
    /// The password changed and the token was spent.
    Changed,
    /// Nothing changed; the message says why.
    Refused(&'static str),
}

/// The completion, in order: the token (unused, unexpired, and naming a
/// living account), the password length, the confirmation — then the
/// transaction that changes the password and burns every credential the
/// old one opened.
async fn reset_attempt(
    state: &AppState,
    token: &str,
    password: &str,
    confirmation: &str,
) -> AppResult<ResetOutcome> {
    let Some((record, target)) = live_reset(state, token).await? else {
        return Ok(ResetOutcome::Refused(INVALID_TOKEN));
    };
    if password.chars().count() < state.config.min_password_length {
        return Ok(ResetOutcome::Refused(PASSWORD_TOO_SHORT));
    }
    if password != confirmation {
        return Ok(ResetOutcome::Refused(PASSWORD_MISMATCH));
    }
    let new_hash = hash_password(password)?;
    if !complete_reset(state, &target, record.id, &new_hash).await? {
        return Ok(ResetOutcome::Refused(INVALID_TOKEN));
    }
    debug!(account = %target.nickname, "password reset: password changed");
    Ok(ResetOutcome::Changed)
}

/// The token's row and its account when the token is unused, unexpired and
/// names a living account.
async fn live_reset(
    state: &AppState,
    token: &str,
) -> AppResult<Option<(password_reset::PasswordResetToken, User)>> {
    let Some(record) =
        password_reset::get_valid(&state.db, token, state.config.password_reset_ttl()).await?
    else {
        return Ok(None);
    };
    let target = user::get_by_id(&state.db, record.user_id).await?;
    Ok(target.map(|target| (record, target)))
}

/// The write half, as one transaction: the token's spend, the new hash
/// (which also clears `password_reset_pending`), the account's other
/// tokens, and every OAuth token the account holds. A failure anywhere
/// lands none of them.
///
/// The spend goes first and is conditional: when a concurrent completion
/// already spent the token it answers `false`, the transaction rolls back
/// with nothing written, and so does this function (`false`: the token is
/// no longer valid). `true` means the reset landed.
async fn complete_reset(
    state: &AppState,
    target: &User,
    token_id: Uuid,
    hash: &str,
) -> AppResult<bool> {
    let mut tx = state.db.begin().await?;
    if !password_reset::mark_used(&mut *tx, token_id).await? {
        tx.rollback().await?;
        return Ok(false);
    }
    user::set_password(&mut *tx, target.id, hash).await?;
    let others = password_reset::delete_others(&mut *tx, target.id, token_id).await?;
    let revoked = oauth_token::delete_all_for_user(&mut *tx, target.id).await?;
    tx.commit().await?;
    debug!(account = %target.nickname, others, revoked, "password reset: tokens revoked");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    #[test]
    fn request_body_reads_email_and_nickname() {
        let params: ResetRequest = serde_json::from_value(json!({
            "email": "a@b.example",
            "nickname": "ignored",
        }))
        .unwrap();
        assert_eq!(params.email.as_deref(), Some("a@b.example"), "email");
        assert_eq!(params.nickname.as_deref(), Some("ignored"), "nickname");
    }

    #[test]
    fn request_body_may_name_neither_field() {
        let params: ResetRequest = serde_json::from_value(json!({})).unwrap();
        assert_eq!(params.email, None, "no email");
        assert_eq!(params.nickname, None, "no nickname");
    }

    #[test]
    fn submission_prefers_the_nested_data_fields() {
        let sub: ResetSubmission = serde_json::from_value(json!({
            "data": {
                "token": "nested-token",
                "password": "nested-pw",
                "password_confirmation": "nested-pw",
            },
        }))
        .unwrap();
        assert_eq!(sub.token(), "nested-token", "data[token]");
        assert_eq!(sub.password(), "nested-pw", "data[password]");
        assert_eq!(
            sub.password_confirmation(),
            "nested-pw",
            "data[password_confirmation]"
        );
    }

    #[test]
    fn submission_falls_back_to_the_flat_fields() {
        let sub: ResetSubmission = serde_json::from_value(json!({
            "token": "flat-token",
            "password": "flat-pw",
            "password_confirmation": "flat-pw",
        }))
        .unwrap();
        assert_eq!(sub.token(), "flat-token", "flat token");
        assert_eq!(sub.password(), "flat-pw", "flat password");
        assert_eq!(sub.password_confirmation(), "flat-pw", "flat confirmation");
    }

    #[test]
    fn submission_treats_an_empty_body_as_empty_fields() {
        let sub: ResetSubmission = serde_json::from_value(json!({})).unwrap();
        assert_eq!(sub.token(), "", "nothing nested, nothing flat");
        assert_eq!(sub.password(), "", "empty password");
    }

    #[test]
    fn submission_debug_redacts_the_passwords() {
        let sub: ResetSubmission = serde_json::from_value(json!({
            "password": "hunter2secret",
            "data": {"password_confirmation": "hunter2secret"},
        }))
        .unwrap();
        let debug = format!("{sub:?}");
        assert!(
            !debug.contains("hunter2secret"),
            "a password must not leak into Debug output: {debug}"
        );
        assert!(
            debug.contains(r#"password: "<redacted>""#),
            "the marker names the field: {debug}"
        );
    }

    #[test]
    fn nested_or_prefers_a_non_empty_nested_value() {
        assert_eq!(nested_or("a", "b"), "a", "nested wins");
        assert_eq!(nested_or("", "b"), "b", "empty falls back");
        assert_eq!(nested_or("", ""), "", "both empty stays empty");
    }

    #[test]
    fn frontend_link_hangs_the_token_off_the_frontend_base_url() {
        let mut config = Config::for_tests();
        config.base_url = "https://api.example".to_owned();
        config.frontend_base_url = "https://fe.example".to_owned();
        assert_eq!(
            frontend_link(&config, "tok3n"),
            "https://fe.example/password_reset/tok3n",
            "the frontend origin, not the API's"
        );
    }

    /// The stored password hash of the account `id`.
    async fn stored_hash(state: &AppState, id: Uuid) -> Option<String> {
        user::get_by_id(&state.db, id)
            .await
            .expect("the account reads")
            .expect("the account exists")
            .password_hash
    }

    #[tokio::test]
    async fn a_second_reset_with_a_spent_token_is_refused() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let ap_id = user::local_ap_id(&state.config.base_url, "replayed");
        let target = user::insert(&state.db, &user::NewUser::local("replayed", &ap_id))
            .await
            .expect("the account fixture inserts");
        let record = password_reset::create_for_user(&state.db, target.id)
            .await
            .expect("the token mints");

        let first = reset_attempt(
            &state,
            &record.token,
            "first-new-password",
            "first-new-password",
        )
        .await
        .expect("the first completion runs");
        assert_eq!(first, ResetOutcome::Changed, "the first reset lands");
        let landed = stored_hash(&state, target.id).await;
        assert!(landed.is_some(), "the first reset set a password");

        let second = reset_attempt(
            &state,
            &record.token,
            "second-new-password",
            "second-new-password",
        )
        .await
        .expect("the replay runs");
        assert_eq!(second, ResetOutcome::Refused(INVALID_TOKEN), "the replay");
        assert_eq!(
            stored_hash(&state, target.id).await,
            landed,
            "the replay left the password unchanged"
        );
    }

    #[tokio::test]
    async fn complete_reset_with_a_token_spent_underneath_it_rolls_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let ap_id = user::local_ap_id(&state.config.base_url, "raced");
        let target = user::insert(&state.db, &user::NewUser::local("raced", &ap_id))
            .await
            .expect("the account fixture inserts");
        let record = password_reset::create_for_user(&state.db, target.id)
            .await
            .expect("the token mints");
        let sibling = password_reset::create_for_user(&state.db, target.id)
            .await
            .expect("the sibling token mints");
        let before = stored_hash(&state, target.id).await;

        // The rival completion lands its spend after this one's lookup.
        assert!(
            password_reset::mark_used(&state.db, record.row.id)
                .await
                .expect("the rival spend runs"),
            "the rival wins the spend"
        );
        let landed = complete_reset(&state, &target, record.row.id, "$argon2id$loser")
            .await
            .expect("the losing completion runs");
        assert!(!landed, "the loser is told the token is spent");
        assert_eq!(
            stored_hash(&state, target.id).await,
            before,
            "the loser's password did not land"
        );
        assert!(
            password_reset::get_valid(&state.db, &sibling.token, Duration::from_hours(1))
                .await
                .expect("the lookup runs")
                .is_some(),
            "the loser's rollback kept the account's other tokens"
        );
    }
}
