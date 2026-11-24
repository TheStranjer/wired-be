//! The maintenance actions: the issued reset token, the forced reset, the
//! email confirmation pair, the MFA removal and the forced follow pair.

use std::{fmt, slice};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

use super::{AREA_ACCOUNTS, AREA_FOLLOWS, CLASS_WRITE, OK, local_account};
use crate::{
    api::{accounts::after_follow_changed, password_reset::RESET_PATH},
    auth::admin::AdminUser,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        follow::{self, FollowState},
        moderation_log, oauth_token, password_reset,
        user::{self, User, admin, confirmation, mfa},
    },
    state::AppState,
};

/// The issued reset: Pleroma's `%{token: …, link: …}` answer, the token's
/// text and the absolute URL that spends it.
#[derive(Serialize)]
pub(super) struct PasswordResetLink {
    /// The minted token's secret text.
    token: String,
    /// The full reset URL: [`RESET_PATH`] with the token as its segment,
    /// hung on the instance's own origin.
    link: String,
}

/// Both fields are the same credential, and neither ever renders: the
/// answer's whole point is that a log line must not leak a live reset.
impl fmt::Debug for PasswordResetLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordResetLink")
            .field("token", &"<redacted>")
            .field("link", &"<redacted>")
            .finish()
    }
}

impl PasswordResetLink {
    /// Answers `token` and the link `{base_url}{RESET_PATH}/{token}`: the
    /// route the `POST /api/v1/pleroma/password_reset` form already serves
    /// (see [`crate::api::password_reset`]), so an admin's link completes a
    /// reset exactly like the emailed one.
    pub(super) fn new(base_url: &str, token: &str) -> Self {
        Self {
            token: token.to_owned(),
            link: format!("{base_url}{RESET_PATH}/{token}"),
        }
    }
}

/// `GET /api/v1/pleroma/admin/users/{nickname}/password_reset` — Pleroma's
/// `get_password_reset`: mint a reset token for the local account and hand
/// back its text beside the link that spends it. Gated
/// `admin:write:accounts` like every other maintenance action here, and
/// left out of the moderation log: it is a mint, not a change, and Pleroma
/// does not log it either.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::RouteNotFound`] from
/// [`local_account`]; [`AppError::Database`] from the mint.
pub(super) async fn get_password_reset(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<PasswordResetLink>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let found = local_account(&state, &raw).await?;
    let issued = password_reset::create_for_user(&state.db, found.id).await?;
    debug!(
        id = %found.id,
        nickname = %found.nickname,
        "admin issued a password reset token"
    );
    Ok(Json(PasswordResetLink::new(
        &state.config.base_url,
        &issued.token,
    )))
}

/// The body of the nickname-list actions: Pleroma's `nicknames` parameter,
/// required, arriving from JSON as the array and from a form as the
/// repeated `nicknames[]` key the extractors nest.
#[derive(Debug, Deserialize)]
pub(super) struct NicknamesBody {
    /// The handles to act on, unknown ones skipped as Pleroma skips them.
    pub(super) nicknames: Vec<String>,
}

/// The body of `PUT /users/disable_mfa`: Pleroma's single required
/// `nickname`.
#[derive(Debug, Deserialize)]
pub(super) struct NicknameBody {
    /// The handle whose MFA goes away.
    pub(super) nickname: String,
}

/// The parameters of the tag pair: Pleroma's `tag_users` and `untag_users`
/// both require `nicknames` AND `tags`, each arriving from JSON as the
/// array and from a form or the query string as the repeated bracketed key
/// the [`ApiParams`] extractor nests.
#[derive(Debug, Deserialize)]
pub(super) struct TagsBody {
    /// The accounts whose profile tags change, unknown ones skipped.
    pub(super) nicknames: Vec<String>,
    /// The tags to add to — or remove from — every one of them.
    pub(super) tags: Vec<String>,
}

/// The body of the follow pair: both ends of the relationship, named by
/// nickname and both required (Pleroma's `open_api_spex` body params).
#[derive(Debug, Deserialize)]
pub(super) struct FollowBody {
    /// The local account that is made to follow.
    pub(super) follower: String,
    /// The account it follows — local or a remote mirror.
    pub(super) followed: String,
}

/// `PATCH /api/v1/pleroma/admin/users/password_reset` — Pleroma's
/// `force_password_reset`: raise `password_reset_pending` and revoke every
/// token of each named account, in one transaction per account so a forced
/// reset can never leave a live token behind an awaiting account.
///
/// A nickname no account answers to is skipped — Pleroma maps the list
/// through its lookup and acts on what it finds, and the 204 says nothing
/// about who was not there. The subject list the log entry names is what
/// was found, which can be empty.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for a body with no `nicknames`; [`AppError::Database`] from
/// the lookups, the writes and the log.
pub(super) async fn force_password_reset(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknamesBody>,
) -> AppResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let users = admin::resolve_nicknames(&state.db, &body.0.nicknames).await?;
    for found in &users {
        let mut tx = state.db.begin().await?;
        user::set_password_reset_pending(&mut *tx, found.id).await?;
        let revoked = oauth_token::delete_all_for_user(&mut *tx, found.id).await?;
        tx.commit().await?;
        debug!(
            account = %found.nickname,
            revoked,
            "admin forced a password reset"
        );
    }
    let logged = moderation_log::force_password_reset(&state.db, &admin.0, &users).await?;
    debug!(
        log = %logged.id,
        forced = users.len(),
        "admin forced password resets"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `PATCH /api/v1/pleroma/admin/users/confirm_email` — Pleroma's
/// `confirm_email`: confirm each named account's address without its token
/// (and retire any outstanding one, so a letter already in transit confirms
/// nothing). 204 where Pleroma answers an empty 200, the ticket's pinning.
///
/// # Errors
///
/// As [`force_password_reset`]'s, from the same shape of body and lookup.
pub(super) async fn confirm_email_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknamesBody>,
) -> AppResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let users = admin::resolve_nicknames(&state.db, &body.0.nicknames).await?;
    for found in &users {
        confirmation::set_confirmed(&state.db, found.id).await?;
    }
    let logged = moderation_log::confirm_email(&state.db, &admin.0, &users).await?;
    debug!(
        log = %logged.id,
        confirmed = users.len(),
        "admin confirmed user emails"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `PATCH /api/v1/pleroma/admin/users/resend_confirmation_email` —
/// Pleroma's `resend_confirmation_email`: queue the confirmation letter
/// again for the named accounts whose address is still unconfirmed. A
/// confirmed one has nothing left to confirm, and Pleroma's own
/// `send_confirmation_email` is reached only through the unconfirmed paths
/// — the flag is the filter here. Every resolved account is a log subject,
/// whether or not it was mailed.
///
/// # Errors
///
/// As [`force_password_reset`]'s, plus whatever the letter queueing
/// (`confirmation::send`) reports.
pub(super) async fn resend_confirmation_email(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknamesBody>,
) -> AppResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let users = admin::resolve_nicknames(&state.db, &body.0.nicknames).await?;
    let mut mailed = 0;
    for found in &users {
        if !found.email_confirmed.get() {
            confirmation::send(&state, found).await?;
            mailed += 1;
        }
    }
    let logged = moderation_log::resend_confirmation_email(&state.db, &admin.0, &users).await?;
    debug!(
        log = %logged.id,
        resolved = users.len(),
        mailed, "admin re-sent confirmation emails"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/v1/pleroma/admin/users/disable_mfa` — Pleroma's
/// `disable_mfa`: reset the named account's `mfa_settings` to the column
/// default — secret, backup codes and the enabled flag all gone — and
/// answer the nickname back as its `json(conn, nickname)`.
///
/// Pleroma logs nothing here; the ticket asks every write to leave an
/// entry, so the account's removal of its second factor is recorded as
/// `disable_mfa` (see [`moderation_log::disable_mfa`]).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for a body with no `nickname`; [`AppError::RouteNotFound`]
/// for a handle no account answers to; [`AppError::Database`] from the
/// writes.
pub(super) async fn disable_mfa(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknameBody>,
) -> AppResult<Json<String>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let found = user::get_by_nickname(&state.db, &body.0.nickname)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    mfa::save(&state.db, found.id, &mfa::MfaSettings::default()).await?;
    let logged = moderation_log::disable_mfa(&state.db, &admin.0, slice::from_ref(&found)).await?;
    debug!(
        log = %logged.id,
        id = %found.id,
        nickname = %found.nickname,
        "admin disabled a user's MFA"
    );
    Ok(Json(body.0.nickname))
}

/// Both accounts one follow body names, looked up by nickname: either miss
/// — and a remote follower, whose follow this instance cannot force — is
/// the module's 404, Pleroma's `with` chain answered whole. The followed
/// account may be remote: forcing a local account's follow of a mirror is
/// exactly what federation then has to carry.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for either miss or a remote follower;
/// [`AppError::Database`] from the reads.
pub(super) async fn resolve_pair(state: &AppState, body: &FollowBody) -> AppResult<(User, User)> {
    let follower = user::get_by_nickname(&state.db, &body.follower)
        .await?
        .filter(|found| found.local.get())
        .ok_or(AppError::RouteNotFound)?;
    let target = user::get_by_nickname(&state.db, &body.followed)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    Ok((follower, target))
}

/// `POST /api/v1/pleroma/admin/users/follow` — Pleroma's admin `follow`:
/// make `follower` follow `followed`, whatever the target's own answer
/// would be. A locked or remote target leaves the model's write `pending`;
/// the admin's act IS the answer, so the request is accepted on the spot
/// (Pleroma's `User.follow(follower, followed, :follow_accept)` does the
/// same even for a locked account). The user-facing endpoint's notification
/// and federation hook then runs with the accepted row, and the entry
/// joins the log.
///
/// Following yourself is nobody's order: the model's 422 (`"Can not follow
/// yourself"`) answers it.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write:follows` gate; [`AppError::Unprocessable`] from the
/// extractor for an incomplete body or from the model for the self-follow;
/// [`AppError::RouteNotFound`] from [`resolve_pair`];
/// [`AppError::Forbidden`] from the model when a block stands between the
/// pair; [`AppError::Database`] from the writes and the log.
pub(super) async fn follow_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<FollowBody>,
) -> AppResult<Json<&'static str>> {
    admin.require(CLASS_WRITE, AREA_FOLLOWS)?;
    let (follower, target) = resolve_pair(&state, &body.0).await?;
    let started = follow::follow(&state.db, &state.config.base_url, &follower, &target).await?;
    let accepted = if started.state == FollowState::Pending {
        follow::accept(&state.db, started.id).await?
    } else {
        started
    };
    after_follow_changed(&state, &follower, &target, Some(&accepted), true).await;
    let logged = moderation_log::follow(&state.db, &admin.0, &follower, &target).await?;
    debug!(
        log = %logged.id,
        follower = %follower.nickname,
        followed = %target.nickname,
        "admin forced a follow"
    );
    Ok(Json(OK))
}

/// `POST /api/v1/pleroma/admin/users/unfollow` — Pleroma's admin
/// `unfollow`: take `follower`'s follow of `followed` away, an accepted
/// relationship or a still-pending request alike, and run the same
/// hook and log as [`follow_users`]. Nothing to remove changes nothing and
/// still answers `ok` — the user endpoint's own idempotence.
///
/// # Errors
///
/// As [`follow_users`]'s, minus the self-follow refusal: the model is
/// content with a pair that never followed.
pub(super) async fn unfollow_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<FollowBody>,
) -> AppResult<Json<&'static str>> {
    admin.require(CLASS_WRITE, AREA_FOLLOWS)?;
    let (follower, target) = resolve_pair(&state, &body.0).await?;
    let previous = follow::get(&state.db, follower.id, target.id).await?;
    let removed = follow::unfollow(&state.db, &follower, &target).await?;
    after_follow_changed(
        &state,
        &follower,
        &target,
        previous.as_ref().filter(|_| removed),
        false,
    )
    .await;
    let logged = moderation_log::unfollow(&state.db, &admin.0, &follower, &target).await?;
    debug!(
        log = %logged.id,
        follower = %follower.nickname,
        followed = %target.nickname,
        removed,
        "admin forced an unfollow"
    );
    Ok(Json(OK))
}
