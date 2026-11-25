//! The credentials write's tail: the email and password checks and the
//! write that applies a checked patch.

use tracing::debug;

use super::credentials::{CredentialsParams, CredentialsPatch};
use super::{CANNOT_BE_BLANK, DOES_NOT_MATCH, HAS_ALREADY_TAKEN, HAS_INVALID_FORMAT, refused};
use crate::{
    api::accounts::after_profile_updated,
    config::Config,
    error::{AppError, AppResult},
    models::{
        oauth_token,
        user::{self, User, register},
    },
    protocols::events::{self, AccountEvent},
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
/// Right after the hash is set, every enabled protocol is handed the
/// plaintext inside that transaction ([`events::password_changed_in_tx`]);
/// a protocol that refuses rolls the password change back. Once the
/// transaction commits they all hear [`AccountEvent::PasswordChanged`].
///
/// # Errors
///
/// [`AppError::Internal`] when the profile write matches no row (the
/// account vanished mid-request) or the hashing fails;
/// [`AppError::Database`] from any statement, or the error of a protocol's
/// in-transaction password hook.
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
        after_profile_updated(state, &updated).await;
    }
    if let Some(email) = patch.email.as_deref() {
        user::set_email(&state.db, current.id, email, current.email_confirmed.get()).await?;
    }
    if let Some(password) = patch.password.as_deref() {
        let hash = hash_password(password)?;
        let mut tx = state.db.begin().await?;
        user::set_password(&mut *tx, current.id, &hash).await?;
        events::password_changed_in_tx(&mut tx, state, current, password).await?;
        let revoked = oauth_token::delete_all_for_user(&mut *tx, current.id).await?;
        tx.commit().await?;
        events::dispatch(state, current, AccountEvent::PasswordChanged).await;
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

    use crate::{
        api::admin::users::test_support::params,
        protocols::fixture::{Recorded, recorded_for},
        test_support::app_state_with_fixture,
        util::password::verify_password,
    };

    /// The password an account has before staff set another.
    const OLD_PASSWORD: &str = "old-password-1";
    /// The password staff set.
    const NEW_PASSWORD: &str = "new-password-2";

    /// Inserts the local account `nickname` whose password is [`OLD_PASSWORD`].
    async fn account_with_old_password(state: &AppState, nickname: &str) -> User {
        let ap_id = user::local_ap_id(&state.config.base_url, nickname);
        let new = user::NewUser {
            password_hash: Some(hash_password(OLD_PASSWORD).expect("the password hashes")),
            ..user::NewUser::local(nickname, &ap_id)
        };
        user::insert(&state.db, &new)
            .await
            .expect("the account fixture inserts")
    }

    /// A patch that only sets [`NEW_PASSWORD`].
    fn password_patch() -> CredentialsPatch {
        CredentialsPatch {
            profile: None,
            email: None,
            password: Some(NEW_PASSWORD.to_owned()),
        }
    }

    /// The stored password hash of `account`.
    async fn stored_hash(state: &AppState, account: &User) -> String {
        user::get_by_id(&state.db, account.id)
            .await
            .expect("the account reads")
            .expect("the account exists")
            .password_hash
            .expect("the account has a password")
    }

    #[tokio::test]
    async fn setting_a_password_runs_the_in_transaction_hook_then_announces_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state_with_fixture(db.pool());
        let account = account_with_old_password(&state, "admin_set_events").await;

        apply_credentials(&state, &account, password_patch())
            .await
            .expect("the write lands");

        assert_eq!(
            recorded_for(account.id),
            vec![
                Recorded::PasswordChangedInTx,
                Recorded::Event(AccountEvent::PasswordChanged),
            ],
            "the in-transaction call came first, the event after the commit"
        );
        assert!(
            verify_password(NEW_PASSWORD, &stored_hash(&state, &account).await),
            "the new password"
        );
    }

    #[tokio::test]
    async fn a_failing_in_transaction_hook_rolls_the_admin_password_write_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state_with_fixture(db.pool());
        let account = account_with_old_password(&state, "fail-hook-admin-set").await;

        let outcome = apply_credentials(&state, &account, password_patch()).await;

        assert!(outcome.is_err(), "the hook's failure fails the write");
        assert!(
            verify_password(OLD_PASSWORD, &stored_hash(&state, &account).await),
            "the old password still works"
        );
        assert_eq!(
            recorded_for(account.id),
            vec![Recorded::PasswordChangedInTx],
            "no event for a change that never committed"
        );
    }

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
