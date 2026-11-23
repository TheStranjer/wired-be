//! Password reset over HTTP (`docs/ARCHITECTURE.md` decision 1): the
//! request endpoint, the emailed token's page and the completion form.
//!
//! The reference is Pleroma's `Pleroma.Web.OAuth.PasswordController`
//! (`lib/pleroma/web/o_auth/password_controller.ex`) and its
//! `Pleroma.PasswordResetToken` (`lib/pleroma/password_reset_token.ex`),
//! whose routes the ticket mirrors: `POST /auth/password` asks for a reset,
//! `GET /api/v1/pleroma/password_reset/{token}` serves the emailed link's
//! page, and `POST /api/v1/pleroma/password_reset` completes the reset.
//!
//! The request endpoint answers 204 to every well-formed call, matched or
//! not: telling a caller whether an account exists is an account probe,
//! which is why Pleroma's `request/2` discards the result of
//! `Registration.password_reset` and renders `:no_content` either way. Only
//! a local, active account with an email gets a token
//! ([`find_password_reset_target`](user::find_password_reset_target)) and a
//! queued [`password_reset`](templates::password_reset) letter whose link
//! is `{BASE_URL}/api/v1/pleroma/password_reset/{token}`; the endpoint
//! counts every call into the `password_reset` rate-limit bucket.
//!
//! The two `/api/v1/pleroma` pages are HTML, not JSON (decision 8 is about
//! API errors; these are browser-facing pages like `/oauth/authorize`): the
//! token page renders the form when the token is valid and an
//! "Invalid Token" page when it is not — used, lapsed or never minted, the
//! three are deliberately not told apart. Completion validates token,
//! password length and confirmation, then lands the new Argon2id hash, the
//! token spend, the deletion of the account's other tokens, the clearing
//! of `password_reset_pending` and the revocation of every one of the
//! account's OAuth tokens as one transaction
//! ([`crate::models::password_reset`]), and answers the
//! "Password changed!" page; any failed validation re-renders the form
//! with the message instead.
//!
//! Every dynamic value on the pages goes through
//! [`escape_html`]: the nickname and the round-tripped token are
//! client-influenced text and must never break out of their attribute or
//! element.

use std::{fmt, time::Duration};

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    error::AppResult,
    extract::ApiBody,
    mailer::{self, templates},
    models::{
        oauth_token, password_reset,
        user::{self, User},
    },
    state::AppState,
    util::{password::hash_password, sanitize::escape_html},
};

/// The path the emailed link names and the reset form posts to, under the
/// `/api/v1` nest [`crate::api::v1`] mounts it on. Crate-visible because
/// the admin's reset endpoint hands out the same link (WBE-198): the token
/// it mints must complete through this flow unchanged.
pub(crate) const RESET_PATH: &str = "/api/v1/pleroma/password_reset";
/// The heading and page of a token that names no live reset.
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

/// The reset endpoints a browser calls.
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
    let token = password_reset::create_for_user(&state.db, target.id).await?;
    let link = format!("{RESET_PATH}/{}", token.token);
    debug!(account = %target.nickname, "password reset: token issued and letter queued");
    mailer::deliver_later(
        state,
        templates::password_reset(&state.config, address, &link),
    )
    .await
}

/// `GET /api/v1/pleroma/password_reset/{token}` — the page the emailed link
/// opens: the reset form when the token is unused and unexpired, the
/// "Invalid Token" page otherwise. Both answers are 200, like Pleroma's
/// `reset/2`, which renders `invalid_token.html` for every failed match.
async fn reset_page(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> AppResult<Html<String>> {
    let page = match password_reset::get_valid(&state.db, &token, token_ttl(&state)).await? {
        Some(record) => match user::get_by_id(&state.db, record.user_id).await? {
            Some(target) => reset_form_page(Some(&target.nickname), &record.token, None),
            None => invalid_token_page(),
        },
        None => invalid_token_page(),
    };
    Ok(Html(page))
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

/// `POST /api/v1/pleroma/password_reset` — complete the reset. Every
/// answer is a 200 HTML page (Pleroma's `do_reset` renders, it never
/// errors at the transport): "Password changed!" on success, the form
/// again with the failure's message on anything else.
async fn do_reset(
    State(state): State<AppState>,
    ApiBody(submission): ApiBody<ResetSubmission>,
) -> AppResult<Html<String>> {
    let page = reset_attempt(
        &state,
        submission.token(),
        submission.password(),
        submission.password_confirmation(),
    )
    .await?;
    Ok(Html(page))
}

/// The completion, in order: the token (unused, unexpired, and naming a
/// living account), the password length, the confirmation — then the
/// transaction that changes the password and burns every credential the
/// old one opened. A failed step re-renders the form with its message.
async fn reset_attempt(
    state: &AppState,
    token: &str,
    password: &str,
    confirmation: &str,
) -> AppResult<String> {
    let Some(record) = password_reset::get_valid(&state.db, token, token_ttl(state)).await? else {
        return Ok(reset_form_page(None, token, Some(INVALID_TOKEN)));
    };
    let Some(target) = user::get_by_id(&state.db, record.user_id).await? else {
        return Ok(reset_form_page(None, token, Some(INVALID_TOKEN)));
    };
    if password.chars().count() < state.config.min_password_length {
        return Ok(reset_form_page(None, token, Some(PASSWORD_TOO_SHORT)));
    }
    if password != confirmation {
        return Ok(reset_form_page(None, token, Some(PASSWORD_MISMATCH)));
    }
    let new_hash = hash_password(password)?;
    if !complete_reset(state, &target, record.id, &new_hash).await? {
        return Ok(reset_form_page(None, token, Some(INVALID_TOKEN)));
    }
    debug!(account = %target.nickname, "password reset: password changed");
    Ok(success_page())
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

/// The configured token window as a [`Duration`] — the same
/// `usize`-seconds-to-`u64`-seconds shape the OAuth endpoints use for their
/// TTL (see [`crate::api::oauth`]).
fn token_ttl(state: &AppState) -> Duration {
    Duration::from_secs(u64::try_from(state.config.password_reset_ttl_secs).unwrap_or(u64::MAX))
}

/// The "Invalid Token" page (Pleroma's `invalid_token.html.eex`): the
/// heading and nothing else.
fn invalid_token_page() -> String {
    let mut out = String::new();
    push_page_head(&mut out);
    push_heading(&mut out, INVALID_TOKEN);
    push_page_tail(&mut out);
    out
}

/// The success page (Pleroma's `reset_success.html.eex`): the heading
/// ("Password changed!") and nothing else.
fn success_page() -> String {
    let mut out = String::new();
    push_page_head(&mut out);
    push_heading(&mut out, "Password changed!");
    push_page_tail(&mut out);
    out
}

/// The reset form, re-rendered after `message` when one failed: the
/// heading with the account's nickname when it is known (Pleroma's
/// "Password Reset for &#64;nickname"), and the form posting
/// `data[password]`, `data[password_confirmation]` and the hidden
/// `data[token]` to [`RESET_PATH`] — every dynamic value escaped.
fn reset_form_page(nickname: Option<&str>, token: &str, message: Option<&str>) -> String {
    let heading = nickname.map_or_else(
        || "Password Reset".to_owned(),
        |nickname| format!("Password Reset for {nickname}"),
    );
    let mut out = String::new();
    push_page_head(&mut out);
    push_heading(&mut out, &heading);
    if let Some(message) = message {
        push_error(&mut out, message);
    }
    out.push_str("<form method=\"post\" action=\"");
    out.push_str(RESET_PATH);
    out.push_str(
        "\" accept-charset=\"UTF-8\">\n\
         <div class=\"form-row\">\n\
         <label for=\"data_password\">Password</label>\n\
         <input type=\"password\" id=\"data_password\" name=\"data[password]\">\n\
         </div>\n\
         <div class=\"form-row\">\n\
         <label for=\"data_password_confirmation\">Confirmation</label>\n\
         <input type=\"password\" id=\"data_password_confirmation\" name=\"data[password_confirmation]\">\n\
         </div>\n\
         <input type=\"hidden\" name=\"data[token]\" value=\"",
    );
    push_escaped(&mut out, token);
    out.push_str("\">\n<input type=\"submit\" value=\"Reset\">\n</form>\n");
    push_page_tail(&mut out);
    out
}

/// The shared page opening of the three answers: no scripts, no external
/// assets — these pages exist for the reset flow alone, and this server has
/// no frontend (decision 1).
fn push_page_head(out: &mut String) {
    out.push_str(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>Password Reset</title>\n\
         </head>\n<body>\n<main>\n",
    );
}

/// The shared page closing that matches [`push_page_head`].
fn push_page_tail(out: &mut String) {
    out.push_str("</main>\n</body>\n</html>\n");
}

/// Appends the `<h2>` heading carrying `text`, escaped.
fn push_heading(out: &mut String, text: &str) {
    out.push_str("<h2>");
    push_escaped(out, text);
    out.push_str("</h2>\n");
}

/// Appends the error line carrying `message`, escaped: the notice a failed
/// completion leaves above the re-rendered form.
fn push_error(out: &mut String, message: &str) {
    out.push_str("<p class=\"error\" role=\"alert\">");
    push_escaped(out, message);
    out.push_str("</p>\n");
}

/// Appends `text` with the unsafe HTML characters escaped.
fn push_escaped(out: &mut String, text: &str) {
    out.push_str(&escape_html(text));
}

#[cfg(test)]
mod tests {
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
    fn form_page_shows_the_nickname_token_and_action() {
        let page = reset_form_page(Some("alice"), "tok3n", None);
        assert!(page.contains("<h2>Password Reset for alice</h2>"), "{page}");
        assert!(
            page.contains("action=\"/api/v1/pleroma/password_reset\""),
            "{page}"
        );
        assert!(page.contains("name=\"data[password]\""), "{page}");
        assert!(
            page.contains("name=\"data[password_confirmation]\""),
            "{page}"
        );
        assert!(page.contains("value=\"tok3n\""), "{page}");
        assert!(
            !page.contains("class=\"error\""),
            "no message renders: {page}"
        );
    }

    #[test]
    fn form_page_carries_an_error_message_when_one_failed() {
        let page = reset_form_page(None, "t", Some(PASSWORD_TOO_SHORT));
        assert!(page.contains("<p class=\"error\""), "{page}");
        assert!(page.contains(PASSWORD_TOO_SHORT), "{page}");
        assert!(
            page.contains("value=\"t\""),
            "the retry keeps the token: {page}"
        );
    }

    #[test]
    fn pages_escape_client_influenced_text() {
        let page = reset_form_page(
            Some(r#"evil"><script>alert(1)</script>"#),
            r#"x" onload="evil()" "#,
            Some("a<b>&c"),
        );
        assert!(
            !page.contains("<script>"),
            "the nickname stays escaped: {page}"
        );
        assert!(page.contains("&lt;script&gt;"), "{page}");
        assert!(!page.contains("value=\"x\" onload=\"evil()\""), "{page}");
        assert!(
            page.contains("a&lt;b&gt;&amp;c"),
            "the message escapes: {page}"
        );
    }

    #[test]
    fn status_pages_render_their_headings() {
        assert!(invalid_token_page().contains("<h2>Invalid Token</h2>"));
        assert!(success_page().contains("<h2>Password changed!</h2>"));
        assert!(invalid_token_page().starts_with("<!DOCTYPE html>"));
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
        let state = AppState::for_tests(db.pool().clone(), crate::config::Config::for_tests());
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
        assert!(first.contains("Password changed!"), "{first}");
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
        assert!(second.contains("Invalid Token"), "{second}");
        assert_eq!(
            stored_hash(&state, target.id).await,
            landed,
            "the replay left the password unchanged"
        );
    }

    #[tokio::test]
    async fn complete_reset_with_a_token_spent_underneath_it_rolls_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), crate::config::Config::for_tests());
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
            password_reset::mark_used(&state.db, record.id)
                .await
                .expect("the rival spend runs"),
            "the rival wins the spend"
        );
        let landed = complete_reset(&state, &target, record.id, "$argon2id$loser")
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
