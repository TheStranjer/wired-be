//! Account lifecycle events: core says what happened, each protocol reacts.
//!
//! When something happens to an account, core fires one [`AccountEvent`] and
//! every enabled protocol decides what it means for itself. Core never names a
//! protocol: an `ActivityPub` `Update`, a future Matrix profile change and a
//! future `ATProto` firehose event all hang off the same
//! [`AccountEvent::ProfileChanged`].
//!
//! # After commit versus inside the transaction
//!
//! There are two hooks on [`ProtocolAdapter`], and the difference is *when*
//! they run:
//!
//! - [`on_account_event`](ProtocolAdapter::on_account_event) runs **after the
//!   change has committed**, through [`dispatch`]. The account is already in
//!   its new state, so a failing hook cannot undo it: [`dispatch`] logs the
//!   error and carries on with the other protocols and the caller. Network
//!   work (federating, announcing) belongs in a job enqueued from here, never
//!   inline.
//! - [`on_password_change_in_tx`](ProtocolAdapter::on_password_change_in_tx)
//!   runs **inside the password-change transaction**, through
//!   [`password_changed_in_tx`], because that is the only moment the plaintext
//!   password exists. A failure here propagates, so the transaction rolls back
//!   and the old password stays. The plaintext must never be stored or logged.
//!
//! A password change therefore fires both: the in-transaction hook while the
//! transaction is open, then [`AccountEvent::PasswordChanged`] once it commits.

use sqlx::PgConnection;
use tracing::warn;

use crate::{
    error::AppResult,
    models::user::User,
    protocols::{adapter::ProtocolAdapter, registry},
    state::AppState,
};

/// Something that happened to an account, for every enabled protocol to react to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountEvent {
    /// The account was created; fired after the registration transaction commits.
    Registered,
    /// The password changed; fired after any password change commits.
    PasswordChanged,
    /// The display name, bio, avatar, banner, fields or featured tags changed.
    ProfileChanged,
    /// A moderator deactivated the account.
    Suspended,
    /// A moderator activated the account again.
    Unsuspended,
    /// The owner disabled their own account.
    Deactivated,
    /// The owner enabled their account again.
    ///
    /// No endpoint fires it yet, because Pleroma has no self re-enable.
    Reactivated,
    /// The account was deleted, by its owner or by staff.
    Deleted,
}

/// Tells every enabled protocol, in registry order, that `event` happened to `user`.
///
/// Call it after the change has committed. An adapter that fails is logged
/// with `tracing::warn!` and does not stop the others or fail the caller.
pub async fn dispatch(state: &AppState, user: &User, event: AccountEvent) {
    let adapters: Vec<&'static dyn ProtocolAdapter> = registry::enabled(&state.config).collect();
    dispatch_to(&adapters, state, user, event).await;
}

/// Calls [`ProtocolAdapter::on_account_event`] on each of `adapters` in turn.
///
/// Split from [`dispatch`] so a test can choose the adapters.
async fn dispatch_to(
    adapters: &[&'static dyn ProtocolAdapter],
    state: &AppState,
    user: &User,
    event: AccountEvent,
) {
    for adapter in adapters {
        if let Err(error) = adapter.on_account_event(state, user, event).await {
            warn!(
                adapter = adapter.id().as_str(),
                event = ?event,
                user_id = %user.id,
                %error,
                "protocol failed to handle an account event",
            );
        }
    }
}

/// Gives every enabled protocol, in registry order, the new plaintext password.
///
/// Call it inside the password-change transaction, through `conn`. The first
/// error is returned so the caller's transaction rolls back. The plaintext
/// must never be stored or logged.
///
/// # Errors
///
/// The first error an adapter's
/// [`on_password_change_in_tx`](ProtocolAdapter::on_password_change_in_tx) returns.
pub async fn password_changed_in_tx(
    conn: &mut PgConnection,
    state: &AppState,
    user: &User,
    new_password: &str,
) -> AppResult<()> {
    for adapter in registry::enabled(&state.config) {
        adapter
            .on_password_change_in_tx(conn, user, new_password)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::{
            BASE_URL_VAR, Config, DATABASE_URL_VAR, PROTOCOLS_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
        },
        protocols::{
            activitypub::ActivityPub,
            fixture::{Fixture, Recorded, recorded_for},
        },
        test_fixtures::local_user,
    };

    /// A state whose `PROTOCOLS` is `protocols`.
    fn state_enabling(pool: &PgPool, protocols: &str) -> AppState {
        let config = Config::from_lookup(|key| match key {
            DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
            BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
            PROTOCOLS_VAR => Some(protocols.to_owned()),
            _ => None,
        })
        .expect("the fixed events test config is valid");
        AppState::for_tests(pool.clone(), config)
    }

    /// The fixture's calls for `user`, as the events it saw.
    fn events_seen(user: &User) -> Vec<Recorded> {
        recorded_for(user.id)
    }

    /// Dispatch reaches the fixture when it is enabled, once per event.
    #[tokio::test]
    async fn dispatch_calls_every_enabled_adapter() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = state_enabling(db.pool(), "activitypub,fixture");
        let user = local_user(db.pool(), "dispatch_enabled").await;

        dispatch(&state, &user, AccountEvent::Registered).await;
        dispatch(&state, &user, AccountEvent::Deleted).await;

        assert_eq!(
            events_seen(&user),
            vec![
                Recorded::Event(AccountEvent::Registered),
                Recorded::Event(AccountEvent::Deleted),
            ],
            "the fixture saw both events in order"
        );
    }

    /// A protocol the operator did not enable is not called.
    #[tokio::test]
    async fn dispatch_skips_a_disabled_adapter() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = state_enabling(db.pool(), "activitypub");
        let user = local_user(db.pool(), "dispatch_disabled").await;

        dispatch(&state, &user, AccountEvent::ProfileChanged).await;

        assert_eq!(events_seen(&user), Vec::new(), "the fixture was not called");
    }

    /// Adapters are called in the order given, and a failing one does not stop the next.
    #[tokio::test]
    async fn dispatch_to_visits_adapters_in_order_past_a_failure() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = state_enabling(db.pool(), "activitypub,fixture");
        let user = local_user(db.pool(), "fail-hook-order").await;
        let adapters: [&'static dyn ProtocolAdapter; 3] = [&Fixture, &ActivityPub, &Fixture];

        dispatch_to(&adapters, &state, &user, AccountEvent::Suspended).await;

        assert_eq!(
            events_seen(&user),
            vec![
                Recorded::Event(AccountEvent::Suspended),
                Recorded::Event(AccountEvent::Suspended),
            ],
            "the fixture ran before and after `ActivityPub`, despite failing"
        );
    }

    /// A failing fixture hook neither stops `ActivityPub`'s nor fails the caller.
    #[tokio::test]
    async fn a_failing_hook_does_not_fail_dispatch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = state_enabling(db.pool(), "activitypub,fixture");
        let user = local_user(db.pool(), "fail-hook-dispatch").await;

        dispatch(&state, &user, AccountEvent::Deactivated).await;

        assert_eq!(
            events_seen(&user),
            vec![Recorded::Event(AccountEvent::Deactivated)],
            "the failing hook ran and dispatch returned normally"
        );
    }

    /// The password hook runs on every enabled adapter and records no password.
    #[tokio::test]
    async fn password_changed_in_tx_calls_enabled_adapters() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = state_enabling(db.pool(), "activitypub,fixture");
        let user = local_user(db.pool(), "password_hook").await;
        let mut tx = db.pool().begin().await.expect("a transaction");

        let result = password_changed_in_tx(&mut tx, &state, &user, "new-secret").await;

        assert!(result.is_ok(), "no adapter refused");
        assert_eq!(
            events_seen(&user),
            vec![Recorded::PasswordChangedInTx],
            "the fixture was told a password changed"
        );
    }

    /// A disabled adapter's password hook is not called.
    #[tokio::test]
    async fn password_changed_in_tx_skips_a_disabled_adapter() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = state_enabling(db.pool(), "activitypub");
        let user = local_user(db.pool(), "password_hook_off").await;
        let mut tx = db.pool().begin().await.expect("a transaction");

        let result = password_changed_in_tx(&mut tx, &state, &user, "new-secret").await;

        assert!(result.is_ok(), "nothing to refuse");
        assert_eq!(events_seen(&user), Vec::new(), "the fixture was not called");
    }

    /// An adapter's error comes back to the caller, so its transaction rolls back.
    #[tokio::test]
    async fn password_changed_in_tx_propagates_an_error() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = state_enabling(db.pool(), "activitypub,fixture");
        let user = local_user(db.pool(), "fail-hook-password").await;
        let mut tx = db.pool().begin().await.expect("a transaction");

        let result = password_changed_in_tx(&mut tx, &state, &user, "new-secret").await;

        assert!(result.is_err(), "the fixture's error propagates");
    }
}
