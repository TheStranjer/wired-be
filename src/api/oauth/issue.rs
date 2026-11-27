//! The issuance half of `POST /oauth/token`: the login the password
//! grant verifies, the readiness gate, the minted pair and the 200 body
//! that renders it.
//!
//! The gate is the one every token path (and the rest of the crate)
//! consults before an account may act; the 200 body renders the fresh
//! pair Pleroma's "token.json" shape.

use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use sqlx::{Executor, PgPool, Postgres};

use super::{
    ACCOUNT_DISABLED, ACCOUNT_IS_DISABLED, TOKEN_TYPE_BEARER,
    params::{TokenParams, invalid_credentials},
};
use crate::{
    api::password_reset::{PASSWORD_RESET_REQUIRED, PASSWORD_RESET_REQUIRED_ID},
    auth::{AWAITING_APPROVAL, AWAITING_APPROVAL_ID, EMAIL_UNCONFIRMED, EMAIL_UNCONFIRMED_ID},
    config::TokenTier,
    error::{AppError, AppResult},
    models::{
        oauth_app::OauthApp,
        oauth_token::{self, NewToken, OauthToken},
        user::{AccountStatus, User, register},
    },
    oauth::scopes::Scopes,
    services::runtime_config,
    state::AppState,
};

/// Verifies the grant's `username` (nickname or email) and `password`
/// against the users table.
///
/// # Errors
///
/// 400 [`INVALID_CREDENTIALS`](super::INVALID_CREDENTIALS) when either field is absent, or they do
/// not log in — one answer for every case.
pub(super) async fn authenticate_user(pool: &PgPool, params: &TokenParams) -> AppResult<User> {
    let username = params.username.as_deref().unwrap_or_default();
    let password = params.password.as_deref().unwrap_or_default();
    register::authenticate_any_status(pool, username, password)
        .await
        .ok_or_else(invalid_credentials)
}

/// Whether an authenticated account may receive a token right now: Pleroma's
/// `User.account_status` (`lib/pleroma/user.ex`, applied by
/// `do_create_authorization`) reduced to the states this server tracks,
/// weighed in its order — deactivated, then awaiting approval, then awaiting
/// email confirmation — with the forced password reset an admin can set (see
/// [`crate::api::password_reset`]) refused next. The MFA gate is not part of
/// it: it answers something other than a plain refusal, so [`password_grant`](super::password_grant)
/// runs [`mfa::required`](super::mfa::required) beside this check.
///
/// [`crate::api::oauth_external`] and the `POST /oauth/authorize` submission
/// gate their logins through here too, and [`crate::auth`] answers an
/// approval-pending account's still-live bearer token with the same refusal.
///
/// # Errors
///
/// 403 [`ACCOUNT_DISABLED`] with identifier [`ACCOUNT_IS_DISABLED`] for a
/// deactivated account, 403 [`AWAITING_APPROVAL`] with
/// [`AWAITING_APPROVAL_ID`] for one staff have not approved, 403
/// [`EMAIL_UNCONFIRMED`] with [`EMAIL_UNCONFIRMED_ID`] for one whose address
/// is not confirmed, and 403 [`PASSWORD_RESET_REQUIRED`] with identifier
/// [`PASSWORD_RESET_REQUIRED_ID`] for an account whose password an admin
/// forced a reset on.
pub(crate) fn check_account_ready(user: &User) -> Result<(), AppError> {
    match user.account_status() {
        AccountStatus::Active => {}
        AccountStatus::Deactivated => {
            return Err(AppError::forbidden_with_identifier(
                ACCOUNT_DISABLED,
                ACCOUNT_IS_DISABLED,
            ));
        }
        AccountStatus::ApprovalPending => {
            return Err(AppError::forbidden_with_identifier(
                AWAITING_APPROVAL,
                AWAITING_APPROVAL_ID,
            ));
        }
        AccountStatus::ConfirmationPending => {
            return Err(AppError::forbidden_with_identifier(
                EMAIL_UNCONFIRMED,
                EMAIL_UNCONFIRMED_ID,
            ));
        }
    }
    if user.password_reset_pending.get() {
        return Err(AppError::forbidden_with_identifier(
            PASSWORD_RESET_REQUIRED,
            PASSWORD_RESET_REQUIRED_ID,
        ));
    }
    Ok(())
}

/// The `actor_type` of a bot account.
const SERVICE_ACTOR_TYPE: &str = "Service";

/// Which lifetime tier the token of `user` gets: admin beats bot beats
/// everyone else, and `None` — the app-only `client_credentials` token —
/// is in the last tier.
fn token_tier(user: Option<&User>) -> TokenTier {
    match user {
        Some(user) if user.is_admin.get() => TokenTier::Admin,
        Some(user) if user.actor_type == SERVICE_ACTOR_TYPE => TokenTier::Bot,
        Some(_) | None => TokenTier::User,
    }
}

/// How long a token for `user` lives, decided now.
///
/// That is the tier's stored admin-config override, else the environment's
/// value. Read on the caller's `executor`, so the refresh grant needs no
/// second connection.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
pub(super) async fn token_ttl<'e, E>(
    executor: E,
    state: &AppState,
    user: Option<&User>,
) -> AppResult<Duration>
where
    E: Executor<'e, Database = Postgres>,
{
    let secs = runtime_config::token_ttl_secs(executor, &state.config, token_tier(user)).await?;
    Ok(Duration::from_secs(u64::try_from(secs).unwrap_or(u64::MAX)))
}

/// Mints the access/refresh token pair for the granted scopes.
///
/// It is valid for the lifetime of the user's tier (`OAUTH_TOKEN_TTL_SECS_ADMIN`,
/// `_BOT` or `_USER`, or the admin config store's override of it). `user` is
/// the account the token acts as; `None` mints the app-only token of the
/// `client_credentials` grant (`user_id = NULL`).
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
pub(crate) async fn issue_token(
    state: &AppState,
    user: Option<&User>,
    app: &OauthApp,
    scopes: Scopes,
) -> AppResult<OauthToken> {
    let ttl = token_ttl(&state.db, state, user).await?;
    issue_token_on(&state.db, user, app, scopes, ttl).await
}

/// [`issue_token`] on the caller's executor, for the `ttl` the caller got
/// from [`token_ttl`]: the refresh grant mints the replacement pair in the
/// transaction that consumed the old one.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
pub(super) async fn issue_token_on<'e, E>(
    executor: E,
    user: Option<&User>,
    app: &OauthApp,
    scopes: Scopes,
    ttl: Duration,
) -> AppResult<OauthToken>
where
    E: Executor<'e, Database = Postgres>,
{
    oauth_token::create_token(
        executor,
        &NewToken {
            user_id: user.map(|user| user.id),
            app_id: app.id,
            scopes: scopes.to_string(),
            ttl: Some(ttl),
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
pub(crate) struct TokenResponse {
    /// Token type, always `Bearer`.
    token_type: &'static str,
    /// The access token, to present as `Authorization: Bearer`.
    access_token: String,
    /// The granted scopes, space-separated.
    scope: String,
    /// Issue time, in unix seconds (Pleroma's `format_created_at`).
    created_at: i64,
    /// The token row's `UUIDv7` id, hyphenated (decision 4).
    id: String,
    /// Seconds of validity left at render time.
    expires_in: i64,
    /// The refresh token, to exchange for a fresh pair; absent for an
    /// app-only token.
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// The authenticated user's `ActivityPub` actor id; absent for an
    /// app-only token.
    #[serde(skip_serializing_if = "Option::is_none")]
    me: Option<String>,
}

impl TokenResponse {
    /// Renders the freshly minted `token`, owned by `user` — `None` for
    /// an app-only `client_credentials` token, which then serializes
    /// without `refresh_token` and `me`.
    pub(crate) fn new(token: OauthToken, user: Option<&User>) -> Self {
        let owner = user.map(|user| user.ap_id.clone());
        Self {
            token_type: TOKEN_TYPE_BEARER,
            access_token: token.token,
            scope: token.scopes,
            created_at: token.inserted_at.timestamp(),
            id: token.id.to_string(),
            expires_in: token
                .valid_until
                .signed_duration_since(Utc::now())
                .num_seconds(),
            refresh_token: owner.is_some().then_some(token.refresh_token),
            me: owner,
        }
    }

    /// This answer without its `me` key: the shape `POST /api/v1/accounts`
    /// renders (Pleroma's `me` belongs to `/oauth/token` only).
    #[must_use]
    pub(crate) fn without_me(mut self) -> Self {
        self.me = None;
        self
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        api::oauth::test_support::token_row,
        config::TEST_BASE_URL,
        error::AppError,
        models::user::{self, DEFAULT_SCOPE, Flag},
        util::id::new_id,
    };

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
            notification_settings: json!({}),
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
            confirmation_token: None,
            confirmation_sent_at: None,
            last_active_at: None,
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            inserted_at: now,
            updated_at: now,
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
            email_notifications: Flag::new(true),
            deleted_at: None,
            suspended_at: None,
            disabled_at: None,
            remote_suspended: Flag::new(false),
        }
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

    /// `user_row` with the three account-status flags set as named: the
    /// fixture behind the registration-mode arms of the gate.
    fn user_with(active: bool, approval_pending: Flag, email_confirmed: Flag) -> User {
        let mut user = user_row(active);
        user.approval_pending = approval_pending;
        user.email_confirmed = email_confirmed;
        user
    }

    /// The refusal [`check_account_ready`] answers with, both halves of it.
    fn refusal_of(user: &User) -> (String, &'static str) {
        let err = check_account_ready(user).expect_err("the account is not ready");
        let AppError::ForbiddenWithIdentifier(message, identifier) = err else {
            panic!("the refusal must carry an identifier, got {err}")
        };
        (message, identifier)
    }

    #[test]
    fn an_unapproved_account_is_refused_with_its_identifier() {
        let (message, identifier) = refusal_of(&user_with(true, Flag::new(true), Flag::new(true)));
        assert_eq!(message, AWAITING_APPROVAL, "Pleroma's wording");
        assert_eq!(identifier, AWAITING_APPROVAL_ID, "and its reason code");
    }

    #[test]
    fn an_unconfirmed_account_is_refused_with_its_identifier() {
        let (message, identifier) =
            refusal_of(&user_with(true, Flag::new(false), Flag::new(false)));
        assert_eq!(message, EMAIL_UNCONFIRMED, "Pleroma's wording");
        assert_eq!(identifier, EMAIL_UNCONFIRMED_ID, "and its reason code");
    }

    #[test]
    fn the_readiness_gate_weighs_deactivation_then_approval_then_confirmation() {
        // Pleroma's `User.account_status` order: the first arm that matches
        // is the answer, so the refusals nest.
        let (_, identifier) = refusal_of(&user_with(false, Flag::new(true), Flag::new(false)));
        assert_eq!(
            identifier, ACCOUNT_IS_DISABLED,
            "a deactivated account says disabled whatever else waits"
        );
        let (_, reason) = refusal_of(&user_with(true, Flag::new(true), Flag::new(false)));
        assert_eq!(
            reason, AWAITING_APPROVAL_ID,
            "approval is reported before the mailbox is checked"
        );
        check_account_ready(&user_with(true, Flag::new(false), Flag::new(true)))
            .expect("approved and confirmed");
    }

    #[test]
    fn an_account_awaiting_forced_reset_is_refused_with_its_identifier() {
        let mut user = user_row(true);
        user.password_reset_pending = Flag::new(true);
        let err = check_account_ready(&user).expect_err("a pending reset is not ready");
        let AppError::ForbiddenWithIdentifier(message, identifier) = &err else {
            panic!("the refusal must carry an identifier, got {err}")
        };
        assert_eq!(message, PASSWORD_RESET_REQUIRED, "the ticket's wording");
        assert_eq!(
            identifier, &PASSWORD_RESET_REQUIRED_ID,
            "and its reason code"
        );
    }

    #[test]
    fn deactivation_is_refused_before_a_pending_reset() {
        // Pleroma's `account_status` matches `is_active: false` first.
        let mut user = user_row(false);
        user.password_reset_pending = Flag::new(true);
        let err = check_account_ready(&user).expect_err("deactivated is not ready");
        let AppError::ForbiddenWithIdentifier(message, _) = &err else {
            panic!("the refusal must carry an identifier, got {err}")
        };
        assert_eq!(message, ACCOUNT_DISABLED, "the disabled answer wins");
    }

    #[test]
    fn the_tier_follows_admin_then_bot_then_user() {
        let mut user = user_row(true);
        assert_eq!(token_tier(None), TokenTier::User, "an app-only token");
        assert_eq!(token_tier(Some(&user)), TokenTier::User, "a plain Person");
        user.actor_type = SERVICE_ACTOR_TYPE.to_owned();
        assert_eq!(token_tier(Some(&user)), TokenTier::Bot, "a Service actor");
        user.is_admin = Flag::new(true);
        assert_eq!(
            token_tier(Some(&user)),
            TokenTier::Admin,
            "a bot that is an admin takes the admin tier"
        );
        user.actor_type = "Person".to_owned();
        assert_eq!(token_tier(Some(&user)), TokenTier::Admin, "an admin Person");
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
    fn the_registration_shape_is_the_token_without_me() {
        let token = token_row(new_id(), "read write follow push");
        let user = user_row(true);
        let json =
            serde_json::to_value(TokenResponse::new(token, Some(&user)).without_me()).unwrap();
        assert!(json.get("me").is_none(), "no actor id: {json}");
        for key in [
            "token_type",
            "access_token",
            "refresh_token",
            "scope",
            "created_at",
            "id",
            "expires_in",
        ] {
            assert!(json.get(key).is_some(), "`{key}` stays: {json}");
        }
        assert_eq!(
            json.as_object().expect("an object").len(),
            7,
            "exactly the registration's seven keys"
        );
    }

    #[test]
    fn the_registration_keys_serialize_in_their_documented_order() {
        let token = token_row(new_id(), "read");
        let user = user_row(true);
        let text =
            serde_json::to_string(&TokenResponse::new(token, Some(&user)).without_me()).unwrap();
        let positions: Vec<usize> = [
            "token_type",
            "access_token",
            "scope",
            "created_at",
            "id",
            "expires_in",
            "refresh_token",
        ]
        .iter()
        .map(|key| text.find(&format!("\"{key}\":")).expect("key present"))
        .collect();
        assert!(
            positions.is_sorted(),
            "the registration answer keeps its key order: {text}"
        );
    }
}
