//! The credentials write's tail: the email and password checks and the
//! write that applies a checked patch.

use tracing::debug;

use super::credentials::{CredentialsParams, CredentialsPatch};
use super::{CANNOT_BE_BLANK, DOES_NOT_MATCH, HAS_ALREADY_TAKEN, HAS_INVALID_FORMAT, refused};
use crate::{
    config::Config,
    error::{AppError, AppResult},
    models::{
        oauth_token,
        user::{self, User, register},
    },
    protocols::events::{AccountEvent, dispatch},
    state::AppState,
    util::password::hash_password,
};

/// The checked address: [`is_plausible_email`] first, then the uniqueness
/// read Pleroma's `unique_constraint(:email)` stands in for — held by
/// another account is taken, held by this one is a no-op rewrite.
///
/// # Errors
///
/// [`AppError::Database`] from the uniqueness read.
pub(super) async fn check_email(
    state: &AppState,
    current: &User,
    submitted: Option<&str>,
    errors: &mut register::FieldErrors,
) -> AppResult<Option<String>> {
    let Some(email) = submitted else {
        return Ok(None);
    };
    if !register::is_plausible_email(email) {
        refused(errors, "email", HAS_INVALID_FORMAT);
        return Ok(None);
    }
    let holder = user::get_by_email(&state.db, email).await?;
    if holder.is_some_and(|other| other.id != current.id) {
        refused(errors, "email", HAS_ALREADY_TAKEN);
        return Ok(None);
    }
    Ok(Some(email.to_owned()))
}

/// The checked password, Pleroma's `password_update_changeset` rules with
/// the confirmation check the admin form keeps (see the module docs for the
/// deliberate divergence from its forced-overwrite): blank is Ecto's
/// `can't be blank`, too short the interpolated `should be at least …`, and
/// a stated confirmation that differs is refused under its own name.
/// `password_confirmation` alone, without a password, is nothing to check.
pub(super) fn check_password(
    config: &Config,
    params: &CredentialsParams,
    errors: &mut register::FieldErrors,
) -> Option<String> {
    let password = params.password.as_deref()?;
    if params
        .password_confirmation
        .as_deref()
        .is_some_and(|confirmation| confirmation != password)
    {
        refused(errors, "password_confirmation", DOES_NOT_MATCH);
    }
    if password.is_empty() {
        refused(errors, "password", CANNOT_BE_BLANK);
        return None;
    }
    if password.chars().count() < config.min_password_length {
        refused(
            errors,
            "password",
            format!(
                "should be at least {} character(s)",
                config.min_password_length
            ),
        );
        return None;
    }
    Some(password.to_owned())
}

/// Runs the checked writes: the profile columns in one statement (whose
/// fresh row runs the user endpoint's own post-update hook), then the
/// address, then the password beside its token revocations in one
/// transaction — the pair Pleroma's reset `Multi` lands together.
///
/// A password write clears [`password_reset_pending`] as it goes (the
/// [`set_password`] rule): an admin who just chose the password has ended
/// any pending reset.
///
/// # Errors
///
/// [`AppError::Internal`] when the profile write matches no row (the
/// account vanished mid-request) or the hashing fails;
/// [`AppError::Database`] from any statement.
///
/// [`password_reset_pending`]: User::password_reset_pending
/// [`set_password`]: crate::models::user::set_password
pub(super) async fn apply_credentials(
    state: &AppState,
    current: &User,
    patch: CredentialsPatch,
) -> AppResult<()> {
    if let Some(profile) = patch.profile.as_ref() {
        let updated = user::update_profile(&state.db, current.id, profile)
            .await?
            .ok_or_else(|| AppError::Internal("credentials update matched no row".to_owned()))?;
        dispatch(state, &updated, AccountEvent::ProfileChanged).await;
    }
    if let Some(email) = patch.email.as_deref() {
        user::set_email(&state.db, current.id, email, current.email_confirmed.get()).await?;
    }
    if let Some(password) = patch.password.as_deref() {
        let hash = hash_password(password)?;
        let mut tx = state.db.begin().await?;
        user::set_password(&mut *tx, current.id, &hash).await?;
        let revoked = oauth_token::delete_all_for_user(&mut *tx, current.id).await?;
        tx.commit().await?;
        debug!(
            revoked,
            "admin credentials write revoked the account's tokens"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    use crate::api::admin::users::test_support::params;

    /// The password check reports the password's own fault and the
    /// confirmation's mismatch independently, the way a changeset does:
    /// blank, too short, mismatching and the pair clean.
    #[test]
    fn the_password_check_reports_both_sides_of_its_rule() {
        /// One run of the check against `body`, asserted to leave `expected_write`
        /// standing and report exactly `expected_errors`.
        fn check(
            body: Value,
            expected_write: Option<&str>,
            expected_errors: &register::FieldErrors,
        ) {
            let config = Config::for_tests();
            let mut errors = register::FieldErrors::new();
            let write = check_password(&config, &params(body), &mut errors);
            assert_eq!(
                write.as_deref(),
                expected_write,
                "the write the check leaves standing"
            );
            assert_eq!(&errors, expected_errors, "the refusals it reported");
        }

        let at_least = format!(
            "should be at least {} character(s)",
            Config::for_tests().min_password_length
        );

        check(
            json!({ "password": "" }),
            None,
            &register::FieldErrors::from([("password", vec![CANNOT_BE_BLANK.to_owned()])]),
        );
        check(
            json!({ "password": "short", "password_confirmation": "other" }),
            None,
            &register::FieldErrors::from([
                ("password", vec![at_least]),
                ("password_confirmation", vec![DOES_NOT_MATCH.to_owned()]),
            ]),
        );
        check(
            json!({ "password": "long enough", "password_confirmation": "nope" }),
            Some("long enough"),
            &register::FieldErrors::from([(
                "password_confirmation",
                vec![DOES_NOT_MATCH.to_owned()],
            )]),
        );
        check(
            json!({ "password": "long enough", "password_confirmation": "long enough" }),
            Some("long enough"),
            &register::FieldErrors::new(),
        );
        check(
            json!({ "password_confirmation": "stray" }),
            None,
            &register::FieldErrors::new(),
        );
    }
}
