//! Removing a relationship from the followed account's side:
//! [`remove_follower`] takes one follower back, [`sever_between`] takes the
//! open rows of a newly blocked pair, and [`sever_with_domain`] takes every row
//! between one account and a blocked domain. Every removed accepted row steps
//! its counters down through [`step_counters`].

use std::collections::HashMap;

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{error::AppResult, models::user::User};

use super::{
    counters::step_counters,
    types::{Follow, FollowState},
};

/// [`remove_follower`]: the follower is gone, and only an *accepted* row ever
/// counted, so the removed row is what decides whether the counters step down
/// and anything else in the table stays exactly as it was.
const REMOVE_FOLLOWER_SQL: &str = "DELETE FROM follows
      WHERE follower_id = $1 AND followee_id = $2 AND state = 'accepted'
  RETURNING *";

/// [`sever_between`]: the open relationships of a pair in *both* directions,
/// which is what a block severs — the same `pending`/`accepted` states
/// [`UNFOLLOW_SQL`] takes back (a mirrored `rejected` row is nobody's active
/// relationship and stays where it is), stated once for the ordered pair and
/// once for its reverse. The removed rows come back, because only an accepted
/// one ever counted and the caller fixes the counters from them.
const SEVER_BETWEEN_SQL: &str = "DELETE FROM follows
      WHERE state IN ('pending', 'accepted')
        AND ((follower_id = $1 AND followee_id = $2)
          OR (follower_id = $2 AND followee_id = $1))
   RETURNING *";

/// [`sever_with_domain`]'s incoming side: every follow of `$1` by a remote
/// account on the domain `$2`, in any state, so a domain block leaves no row
/// between the pair of sides behind.
const SEVER_INCOMING_BY_DOMAIN_SQL: &str = "DELETE FROM follows AS f
       USING users AS u
       WHERE f.followee_id = $1 AND u.id = f.follower_id
         AND NOT u.local AND u.domain = $2
   RETURNING f.*";

/// [`sever_with_domain`]'s outgoing side: every follow by `$1` of a remote
/// account on the domain `$2`, in any state.
const SEVER_OUTGOING_BY_DOMAIN_SQL: &str = "DELETE FROM follows AS f
       USING users AS u
       WHERE f.follower_id = $1 AND u.id = f.followee_id
         AND NOT u.local AND u.domain = $2
   RETURNING f.*";

/// [`sever_with_domain`]: the memberships of the severed accounts in `$1`'s
/// own lists — a list member is a followed account, the same rule
/// [`unfollow`](super::unfollow) keeps.
const SEVER_LIST_MEMBERSHIPS_SQL: &str = "DELETE FROM list_members AS lm
     USING lists AS l
     WHERE l.id = lm.list_id AND l.user_id = $1 AND lm.user_id = ANY($2)";

/// [`sever_with_domain`]: the endorsements `$1` gave the severed accounts.
const SEVER_ENDORSEMENTS_SQL: &str =
    "DELETE FROM endorsements WHERE user_id = $1 AND target_id = ANY($2)";

/// [`sever_with_domain`]: the accounts on the other end of the removed rows.
const SEVERED_USERS_SQL: &str = "SELECT * FROM users WHERE id = ANY($1)";

/// What [`sever_with_domain`] took out of the table: each open (`pending` or
/// `accepted`) row it deleted next to the account on the other end, which is
/// what the federation hooks need to answer or withdraw it.
#[derive(Debug)]
pub struct SeveredByDomain {
    /// Follows *of* the blocking account by remote accounts on the domain,
    /// each with its follower — the rows a `Reject` is owed for.
    pub followers: Vec<(Follow, User)>,
    /// Follows *by* the blocking account of remote accounts on the domain,
    /// each with its followee — the rows an `Undo Follow` is owed for.
    pub followees: Vec<(Follow, User)>,
}

/// Removes `follower_id`'s accepted follow of `user_id` — the followed
/// account, the row's [`Follow::followee_id`] — and returns the removed
/// relationship, `None` when the account had no such follower to remove.
///
/// The followee's side of [`unfollow`](super::unfollow) (Pleroma's `User.remove_follower`,
/// the write behind `remove_from_followers`): the same accepted relationship
/// taken back, but asked for by the account it points at instead of the one
/// that granted it. Only a follower is ever removed: a `pending` request
/// waits for its owner's own answer ([`accept`](super::accept) / [`reject`](super::reject), the
/// `/api/v1/follow_requests` routes), a `rejected` row (see
/// [`FollowState::Rejected`]) and an unknown pair are the `None` answer, and
/// none of them changes anything. Removing the row steps both denormalised
/// counters down, in the same transaction as the delete: the row and the
/// counts it fed always move together or not at all.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn remove_follower(
    pool: &PgPool,
    user_id: Uuid,
    follower_id: Uuid,
) -> AppResult<Option<Follow>> {
    let mut tx = pool.begin().await?;
    let removed = sqlx::query_as::<_, Follow>(REMOVE_FOLLOWER_SQL)
        .bind(follower_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(follow) = removed else {
        // Nothing was a follower: nothing was written, and the empty
        // transaction ends with the connection (`chat::delete_message_for`
        // does the same).
        return Ok(None);
    };
    step_counters(&mut tx, &follow, -1).await?;
    tx.commit().await?;
    Ok(Some(follow))
}

/// Severs every open relationship between `first` and `second` — in *both*
/// directions — on the open transaction `conn`, and returns nothing but the
/// outcome of the write.
///
/// This is the follow-side half of a block: [`block`](crate::models::block) owns the transaction
/// and calls this inside it, so the block row and the relationships it
/// sever land (or vanish) together — Pleroma's `User.block` unfollowing the
/// pair and withdrawing both directions' pending requests before it stores
/// the block. Every removed row that had been *accepted* steps both
/// denormalised counters down through the module's
/// `step_counters`, a `pending` request moves nothing, and a
/// mirrored `rejected` row is left exactly where it was, because it is
/// nobody's active relationship (see [`unfollow`](super::unfollow)'s same rule).
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn sever_between(conn: &mut PgConnection, first: Uuid, second: Uuid) -> AppResult<()> {
    let removed = sqlx::query_as::<_, Follow>(SEVER_BETWEEN_SQL)
        .bind(first)
        .bind(second)
        .fetch_all(&mut *conn)
        .await?;
    for follow in removed
        .iter()
        .filter(|row| row.state == FollowState::Accepted)
    {
        step_counters(conn, follow, -1).await?;
    }
    Ok(())
}

/// Severs every follow between `user_id` and the remote accounts on `domain`
/// — both directions, every state — in one transaction, and returns the open
/// rows it removed with the accounts at their other ends.
///
/// This is the follow-side half of a domain block (Mastodon's
/// `AfterBlockDomainFromAccountService`): followers on the domain are removed
/// (`accepted`) or refused (`pending`), and the account's own follows of
/// accounts there are withdrawn. `domain` is the canonical spelling
/// [`normalize_domain`](crate::models::domain_block::normalize_domain)
/// writes, compared with `users.domain`; local accounts never match. Every
/// accepted row steps both counters down like [`remove_follower`] does, an
/// outgoing row also takes the list memberships and endorsements that ride on
/// it, and a mirrored `rejected` row is deleted but not returned (there is
/// nothing left to answer). A domain with no relationships changes nothing
/// and returns two empty lists.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn sever_with_domain(
    pool: &PgPool,
    user_id: Uuid,
    domain: &str,
) -> AppResult<SeveredByDomain> {
    let mut tx = pool.begin().await?;
    let incoming = sqlx::query_as::<_, Follow>(SEVER_INCOMING_BY_DOMAIN_SQL)
        .bind(user_id)
        .bind(domain)
        .fetch_all(&mut *tx)
        .await?;
    let outgoing = sqlx::query_as::<_, Follow>(SEVER_OUTGOING_BY_DOMAIN_SQL)
        .bind(user_id)
        .bind(domain)
        .fetch_all(&mut *tx)
        .await?;
    if incoming.is_empty() && outgoing.is_empty() {
        return Ok(SeveredByDomain {
            followers: Vec::new(),
            followees: Vec::new(),
        });
    }
    for follow in incoming
        .iter()
        .chain(&outgoing)
        .filter(|row| row.state == FollowState::Accepted)
    {
        step_counters(&mut tx, follow, -1).await?;
    }
    let outgoing_ids: Vec<Uuid> = outgoing.iter().map(|row| row.followee_id).collect();
    sqlx::query(SEVER_LIST_MEMBERSHIPS_SQL)
        .bind(user_id)
        .bind(&outgoing_ids)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    sqlx::query(SEVER_ENDORSEMENTS_SQL)
        .bind(user_id)
        .bind(&outgoing_ids)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    let incoming_ids: Vec<Uuid> = incoming.iter().map(|row| row.follower_id).collect();
    let mut incoming_peers = load_peers(&mut tx, &incoming_ids).await?;
    let mut outgoing_peers = load_peers(&mut tx, &outgoing_ids).await?;
    tx.commit().await?;
    Ok(SeveredByDomain {
        followers: open_with_peers(incoming, &mut incoming_peers, |row| row.follower_id),
        followees: open_with_peers(outgoing, &mut outgoing_peers, |row| row.followee_id),
    })
}

/// The accounts `ids` name, keyed by id, read on the open transaction `conn`.
async fn load_peers(conn: &mut PgConnection, ids: &[Uuid]) -> AppResult<HashMap<Uuid, User>> {
    let peers = sqlx::query_as::<_, User>(SEVERED_USERS_SQL)
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;
    Ok(peers.into_iter().map(|peer| (peer.id, peer)).collect())
}

/// The open rows of `rows`, each paired with the account `peer_of` names and
/// taken out of `peers`; a row whose account is gone is dropped.
fn open_with_peers(
    rows: Vec<Follow>,
    peers: &mut HashMap<Uuid, User>,
    peer_of: fn(&Follow) -> Uuid,
) -> Vec<(Follow, User)> {
    rows.into_iter()
        .filter(|row| row.state != FollowState::Rejected)
        .filter_map(|row| {
            let peer = peers.remove(&peer_of(&row))?;
            Some((row, peer))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::follow::{follow, get, test_support::*},
        util::id::new_id,
    };

    #[tokio::test]
    async fn remove_follower_takes_an_accepted_follower_and_uncounts_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob).await.unwrap();

        let removed = remove_follower(&pool, bob.id, alice.id).await.unwrap();
        assert_eq!(
            removed.as_ref(),
            Some(&accepted),
            "the removed row comes back as it stood"
        );
        assert!(
            get(&pool, alice.id, bob.id).await.unwrap().is_none(),
            "the follower is gone from the table"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            0,
            "the followee's count steps down with the row"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.following_count,
            0,
            "and the follower's"
        );

        let again = remove_follower(&pool, bob.id, alice.id).await.unwrap();
        assert!(again.is_none(), "a second call finds no follower");
        assert_eq!(follow_count(&pool).await, 0, "and writes nothing");
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            0,
            "with no row left, no counter moves a second time"
        );
    }

    #[tokio::test]
    async fn remove_follower_leaves_everything_but_a_follower_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let erin = local_user(&pool, "erin").await;

        let pending = follow(&pool, TEST_BASE_URL, &alice, &carol).await.unwrap();
        assert_eq!(pending.state, FollowState::Pending, "the fixture waits");
        assert!(
            remove_follower(&pool, carol.id, alice.id)
                .await
                .unwrap()
                .is_none(),
            "a request nobody answered is not a follower"
        );
        assert_eq!(
            relation(&pool, &alice, &carol).await,
            pending,
            "the request waits on, untouched"
        );

        let mirrored = rejected_relation(&pool, &erin, &carol).await;
        assert!(
            remove_follower(&pool, carol.id, erin.id)
                .await
                .unwrap()
                .is_none(),
            "a mirrored rejection is not a follower either"
        );
        assert_eq!(
            relation(&pool, &erin, &carol).await,
            mirrored,
            "and its row stays in place"
        );

        // The reverse of the pair the caller follows: that is the caller's
        // own follow to give back through unfollow, not a follower to remove.
        let outgoing = follow(&pool, TEST_BASE_URL, &carol, &bob).await.unwrap();
        assert_eq!(outgoing.state, FollowState::Accepted, "carol follows bob");
        assert!(
            remove_follower(&pool, carol.id, bob.id)
                .await
                .unwrap()
                .is_none(),
            "bob is not a follower of carol"
        );
        assert_eq!(
            relation(&pool, &carol, &bob).await,
            outgoing,
            "the outgoing follow stays"
        );

        assert!(
            remove_follower(&pool, carol.id, new_id())
                .await
                .unwrap()
                .is_none(),
            "an account with no row at all is no follower"
        );

        assert_eq!(
            (
                fresh_user(&pool, carol.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count,
                fresh_user(&pool, erin.id).await.following_count
            ),
            (0, 0, 0),
            "none of these removals counted anything"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            1,
            "and the one accepted relationship that exists is still on the books"
        );
    }

    #[tokio::test]
    async fn sever_with_domain_takes_both_directions_on_that_domain_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = locked_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let remote = remote_user(&pool, "zed").await;
        let other = remote_user(&pool, "yan").await;
        sqlx::query("UPDATE users SET domain = 'other.example' WHERE id = $1")
            .bind(other.id)
            .execute(&pool)
            .await
            .map(drop)
            .unwrap();

        let incoming = follow(&pool, TEST_BASE_URL, &remote, &alice).await.unwrap();
        let _granted = crate::models::follow::accept(&pool, incoming.id)
            .await
            .unwrap();
        let outgoing = follow(&pool, TEST_BASE_URL, &alice, &remote).await.unwrap();
        let _kept_remote = follow(&pool, TEST_BASE_URL, &other, &alice).await.unwrap();
        let _kept_local = follow(&pool, TEST_BASE_URL, &bob, &alice).await.unwrap();

        let severed = sever_with_domain(&pool, alice.id, "remote.example")
            .await
            .unwrap();
        assert_eq!(severed.followers.len(), 1, "the accepted follower");
        assert_eq!(severed.followers[0].0.id, incoming.id);
        assert_eq!(severed.followers[0].1.id, remote.id);
        assert_eq!(severed.followees.len(), 1, "the pending outgoing request");
        assert_eq!(severed.followees[0].0.id, outgoing.id);
        assert_eq!(severed.followees[0].1.id, remote.id);
        assert_eq!(follow_count(&pool).await, 2, "the other two rows stay");
        assert_eq!(
            fresh_user(&pool, alice.id).await.followers_count,
            0,
            "the removed follower stopped counting"
        );

        let again = sever_with_domain(&pool, alice.id, "remote.example")
            .await
            .unwrap();
        assert!(again.followers.is_empty() && again.followees.is_empty());
        let none = sever_with_domain(&pool, alice.id, "nothing.example")
            .await
            .unwrap();
        assert!(none.followers.is_empty() && none.followees.is_empty());
    }
}
