//! The suggestion pool and its dismissals: the accounts staff pick for
//! `GET /api/v1/suggestions` and `GET /api/v2/suggestions`, and the
//! per-viewer record behind `DELETE /api/v1/suggestions/{account_id}`.
//!
//! Reference: Pleroma's `SuggestionController`
//! (`lib/pleroma/web/mastodon_api/controllers/suggestion_controller.ex`)
//! over `User.Query.build(%{is_suggested: true, invisible: false})` and the
//! `:suggestion_dismiss` edge of `Pleroma.UserRelationship`
//! (`lib/pleroma/user_relationship.ex`). Pleroma stores a dismissal as one
//! more relationship type in its shared table; here it is a table of its
//! own, keyed by the `(user_id, target_id)` pair the endpoint works on, and
//! the pool is the `users.is_suggested` column the admin API (a later
//! ticket) sets.
//!
//! A dismissal is private to the viewer who made it: it hides the account
//! from *their* suggestions and clears nothing about anyone else, and it
//! never touches the account's own `is_suggested` flag. Deleting either
//! account takes the dismissal through the foreign key's cascade, and
//! [`dismiss`] is idempotent on the key, so dismissing twice is one row.
//!
//! [`suggested_for`] is the pool as one viewer sees it: suggested, local,
//! active, and none of the accounts they already follow, block, mute or
//! dismissed — the four exclusions Pleroma's controller stacks on its
//! query, here in one statement.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, models::user::User};

/// [`dismiss`]: one viewer's dismissal of one suggested account. `ON
/// CONFLICT DO NOTHING` on the `(user_id, target_id)` key is the
/// idempotence: a pair that is already dismissed keeps its row and a repeat
/// call writes nothing.
const DISMISS_SQL: &str = "INSERT INTO dismissed_suggestions (user_id, target_id)
     VALUES ($1, $2)
ON CONFLICT (user_id, target_id) DO NOTHING";

/// [`dismissed_ids`]: every account one viewer dismissed, ordered by the
/// dismissed account's id. The table keys on the pair and carries no
/// timestamp of its own, so this is a stability promise rather than a
/// creation order: callers ask set membership, and [`suggested_for`]
/// filters the same set inside its own statement.
const ALL_IDS_SQL: &str = "SELECT target_id FROM dismissed_suggestions
     WHERE user_id = $1 ORDER BY target_id";

/// [`suggested_for`]: the suggestion pool as one viewer sees it — Pleroma's
/// `User.Query.build(%{is_suggested: true, invisible: false})` with its
/// controller's exclusions.
///
/// `is_active` stands in for Pleroma's `invisible: false`, `local` for a
/// pool this server curates by hand (a remote mirror is never suggested),
/// and the four `NOT EXISTS` arms are `exclude_user`,
/// `exclude_relationships([:block, :mute, :suggestion_dismiss])` and
/// `exclude_following` folded into one statement. The follow arm is
/// accepted-only, as Pleroma's `state == :follow_accept` is; the mute arm
/// skips a row whose `expires_at` has been reached, which every read of
/// `mutes` treats as absent. Newest account first (`UUIDv7` text sorts in
/// creation order, decision 4), which is the order the listing walks.
const SUGGESTED_SQL: &str = "SELECT * FROM users u
      WHERE u.is_suggested AND u.local AND u.is_active AND u.id <> $1
        AND NOT EXISTS (SELECT 1 FROM follows f
                         WHERE f.follower_id = $1 AND f.followee_id = u.id
                           AND f.state = 'accepted')
        AND NOT EXISTS (SELECT 1 FROM blocks b
                         WHERE b.blocker_id = $1 AND b.blocked_id = u.id)
        AND NOT EXISTS (SELECT 1 FROM mutes m
                         WHERE m.muter_id = $1 AND m.muted_id = u.id
                           AND (m.expires_at IS NULL OR m.expires_at > now()))
        AND NOT EXISTS (SELECT 1 FROM dismissed_suggestions d
                         WHERE d.user_id = $1 AND d.target_id = u.id)
   ORDER BY u.id DESC
      LIMIT $2";

/// Records that `user_id` dismissed the suggestion of `target_id`,
/// idempotently.
///
/// A pair that is already dismissed keeps its row and a repeat call writes
/// nothing; a target that is not in the pool at all is still recorded, as in
/// Pleroma, where the dismissal edge is created whatever the account's own
/// `is_suggested` flag says.
///
/// # Errors
///
/// Any database failure (connection, encoding, constraint) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn dismiss(pool: &PgPool, user_id: Uuid, target_id: Uuid) -> AppResult<()> {
    sqlx::query(DISMISS_SQL)
        .bind(user_id)
        .bind(target_id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The ids of the accounts `user_id` dismissed, by target id.
///
/// The whole set in one statement, for the callers that ask membership of
/// it; [`suggested_for`] needs no read of it, filtering the same set inside
/// its own query. A viewer who dismissed nothing answers the empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn dismissed_ids(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(ALL_IDS_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(target_id,)| target_id).collect())
}

/// The accounts to suggest to `viewer_id`: `limit` of the suggested, local,
/// active accounts, newest first.
///
/// The listing skips the viewer's own account and everyone they follow,
/// block, mute or dismissed (see `SUGGESTED_SQL` in this module for the
/// states those exclusions read).
///
/// A viewer with nothing left to see answers the empty `Vec`, and so does an
/// instance whose pool is empty: the listing is never an error.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn suggested_for(pool: &PgPool, viewer_id: Uuid, limit: i64) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(SUGGESTED_SQL)
        .bind(viewer_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::{
            block, follow,
            mute::{self, MuteOptions},
            user::{Flag, NewUser, insert},
        },
        test_fixtures::{local_new, local_user as user},
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = TEST_BASE_URL;

    /// Inserts a local user named `nickname` and puts it in the pool.
    async fn suggested(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                is_suggested: Flag::new(true),
                ..local_new(nickname)
            },
        )
        .await
        .expect("the suggested fixture inserts")
    }

    /// Inserts a mirror of `nickname` on `remote.example`, flagged into the
    /// pool the way an admin flag would leave it: suggested and active, but
    /// not local.
    async fn suggested_remote(pool: &PgPool, nickname: &str) -> User {
        let ap_id = format!("https://remote.example/users/{nickname}");
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id,
                domain: Some("remote.example".to_owned()),
                is_suggested: Flag::new(true),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// The ids of `users`, in the order they came back.
    fn ids(users: &[User]) -> Vec<Uuid> {
        users.iter().map(|found| found.id).collect()
    }

    /// The pool's ids as storage orders them, newest first — the expected
    /// order for the listing assertions, read straight from the table so
    /// same-millisecond `UUIDv7`s cannot make it depend on insert timing
    /// (decision 4).
    async fn pool_order(pool: &PgPool) -> Vec<Uuid> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM users WHERE is_suggested AND local AND is_active ORDER BY id DESC",
        )
        .fetch_all(pool)
        .await
        .expect("the stored order reads")
    }

    /// Records `follower`'s accepted follow of `target`.
    async fn follow_pair(pool: &PgPool, follower: &User, target: &User) {
        let row = follow::follow(pool, BASE, follower, target)
            .await
            .expect("the fixture follow records");
        assert_eq!(
            row.state,
            follow::FollowState::Accepted,
            "an open local account answers on the spot"
        );
    }

    /// Moves `target`'s mute row into the past: the state a lapsed mute
    /// that the sweep has not yet deleted has.
    async fn lapse_mute(pool: &PgPool, muter: &User, target: &User) {
        let lapsed = sqlx::query(
            "UPDATE mutes SET expires_at = now() - interval '1 hour'
              WHERE muter_id = $1 AND muted_id = $2",
        )
        .bind(muter.id)
        .bind(target.id)
        .execute(pool)
        .await
        .expect("the fixture mute lapses");
        assert_eq!(lapsed.rows_affected(), 1, "the fixture mute was there");
    }

    #[tokio::test]
    async fn a_dismissal_is_written_and_read_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;

        dismiss(&pool, alice.id, bob.id)
            .await
            .expect("the first dismissal stores");
        assert_eq!(
            dismissed_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "the target comes back"
        );
    }

    #[tokio::test]
    async fn dismissing_twice_is_one_row_and_each_viewer_has_their_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let carol = user(&pool, "carol").await;
        let bob = user(&pool, "bob").await;
        dismiss(&pool, alice.id, bob.id).await.unwrap();

        dismiss(&pool, alice.id, bob.id)
            .await
            .expect("the repeat dismissal is not an error");
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM dismissed_suggestions")
            .fetch_one(&pool)
            .await
            .expect("the count reads");
        assert_eq!(rows, 1, "the second call wrote nothing");

        assert!(
            dismissed_ids(&pool, carol.id).await.unwrap().is_empty(),
            "carol has nothing of alice's dismissals"
        );
        dismiss(&pool, carol.id, bob.id)
            .await
            .expect("the same target dismissed by another account is a second row");
        assert_eq!(
            dismissed_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "and it left alice's dismissal alone"
        );
    }

    #[tokio::test]
    async fn deleting_either_side_takes_the_dismissal() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        dismiss(&pool, alice.id, bob.id).await.unwrap();

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the fixture target deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");
        assert!(
            dismissed_ids(&pool, alice.id).await.unwrap().is_empty(),
            "the dismissal of the deleted account went with it"
        );
    }

    #[tokio::test]
    async fn a_dismissal_for_no_stored_account_fails() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // The foreign key is the table's only guard: the endpoint hands ids
        // that name live users, and a fabricated one is a database error,
        // never a silently stored orphan.
        let alice = user(&pool, "alice").await;
        let nobody = Uuid::now_v7();
        let err = dismiss(&pool, alice.id, nobody)
            .await
            .expect_err("the foreign key refuses");
        assert!(
            matches!(err, AppError::Database(_)),
            "a constraint failure is the database variant: {err}"
        );
        assert!(
            dismissed_ids(&pool, alice.id).await.unwrap().is_empty(),
            "nothing was stored"
        );
    }

    #[tokio::test]
    async fn the_pool_is_suggested_local_active_accounts_newest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let first = suggested(&pool, "first").await;
        let second = suggested(&pool, "second").await;
        // Out of the pool: never flagged, flagged but deactivated, flagged
        // but remote, and the viewer themself.
        let plain = user(&pool, "plain").await;
        let gone = suggested(&pool, "gone").await;
        let deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(gone.id)
            .execute(&pool)
            .await
            .expect("the fixture deactivates");
        assert_eq!(deactivated.rows_affected(), 1, "and it was one row");
        let mirror = suggested_remote(&pool, "mirror").await;

        let seen = ids(&suggested_for(&pool, alice.id, 40).await.unwrap());
        assert_eq!(
            seen,
            pool_order(&pool).await,
            "the whole pool, in the order storage keys it in"
        );
        assert_eq!(seen.len(), 2, "the two live local picks and nothing else");
        for skipped in [plain.id, gone.id, mirror.id, alice.id] {
            assert!(!seen.contains(&skipped), "{skipped} is not pool material");
        }
        let other_viewer = user(&pool, "other-viewer").await;
        assert_eq!(
            ids(&suggested_for(&pool, other_viewer.id, 40).await.unwrap()),
            seen,
            "the pool itself is the same for every viewer"
        );
        assert!(
            !ids(&suggested_for(&pool, first.id, 40).await.unwrap()).contains(&first.id),
            "an account in the pool is out of its own listing"
        );
        assert!(
            ids(&suggested_for(&pool, second.id, 40).await.unwrap()).contains(&first.id),
            "and it still sees the rest of the pool"
        );
    }

    #[tokio::test]
    async fn the_limit_bounds_the_pool() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        for nickname in ["one", "two", "three"] {
            let pick = suggested(&pool, nickname).await;
            assert!(pick.is_suggested.get(), "{nickname} is in the pool");
        }
        let stored = pool_order(&pool).await;
        assert_eq!(stored.len(), 3, "three fixture picks");

        let page = ids(&suggested_for(&pool, alice.id, 2).await.unwrap());
        assert_eq!(page, stored[..2], "the caller's limit is the page size");
        let all = ids(&suggested_for(&pool, alice.id, 40).await.unwrap());
        assert_eq!(all, stored, "and a roomy limit answers the whole pool");
    }

    #[tokio::test]
    async fn a_viewer_never_sees_theyself() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = suggested(&pool, "alice").await;
        let other = suggested(&pool, "other").await;

        assert_eq!(
            ids(&suggested_for(&pool, alice.id, 40).await.unwrap()),
            vec![other.id],
            "the caller is out of their own pool"
        );
    }

    #[tokio::test]
    async fn followed_blocked_muted_and_dismissed_accounts_are_excluded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let following = suggested(&pool, "following").await;
        let blocked = suggested(&pool, "blocked").await;
        let muted = suggested(&pool, "muted").await;
        let dismissed = suggested(&pool, "dismissed").await;
        let kept = suggested(&pool, "kept").await;

        follow_pair(&pool, &alice, &following).await;
        let blocked_row = block::block(&pool, BASE, &alice, &blocked)
            .await
            .expect("the fixture block writes");
        assert_eq!(
            blocked_row.blocked_id, blocked.id,
            "the fixture block is alice's on `blocked`"
        );
        let muted_row = mute::mute(&pool, &alice, &muted, MuteOptions::default())
            .await
            .expect("the fixture mute writes");
        assert_eq!(
            muted_row.muted_id, muted.id,
            "the fixture mute is alice's on `muted`"
        );
        dismiss(&pool, alice.id, dismissed.id).await.unwrap();

        assert_eq!(
            ids(&suggested_for(&pool, alice.id, 40).await.unwrap()),
            vec![kept.id],
            "only the account nobody has any relationship with is left"
        );
    }

    #[tokio::test]
    async fn a_pending_follow_and_an_expired_mute_do_not_exclude() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        // A locked account answers nothing: alice's follow of it stays
        // pending, which is not a follow for the exclusion's purpose.
        let locked = insert(
            &pool,
            &NewUser {
                locked: Flag::new(true),
                is_suggested: Flag::new(true),
                ..local_new("locked")
            },
        )
        .await
        .expect("the locked fixture inserts");
        let request = follow::follow(&pool, BASE, &alice, &locked)
            .await
            .expect("the pending request records");
        assert_eq!(
            request.state,
            follow::FollowState::Pending,
            "a locked account leaves the request unanswered"
        );
        // A mute whose expiry has passed is a row no read counts as a mute.
        let lapsed = suggested(&pool, "lapsed").await;
        let timed = mute::mute(
            &pool,
            &alice,
            &lapsed,
            MuteOptions {
                notifications: true,
                expires_in: Some(60),
            },
        )
        .await
        .expect("the timed mute writes");
        assert_eq!(
            timed.muted_id, lapsed.id,
            "the timed mute is alice's on `lapsed`"
        );
        lapse_mute(&pool, &alice, &lapsed).await;

        let seen = ids(&suggested_for(&pool, alice.id, 40).await.unwrap());
        assert!(
            seen.contains(&locked.id),
            "an unanswered request keeps the account suggestable"
        );
        assert!(
            seen.contains(&lapsed.id),
            "a lapsed mute is no longer a mute: the account is back"
        );
    }

    #[tokio::test]
    async fn an_empty_pool_answers_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        assert!(
            suggested_for(&pool, alice.id, 40)
                .await
                .expect("an empty listing is no error")
                .is_empty(),
            "nothing flagged, nothing to suggest"
        );
        assert!(
            dismissed_ids(&pool, alice.id).await.unwrap().is_empty(),
            "and nothing dismissed"
        );
    }
}
