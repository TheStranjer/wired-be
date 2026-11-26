//! The logged activation write the admin surfaces share ([`set_activation`]).

use tracing::debug;

use crate::{
    error::AppResult,
    models::{
        moderation_log,
        user::{
            User,
            admin::{self, Standing},
        },
    },
    protocols::events::{AccountEvent, dispatch},
    state::AppState,
};

/// Sets [`is_active`](User::is_active) to `active` on every account of
/// `accounts` and records the move as one `activate` (or `deactivate`)
/// moderation-log entry by `actor` over the rows the writes returned.
///
/// The shared half of the four activation paths — the Pleroma bulk
/// `activate`/`deactivate`, its `toggle_activation`, and the Mastodon admin
/// `enable` and `action` endpoints. Which accounts may be named at all (the
/// caller's own, a remote mirror) is each endpoint's own filter, applied
/// before this; the flag is the whole write, so no token is revoked and no
/// deletion stamped. Once the writes and the log have committed, each moved
/// account fires [`AccountEvent::Suspended`] (`active` false) or
/// [`AccountEvent::Unsuspended`] (`active` true), so the protocols decide
/// what a moderation change means for them.
///
/// The answer is the updated rows, in the order of `accounts`. An account
/// whose row went away mid-request matches no write and is simply not among
/// them, so a caller that needs the row checks for an empty answer. The entry
/// is written whatever the number of moved rows, as the bulk endpoints always
/// have.
///
/// # Errors
///
/// [`AppError::Database`](crate::error::AppError) from the writes and the log.
pub async fn set_activation(
    state: &AppState,
    actor: &User,
    accounts: &[User],
    active: bool,
) -> AppResult<Vec<User>> {
    let standing = if active {
        Standing::Active
    } else {
        Standing::Suspended
    };
    set_standing(state, actor, accounts, standing).await
}

/// [`set_activation`] with the kind of deactivation named (WBE-878).
///
/// [`Standing::Disabled`] and [`Standing::Suspended`] both deactivate and are
/// logged as one `deactivate` entry, but only the suspension stamps
/// [`suspended_at`](User::suspended_at), which is what the account entities
/// and the federated actor read `suspended` from. [`Standing::Active`]
/// reactivates and clears it.
///
/// # Errors
///
/// [`AppError::Database`](crate::error::AppError) from the writes and the log.
pub async fn set_standing(
    state: &AppState,
    actor: &User,
    accounts: &[User],
    standing: Standing,
) -> AppResult<Vec<User>> {
    let active = matches!(standing, Standing::Active);
    let mut moved = Vec::with_capacity(accounts.len());
    for account in accounts {
        if let Some(updated) = admin::set_standing(&state.db, account.id, standing).await? {
            moved.push(updated);
        }
    }
    let logged = if active {
        moderation_log::activate(&state.db, actor, &moved).await?
    } else {
        moderation_log::deactivate(&state.db, actor, &moved).await?
    };
    debug!(
        log = %logged.id,
        moved = moved.len(),
        active,
        "admin changed account activations"
    );
    let event = if active {
        AccountEvent::Unsuspended
    } else {
        AccountEvent::Suspended
    };
    for account in &moved {
        dispatch(state, account, event).await;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        models::moderation_log::{LogListParams, list},
        test_fixtures::local_user,
    };

    /// Every entry of the log, newest first.
    async fn entries(state: &AppState) -> Vec<moderation_log::ModerationLog> {
        let params = LogListParams {
            page: 1,
            page_size: 50,
            user_id: None,
            search: None,
            start_date: None,
            end_date: None,
        };
        list(&state.db, &params).await.expect("the log lists").items
    }

    #[tokio::test]
    async fn two_deactivated_accounts_leave_one_entry_naming_both() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let actor = local_user(&state.db, "root").await;
        let alice = local_user(&state.db, "alice").await;
        let bob = local_user(&state.db, "bob").await;

        let moved = set_activation(&state, &actor, &[alice.clone(), bob.clone()], false)
            .await
            .expect("the deactivation runs");

        assert_eq!(
            moved.iter().map(|user| user.id).collect::<Vec<_>>(),
            [alice.id, bob.id],
            "the updated rows come back in order"
        );
        assert!(
            moved.iter().all(|user| !user.is_active.get()),
            "both are inactive"
        );
        let log = entries(&state).await;
        assert_eq!(log.len(), 1, "one entry for the whole write");
        assert_eq!(log[0].action, "deactivate", "named for the move");
        assert_eq!(log[0].subject_ids, [alice.id, bob.id], "over both rows");
        assert_eq!(
            log[0].message, "@root deactivated users: @alice, @bob",
            "in Pleroma's wording"
        );
    }

    #[tokio::test]
    async fn activating_an_already_active_account_still_returns_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let actor = local_user(&state.db, "root").await;
        let alice = local_user(&state.db, "alice").await;
        assert!(alice.is_active.get(), "the fixture starts active");

        let moved = set_activation(&state, &actor, std::slice::from_ref(&alice), true)
            .await
            .expect("the activation runs");

        assert_eq!(moved.len(), 1, "the row is handed back");
        assert_eq!(moved[0].id, alice.id, "the same account");
        assert!(moved[0].is_active.get(), "and it is active");
        let log = entries(&state).await;
        assert_eq!(log.len(), 1, "one entry");
        assert_eq!(log[0].action, "activate", "an activation");
        assert_eq!(log[0].subject_ids, [alice.id], "naming the account");
    }

    #[tokio::test]
    async fn a_row_that_is_gone_is_not_among_the_answer() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let actor = local_user(&state.db, "root").await;
        let alice = local_user(&state.db, "alice").await;
        let mut ghost = local_user(&state.db, "ghost").await;
        ghost.id = uuid::Uuid::now_v7();

        let moved = set_activation(&state, &actor, &[ghost, alice.clone()], false)
            .await
            .expect("the deactivation runs");

        assert_eq!(
            moved.iter().map(|user| user.id).collect::<Vec<_>>(),
            [alice.id],
            "only the row that exists is answered"
        );
    }
}
