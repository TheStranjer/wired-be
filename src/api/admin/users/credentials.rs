//! The credentials read and write: the owner view, the request bodies and
//! the two entry checks that fold a patch into the stored account.

use std::{fmt, slice};

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use super::credentials_apply::{apply_credentials, check_email, check_password};
use super::credentials_checks::{
    ProfileChecks, checked_actor, checked_metadata, checked_name, checked_note, checked_scope,
    checked_store,
};
use super::{
    AREA_ACCOUNTS, CLASS_READ, CLASS_WRITE, SUCCESS, UserError, UserResult, local_account,
};
use crate::{
    auth::admin::AdminUser,
    entities::admin_account::AdminCredentials,
    error::AppResult,
    extract::{ApiBody, flex},
    models::{
        moderation_log,
        user::{ProfileFields, User, register},
    },
    state::AppState,
};

/// `GET /api/v1/pleroma/admin/users/{nickname}/credentials` — the
/// moderation form's own entity: Pleroma's `"credentials.json"` over the
/// local account the segment names.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::RouteNotFound`] from
/// [`local_account`]; [`AppError::Database`] from the read.
pub(super) async fn show_user_credentials(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminCredentials>> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    let found = local_account(&state, &raw).await?;
    debug!(
        id = %found.id,
        nickname = %found.nickname,
        "admin credentials shown"
    );
    Ok(Json(AdminCredentials::from_user(&found, &state.config)))
}

/// The parameters of `PATCH /api/v1/pleroma/admin/users/{nickname}/credentials`:
/// the columns Pleroma's `update_as_admin` owns — the profile fields of the
/// user-facing [`update_credentials`] plus `email`, `password` and its
/// confirmation — every one of them optional, a JSON `null` reading as
/// absent exactly as Pleroma drops nil-valued parameters before the
/// changeset.
///
/// The preference booleans arrive through [`flex::opt_bool`], so a form's
/// `"true"`/`"on"`/`"1"` spells the same as JSON `true` and an unknown
/// string is the extractor's 422; `raw_fields` and `pleroma_settings_store`
/// stay [`Value`]s so every shape survives the body parser and
/// [`stored_fields`] / [`checked_store`] fold them.
///
/// [`update_credentials`]: crate::api::accounts
#[derive(Deserialize)]
pub(super) struct CredentialsParams {
    /// The display name: [`sanitized_name`]'s rules, refused under `name`.
    #[serde(default)]
    pub(super) name: Option<String>,
    /// The profile note: the user endpoint's length rule, then
    /// [`render_note`]'s rendering, refused under `bio`.
    #[serde(default)]
    pub(super) bio: Option<String>,
    /// The profile metadata, [`stored_fields`]' input, refused under
    /// `raw_fields`.
    #[serde(default)]
    pub(super) raw_fields: Option<Value>,
    /// The address to store, format- and uniqueness-checked.
    #[serde(default)]
    pub(super) email: Option<String>,
    /// The visibility new statuses default to, one of [`SCOPES`].
    #[serde(default)]
    pub(super) default_scope: Option<String>,
    /// The actor kind, one of [`ACTOR_TYPES`]; it also re-decides `bot`.
    #[serde(default)]
    pub(super) actor_type: Option<String>,
    /// The client settings patch, shallow-merged over the stored blob.
    #[serde(default)]
    pub(super) pleroma_settings_store: Option<Value>,
    /// Whether follow requests need manual approval.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) is_locked: Option<bool>,
    /// Whether the account is in the discovery pool.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) is_discoverable: Option<bool>,
    /// Whether clients should render the account's content as plain text.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) no_rich_text: Option<bool>,
    /// Whether the follow collection is hidden from other accounts.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) hide_follows: Option<bool>,
    /// Whether the follower collection is hidden from other accounts.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) hide_followers: Option<bool>,
    /// Whether the follower count renders as zero for others.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) hide_followers_count: Option<bool>,
    /// Whether the follow count renders as zero for others.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) hide_follows_count: Option<bool>,
    /// Whether the favourites list is private to the account.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) hide_favorites: Option<bool>,
    /// Whether a `Move` of the account may take the following list with it.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) allow_following_move: Option<bool>,
    /// Whether the staff flags may be shown to other accounts.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) show_role: Option<bool>,
    /// Whether the account's statuses skip thread containment.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) skip_thread_containment: Option<bool>,
    /// The new password, in plain text — length-checked, hashed on apply.
    #[serde(default)]
    pub(super) password: Option<String>,
    /// The confirmation of [`password`](Self::password); present-and-
    /// different is the refusal, absent is Pleroma's overwrite semantics
    /// (the admin does not have to type the password twice).
    #[serde(default)]
    pub(super) password_confirmation: Option<String>,
}

/// Neither `password` nor `password_confirmation` ever renders: the body
/// carries the plaintext the admin (or a leaked form) chose.
impl fmt::Debug for CredentialsParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialsParams")
            .field("name", &self.name)
            .field("bio", &self.bio)
            .field("raw_fields", &self.raw_fields)
            .field("email", &self.email)
            .field("default_scope", &self.default_scope)
            .field("actor_type", &self.actor_type)
            .field("pleroma_settings_store", &self.pleroma_settings_store)
            .field("is_locked", &self.is_locked)
            .field("is_discoverable", &self.is_discoverable)
            .field("no_rich_text", &self.no_rich_text)
            .field("hide_follows", &self.hide_follows)
            .field("hide_followers", &self.hide_followers)
            .field("hide_followers_count", &self.hide_followers_count)
            .field("hide_follows_count", &self.hide_follows_count)
            .field("hide_favorites", &self.hide_favorites)
            .field("allow_following_move", &self.allow_following_move)
            .field("show_role", &self.show_role)
            .field("skip_thread_containment", &self.skip_thread_containment)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field(
                "password_confirmation",
                &self.password_confirmation.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl CredentialsParams {
    /// Whether the body carries any of the preference booleans — half of
    /// what makes a credentials PATCH a profile write at all.
    pub(super) fn any_flag(&self) -> bool {
        [
            self.is_locked,
            self.is_discoverable,
            self.no_rich_text,
            self.hide_follows,
            self.hide_followers,
            self.hide_followers_count,
            self.hide_follows_count,
            self.hide_favorites,
            self.allow_following_move,
            self.show_role,
            self.skip_thread_containment,
        ]
        .into_iter()
        .any(|flag| flag.is_some())
    }
}

/// `PATCH /api/v1/pleroma/admin/users/{nickname}/credentials` — Pleroma's
/// `update_user_credentials`: validate the whole body Ecto-style, write
/// what passed and log it.
///
/// # Errors
///
/// [`UserError::Invalid`] for the field map of the refusals (see
/// [`check_credentials`]); [`AppError::Unauthorized`] /
/// [`AppError::Forbidden`] from [`AdminUser`] and its `admin:write` gate;
/// [`AppError::RouteNotFound`] from [`local_account`];
/// [`AppError::Unprocessable`] from the body extractor for an unusable
/// boolean; [`AppError::Internal`] from the password hashing; and
/// [`AppError::Database`] from any read or write.
pub(super) async fn update_user_credentials(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
    body: ApiBody<CredentialsParams>,
) -> UserResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let found = local_account(&state, &raw).await?;
    let patch = check_credentials(&state, &found, &body.0).await?;
    apply_credentials(&state, &found, patch).await?;
    let logged =
        moderation_log::updated_users(&state.db, &admin.0, slice::from_ref(&found)).await?;
    debug!(
        log = %logged.id,
        id = %found.id,
        nickname = %found.nickname,
        "admin user credentials updated"
    );
    Ok(Json(json!({ "status": SUCCESS })))
}

/// A validated credentials body: the writes it asks for, with every value
/// checked, merged and rendered, and nothing left to refuse.
#[derive(Debug)]
pub(super) struct CredentialsPatch {
    /// The whole profile column set for
    /// [`update_profile`](crate::models::user::update_profile), `None` when
    /// the body named no profile field at all.
    pub(super) profile: Option<ProfileFields>,
    /// The new address for [`set_email`](crate::models::user::set_email),
    /// `None` for no email write.
    pub(super) email: Option<String>,
    /// The new password in plain text — only its hash ever reaches a row,
    /// and it never lives past this request.
    pub(super) password: Option<String>,
}

/// Runs Pleroma's `update_as_admin` changeset over the submitted body:
/// every rule answers with its Ecto message under its own field name, and a
/// body that refuses on several fields reports all of them at once. With
/// nothing refused, the checked writes come back for [`apply_credentials`].
///
/// # Errors
///
/// [`UserError::Invalid`] when any field refused; [`AppError::Database`]
/// from the uniqueness lookup and the note's mention resolution.
pub(super) async fn check_credentials(
    state: &AppState,
    current: &User,
    params: &CredentialsParams,
) -> Result<CredentialsPatch, UserError> {
    let mut errors = register::FieldErrors::new();
    let profile = check_profile(state, current, params, &mut errors).await?;
    let email = check_email(state, current, params.email.as_deref(), &mut errors).await?;
    let password = check_password(&state.config, params, &mut errors);
    if !errors.is_empty() {
        return Err(UserError::Invalid(errors));
    }
    Ok(CredentialsPatch {
        profile,
        email,
        password,
    })
}

/// The profile half of the changeset: the six field checks, then — when the
/// body touched any profile column — the stored row with every submission
/// written on top, ready for the one-statement [`update_profile`].
///
/// # Errors
///
/// [`AppError::Database`] from [`render_note`]'s mention lookups; the
/// refusals land in `errors`, not in the return.
///
/// [`update_profile`]: crate::models::user::update_profile
pub(super) async fn check_profile(
    state: &AppState,
    current: &User,
    params: &CredentialsParams,
    errors: &mut register::FieldErrors,
) -> AppResult<Option<ProfileFields>> {
    let checks = ProfileChecks {
        name: checked_name(params.name.as_deref(), &state.config, errors)?,
        note: checked_note(params.bio.as_deref(), &state.config, errors),
        metadata: checked_metadata(params.raw_fields.clone(), &state.config, errors)?,
        scope: checked_scope(params.default_scope.as_deref(), errors).map(str::to_owned),
        actor: checked_actor(params.actor_type.as_deref(), errors),
        store: checked_store(params.pleroma_settings_store.as_ref(), current, errors),
    };
    if checks.untouched(params) {
        return Ok(None);
    }
    Ok(Some(checks.merge_into(state, current, params).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::admin::users::{
        maintenance::{FollowBody, NicknameBody, NicknamesBody, PasswordResetLink},
        test_support::params,
    };

    /// An absent key and a JSON `null` read alike — the changeset drops
    /// both before casting — and a form's string booleans reach the same
    /// flags as JSON's, while unusable bool text is the parse error the
    /// extractor turns into its 422.
    #[test]
    fn the_credentials_body_reads_null_as_absent_and_forms_as_flex() {
        let bare = params(json!({}));
        assert!(
            bare.name.is_none()
                && bare.email.is_none()
                && bare.password.is_none()
                && bare.is_locked.is_none()
                && !bare.any_flag(),
            "nothing stated: {bare:?}"
        );
        let nulled = params(json!({ "name": null, "pleroma_settings_store": null }));
        assert!(
            nulled.name.is_none() && nulled.pleroma_settings_store.is_none(),
            "nulls are absence"
        );
        let flagged = params(json!({
            "is_locked": "on",
            "hide_favorites": "0",
            "is_discoverable": true,
        }));
        assert_eq!(flagged.is_locked, Some(true), "the form's on");
        assert_eq!(flagged.hide_favorites, Some(false), "and its 0");
        assert_eq!(flagged.is_discoverable, Some(true), "and JSON's own");
        assert!(flagged.any_flag(), "a stated flag counts");
        let junk = serde_json::from_value::<CredentialsParams>(json!({ "is_locked": "maybe" }));
        assert!(junk.is_err(), "unusable bool text is the extractor's 422");
    }

    /// Neither the credentials body nor an issued reset link ever renders
    /// its secrets.
    #[test]
    fn the_secret_bodies_redact_their_debug_output() {
        let body = params(json!({
            "password": "hunter2 password",
            "password_confirmation": "hunter2 password",
        }));
        let shown_body = format!("{body:?}");
        assert!(
            !shown_body.contains("hunter2"),
            "the plaintext never reaches a log line: {shown_body}"
        );
        let issued = PasswordResetLink::new(&crate::config::Config::for_tests(), "a-token-text");
        let shown_link = format!("{issued:?}");
        assert!(
            !shown_link.contains("a-token-text"),
            "the minted credential never either: {shown_link}"
        );
        assert_eq!(
            serde_json::to_value(&issued).expect("the answer serializes"),
            json!({
                "token": "a-token-text",
                "link": "https://test.example/password_reset/a-token-text",
            }),
            "the wire answer carries both where Debug withholds them"
        );
    }

    /// The three list bodies read exactly the parameters the routes pin,
    /// and the follow pair insists on both names.
    #[test]
    fn the_bodies_read_their_pinned_parameters() {
        let names: NicknamesBody =
            serde_json::from_value(json!({ "nicknames": ["a", "b"] })).expect("the list body");
        assert_eq!(names.nicknames, vec!["a", "b"], "the submitted order");
        let one: NicknameBody =
            serde_json::from_value(json!({ "nickname": "alice" })).expect("the lone body");
        assert_eq!(one.nickname, "alice");
        let pair: FollowBody = serde_json::from_value(json!({
            "follower": "alice",
            "followed": "bob",
        }))
        .expect("the follow body");
        assert_eq!(pair.follower, "alice", "both ends");
        assert_eq!(pair.followed, "bob", "are required");
        assert!(
            serde_json::from_value::<FollowBody>(json!({ "follower": "alice" }))
                .expect_err("half a pair does not parse")
                .to_string()
                .contains("followed"),
            "and the error names the missing one"
        );
        assert!(
            serde_json::from_value::<NicknamesBody>(json!({}))
                .expect_err("no list does not parse")
                .to_string()
                .contains("nicknames"),
            "and says so"
        );
    }
}
