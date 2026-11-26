//! The admin writes that answer with the moved row: [`set_suggested`],
//! [`set_active`], [`set_approved`] and the reject [`delete_user`].

use sqlx::PgPool;
use uuid::Uuid;

use super::super::{Flag, User};
use crate::error::AppResult;

/// The `UPDATE` behind [`set_suggested`]: writes the pool flag, refreshes
/// `updated_at` and returns the stored row.
const SET_SUGGESTED_SQL: &str = "UPDATE users SET is_suggested = $2, updated_at = now() \
                                 WHERE id = $1 RETURNING *";

/// Stores `suggested` as user `id`'s [`is_suggested`](User::is_suggested)
/// flag and returns the updated row.
///
/// The write of the admin `PATCH /users/suggest` and `PATCH
/// /users/unsuggest` pair, whose answer is the affected accounts as the
/// admin entity reads them, so the fresh row comes back for the endpoint to
/// render.
///
/// `None` when `id` names no row: the account went away between the
/// endpoint's lookup and this write, and it is simply not among the
/// answered users.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn set_suggested(pool: &PgPool, id: Uuid, suggested: bool) -> AppResult<Option<User>> {
    let updated = sqlx::query_as::<_, User>(SET_SUGGESTED_SQL)
        .bind(id)
        .bind(Flag::new(suggested))
        .fetch_optional(pool)
        .await?;
    Ok(updated)
}

/// The `UPDATE` of [`Standing::Suspended`]: deactivates and stamps the
/// suspension, which keeps the time it began when it is written again. The
/// disable stays as it was.
const SUSPEND_SQL: &str = "UPDATE users
       SET local_deactivated = true,
           is_active = false,
           suspended_at = COALESCE(suspended_at, now()),
           updated_at = now()
     WHERE id = $1 RETURNING *";

/// The `UPDATE` of [`Standing::Unsuspended`]: lifts the local suspension. A
/// remote one keeps the account inactive (WBE-873) and the disable stays.
const UNSUSPEND_SQL: &str = "UPDATE users
       SET local_deactivated = false,
           is_active = NOT remote_suspended,
           suspended_at = NULL,
           updated_at = now()
     WHERE id = $1 RETURNING *";

/// The `UPDATE` of [`Standing::Active`]: [`UNSUSPEND_SQL`] and the disable
/// lifted too.
const ACTIVATE_SQL: &str = "UPDATE users
       SET local_deactivated = false,
           is_active = NOT remote_suspended,
           suspended_at = NULL,
           disabled_at = NULL,
           updated_at = now()
     WHERE id = $1 RETURNING *";

/// The `UPDATE` of [`Standing::Disabled`]: stamps the disable, which keeps
/// the time it began when it is written again, and nothing else.
const DISABLE_SQL: &str = "UPDATE users
       SET disabled_at = COALESCE(disabled_at, now()),
           updated_at = now()
     WHERE id = $1 RETURNING *";

/// The `UPDATE` of [`Standing::Enabled`]: lifts the disable and nothing else.
const ENABLE_SQL: &str = "UPDATE users
       SET disabled_at = NULL,
           updated_at = now()
     WHERE id = $1 RETURNING *";

/// The local standing an admin write gives an account (WBE-878, WBE-883).
///
/// A suspension and a disable are two flags that never touch each other:
/// suspending or unsuspending leaves a disable in place, and enabling leaves
/// a suspension in place.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Standing {
    /// Neither disabled nor suspended: lifts both.
    Active,
    /// Disabled: only [`disabled_at`](User::disabled_at) is stamped. The
    /// account stays active, visible and federated; its owner cannot sign in.
    Disabled,
    /// The disable lifted; a suspension stays.
    Enabled,
    /// Suspended: [`is_active`](User::is_active) off and
    /// [`suspended_at`](User::suspended_at) stamped.
    Suspended,
    /// The local suspension lifted; a disable stays.
    Unsuspended,
}

impl Standing {
    /// The write this standing is.
    const fn sql(self) -> &'static str {
        match self {
            Self::Active => ACTIVATE_SQL,
            Self::Disabled => DISABLE_SQL,
            Self::Enabled => ENABLE_SQL,
            Self::Suspended => SUSPEND_SQL,
            Self::Unsuspended => UNSUSPEND_SQL,
        }
    }

    /// Whether the standing reverses something (and is logged as an
    /// activation) rather than imposing it.
    #[must_use]
    pub const fn lifts(self) -> bool {
        matches!(self, Self::Active | Self::Enabled | Self::Unsuspended)
    }

    /// Whether the write changes the account's federated face: only a
    /// suspension and its lifting do (a disable federates nothing).
    #[must_use]
    pub const fn federates(self) -> bool {
        matches!(self, Self::Active | Self::Suspended | Self::Unsuspended)
    }
}

/// Stores `standing` as user `id`'s local standing and returns the updated
/// row (WBE-878, WBE-883).
///
/// Only the local origin moves. [`Standing::Suspended`] sets
/// `local_deactivated` and stamps [`suspended_at`](User::suspended_at);
/// [`Standing::Unsuspended`] clears both, and [`is_active`](User::is_active)
/// comes back unless the account's origin server has it suspended
/// (`remote_suspended`), which a local activation does not clear (WBE-873).
/// [`Standing::Disabled`] and [`Standing::Enabled`] write
/// [`disabled_at`](User::disabled_at) alone.
///
/// `None` when `id` names no row.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn set_standing(pool: &PgPool, id: Uuid, standing: Standing) -> AppResult<Option<User>> {
    let updated = sqlx::query_as::<_, User>(standing.sql())
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(updated)
}

/// Stores `active` as user `id`'s local activation and returns the updated
/// row (WBE-196).
///
/// [`set_standing`] with [`Standing::Active`] for `true` and
/// [`Standing::Suspended`] for `false`: the Pleroma deactivation is the
/// suspension of Mastodon's vocabulary, and the actor federates as
/// `suspended` (WBE-878). A remote suspension is not cleared by a local
/// activation (WBE-873).
///
/// The write of the admin `PATCH /users/activate`, `PATCH /users/deactivate`
/// and `PATCH /users/{nickname}/toggle_activation` trio, whose answers carry
/// the moved accounts as admin entities — so the fresh row comes back for
/// the endpoint to render, the shape [`set_suggested`] keeps.
///
/// The flag alone: unlike [`super::set_activation`](super::super::set_activation), the user-facing write
/// of `POST /api/pleroma/disable_account` (which answers `()` and runs on
/// any executor), this one writes no other column — a deactivation started
/// here revokes no token, stamps no deletion and publishes nothing. It is
/// the flag the login gate and the bearer-token gate read.
///
/// `None` when `id` names no row: the account went away between the
/// endpoint's lookup and this write, and it is simply not among the
/// answered users.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn set_active(pool: &PgPool, id: Uuid, active: bool) -> AppResult<Option<User>> {
    let standing = if active {
        Standing::Active
    } else {
        Standing::Suspended
    };
    set_standing(pool, id, standing).await
}

/// The `UPDATE` behind [`set_approved`]: clears the staff gate, refreshes
/// `updated_at` and returns the stored row.
const SET_APPROVED_SQL: &str = "UPDATE users SET approval_pending = false, updated_at = now() \
                                WHERE id = $1 RETURNING *";

/// Clears user `id`'s [`approval_pending`](User::approval_pending) flag and
/// returns the updated row (WBE-196).
///
/// The write of the admin `PATCH /users/approve` — Pleroma's
/// `User.approve` setting `is_approved: true`, this schema's opposite
/// column being cleared (see the module docs' criteria note) — and of the
/// admin `POST /users` bypass, which creates the account through the
/// registration path and lifts the flag off the row that path stored while
/// `REGISTRATIONS_APPROVAL_REQUIRED` is on. No other column moves: the
/// approval is a staff decision about the account, not about its address
/// ([`super::confirmation::set_confirmed`] is that write) and it mints no
/// credential — the approved account signs in with its own password.
///
/// `None` when `id` names no row.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
///
/// [`super::confirmation::set_confirmed`]: super::super::confirmation::set_confirmed
pub async fn set_approved(pool: &PgPool, id: Uuid) -> AppResult<Option<User>> {
    let updated = sqlx::query_as::<_, User>(SET_APPROVED_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(updated)
}

/// The `DELETE` behind [`delete_user`]: takes the row itself, whose
/// cascades go through every dependent row the schema hangs off it.
const DELETE_SQL: &str = "DELETE FROM users WHERE id = $1";

/// Removes user `id`'s row outright and reports whether one went (WBE-213).
///
/// The write behind the Mastodon admin API's
/// `POST /api/v1/admin/accounts/{id}/reject`: Pleroma's `User.reject` runs
/// `User.delete`, and its `maybe_delete_from_db` clause drops the row of a
/// local account that was never confirmed or approved instead of only
/// purging it — which is exactly the account this route refuses anything
/// else than, so the row goes here too. The tokens, authorizations,
/// follows, notifications and every other dependent row leave with it on
/// their `ON DELETE CASCADE` keys; the `moderation_log` entry the endpoint
/// writes names the account by nickname, its `actor_id` key being `SET
/// NULL` and its subject arrays carrying no key at all.
///
/// Unlike [`super::mark_deleted`](super::super::mark_deleted) — the self-service deletion's stamp, which
/// keeps the row so its `ActivityPub` actor stays resolvable and lets the
/// `users.delete` sweep purge it — this takes the row in one statement and
/// queues nothing. An account that has content worth sweeping is the delete
/// endpoint's subject, not this one's.
///
/// `false` when `id` names no row.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError):
/// the one reachable here is a dependent row whose key does not cascade
/// (a report assigned to this account), which refuses the delete.
pub async fn delete_user(pool: &PgPool, id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query(DELETE_SQL).bind(id).execute(pool).await?;
    Ok(deleted.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::{
        follow,
        user::{NewUser, get_by_id, insert},
    };

    /// The pool flag lands and the fresh row comes back with it — the pair
    /// the suggest endpoint answers its entities from — and an unknown id
    /// answers `None` rather than inventing a user.
    #[tokio::test]
    async fn set_suggested_writes_the_flag_and_returns_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser::local("alice", "https://test.example/users/alice"),
        )
        .await
        .expect("the fixture inserts");

        let picked = set_suggested(&pool, alice.id, true)
            .await
            .expect("the suggest runs")
            .expect("the row comes back");
        assert!(picked.is_suggested.get(), "the account is in the pool");
        assert_eq!(picked.nickname, "alice", "the whole row came back");

        let dropped = set_suggested(&pool, alice.id, false)
            .await
            .expect("the unsuggest runs")
            .expect("the row comes back");
        assert!(!dropped.is_suggested.get(), "and it is out of the pool");
        assert!(
            set_suggested(&pool, Uuid::now_v7(), true)
                .await
                .expect("a miss writes nothing")
                .is_none(),
            "an unknown id matches no row"
        );
    }

    /// The lifecycle pair each move their own flag and hand the fresh row
    /// back: `set_active` touches nothing a deactivation must not touch
    /// (the address, the approval gate, the deletion stamp), and
    /// `set_approved` clears only the staff gate. Unknown ids answer
    /// `None`, the way [`set_suggested`] does.
    #[tokio::test]
    async fn the_activation_and_approval_writes_move_their_own_flag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser {
                is_active: Flag::new(false),
                approval_pending: Flag::new(true),
                ..NewUser::local("alice", "https://test.example/users/alice")
            },
        )
        .await
        .expect("the fixture inserts");

        let woken = set_active(&pool, alice.id, true)
            .await
            .expect("the activation runs")
            .expect("the row comes back");
        assert!(woken.is_active.get(), "the flag moved");
        assert!(
            woken.approval_pending.get(),
            "the approval gate stayed shut — activation is not approval"
        );
        assert!(woken.deleted_at.is_none(), "and no deletion was stamped");

        let cleared = set_approved(&pool, alice.id)
            .await
            .expect("the approval runs")
            .expect("the row comes back");
        assert!(
            !cleared.approval_pending.get(),
            "the pending flag is cleared for good"
        );
        assert!(cleared.is_active.get(), "the activation stayed");

        let asleep = set_active(&pool, alice.id, false)
            .await
            .expect("the deactivation runs")
            .expect("the row comes back");
        assert!(!asleep.is_active.get(), "and the flag moves back");

        assert!(
            set_active(&pool, Uuid::now_v7(), true)
                .await
                .expect("a miss writes nothing")
                .is_none(),
            "an unknown id matches no row"
        );
        assert!(
            set_approved(&pool, Uuid::now_v7())
                .await
                .expect("a miss writes nothing")
                .is_none(),
            "and neither does it for the approval write"
        );
    }

    /// A suspension stamps `suspended_at`, a disable leaves it empty, and
    /// the active standing clears it; a remote suspension keeps the account
    /// inactive through a local activation (WBE-878).
    #[tokio::test]
    async fn set_standing_tells_a_suspension_from_a_disable() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser::local("alice", "https://test.example/users/alice"),
        )
        .await
        .expect("the fixture inserts");
        assert!(!alice.is_suspended(), "a fresh account is not suspended");

        let suspended = set_standing(&pool, alice.id, Standing::Suspended)
            .await
            .expect("the suspension runs")
            .expect("the row comes back");
        assert!(!suspended.is_active.get(), "the account is inactive");
        assert!(suspended.suspended_at.is_some(), "and stamped");
        assert!(suspended.is_suspended(), "so it reads suspended");

        let lifted = set_standing(&pool, alice.id, Standing::Unsuspended)
            .await
            .expect("the unsuspension runs")
            .expect("the row comes back");
        assert!(lifted.is_active.get(), "the unsuspension wakes it");
        assert!(lifted.suspended_at.is_none(), "and clears the stamp");

        let disabled = set_standing(&pool, alice.id, Standing::Disabled)
            .await
            .expect("the disable runs")
            .expect("the row comes back");
        assert!(disabled.is_active.get(), "a disable leaves it active");
        assert!(disabled.suspended_at.is_none(), "and unstamped");
        assert!(disabled.disabled_at.is_some(), "but it is disabled");
        assert!(!disabled.is_suspended(), "so it reads disabled only");
    }

    /// The disable and the suspension are two flags: each reversal lifts only
    /// its own, the active standing lifts both, and a remote suspension
    /// outlives a local activation (WBE-883).
    #[tokio::test]
    async fn the_disable_and_the_suspension_are_lifted_separately() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser::local("alice", "https://test.example/users/alice"),
        )
        .await
        .expect("the fixture inserts");
        let _disabled = set_standing(&pool, alice.id, Standing::Disabled)
            .await
            .expect("the disable runs");

        let resuspended = set_standing(&pool, alice.id, Standing::Suspended)
            .await
            .expect("the second suspension runs")
            .expect("the row comes back");
        assert!(resuspended.suspended_at.is_some(), "suspended again");
        assert!(resuspended.disabled_at.is_some(), "the disable stays");
        let unsuspended = set_standing(&pool, alice.id, Standing::Unsuspended)
            .await
            .expect("the unsuspension runs")
            .expect("the row comes back");
        assert!(unsuspended.is_active.get(), "the account is awake");
        assert!(unsuspended.disabled_at.is_some(), "and still disabled");
        let enabled = set_standing(&pool, alice.id, Standing::Enabled)
            .await
            .expect("the enable runs")
            .expect("the row comes back");
        assert!(
            enabled.disabled_at.is_none(),
            "the enable lifts the disable"
        );
        let suspended_again = set_standing(&pool, alice.id, Standing::Suspended)
            .await
            .expect("the suspension runs")
            .expect("the row comes back");
        let still = set_standing(&pool, alice.id, Standing::Enabled)
            .await
            .expect("the enable runs")
            .expect("the row comes back");
        assert!(suspended_again.suspended_at.is_some(), "suspended");
        assert!(
            !still.is_active.get() && still.suspended_at.is_some(),
            "an enable leaves a suspension in force"
        );
        let both = set_standing(&pool, alice.id, Standing::Disabled)
            .await
            .expect("the disable runs")
            .expect("the row comes back");
        let again = set_standing(&pool, alice.id, Standing::Active)
            .await
            .expect("the activation runs")
            .expect("the row comes back");
        assert!(both.disabled_at.is_some(), "disabled and suspended");
        assert!(again.is_active.get(), "the activation wakes it");
        assert!(again.suspended_at.is_none(), "clears the stamp");
        assert!(again.disabled_at.is_none(), "and the disable");
    }

    /// A remote suspension outlives a local activation, and an unknown id
    /// matches no row.
    #[tokio::test]
    async fn a_remote_suspension_outlives_a_local_activation() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser::local("alice", "https://test.example/users/alice"),
        )
        .await
        .expect("the fixture inserts");

        let flagged = sqlx::query("UPDATE users SET remote_suspended = true WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the remote flag writes")
            .rows_affected();
        assert_eq!(flagged, 1, "one row");
        let held = set_standing(&pool, alice.id, Standing::Active)
            .await
            .expect("the activation runs")
            .expect("the row comes back");
        assert!(!held.is_active.get(), "a remote suspension stays in force");
        assert!(held.is_suspended(), "and the account reads suspended");

        assert!(
            set_standing(&pool, Uuid::now_v7(), Standing::Suspended)
                .await
                .expect("a miss writes nothing")
                .is_none(),
            "an unknown id matches no row"
        );
    }

    /// The rejection's delete takes the row itself and everything the schema
    /// hangs off it: the follow it held goes on the cascade, the account it
    /// followed stays, and a second delete — like an id that never existed —
    /// reports that no row matched.
    #[tokio::test]
    async fn delete_user_takes_the_row_and_its_cascades() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let follower = insert(
            &pool,
            &NewUser::local("alice", "https://test.example/users/alice"),
        )
        .await
        .expect("the follower inserts");
        let other = insert(
            &pool,
            &NewUser::local("bob", "https://test.example/users/bob"),
        )
        .await
        .expect("the followed account inserts");
        let _placed: follow::Follow =
            follow::follow(&pool, "https://test.example", &follower, &other)
                .await
                .expect("the follow is placed");

        assert!(
            delete_user(&pool, follower.id)
                .await
                .expect("the delete runs"),
            "the named row went"
        );
        assert!(
            get_by_id(&pool, follower.id)
                .await
                .expect("the read runs")
                .is_none(),
            "and nothing answers to that id any more"
        );
        let held =
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM follows WHERE follower_id = $1")
                .bind(follower.id)
                .fetch_one(&pool)
                .await
                .expect("the follow count reads");
        assert_eq!(held, 0, "the follow left with the row");
        assert!(
            get_by_id(&pool, other.id)
                .await
                .expect("the read runs")
                .is_some(),
            "the account it followed is untouched"
        );
        assert!(
            !delete_user(&pool, follower.id).await.expect("a miss runs"),
            "the second delete matches no row"
        );
        assert!(
            !delete_user(&pool, Uuid::now_v7())
                .await
                .expect("an unknown id runs"),
            "and neither does an id that never existed"
        );
    }
}
