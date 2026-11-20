//! Reading one relationship: by pair ([`get`]), by primary key ([`by_id`]),
//! by `Follow` activity id ([`get_by_ap_id`], [`find_by_ap_id`]), and the
//! existence checks built on those ([`is_following`], [`has_accepted_follow`],
//! [`has_local_followers`], [`following_ids`]), plus [`required`] turning a
//! row a concurrent write removed into a 404.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::user::User,
};

use super::types::Follow;

/// [`get`]: the one relationship of an ordered pair, whatever its state.
const GET_SQL: &str = "SELECT * FROM follows WHERE follower_id = $1 AND followee_id = $2";

/// [`by_id`]: the one relationship behind a primary key.
const BY_ID_SQL: &str = "SELECT * FROM follows WHERE id = $1";

/// [`following_ids`]: the accepted ones, oldest relationship first (`UUIDv7`
/// text sorts in creation order, decision 4).
const FOLLOWING_IDS_SQL: &str = "SELECT followee_id FROM follows
     WHERE follower_id = $1 AND state = 'accepted'
  ORDER BY id";

/// [`has_accepted_follow`] (and so [`is_following`]): whether the pair has
/// an accepted relationship.
const IS_FOLLOWING_SQL: &str = "SELECT EXISTS (
    SELECT 1 FROM follows
     WHERE follower_id = $1 AND followee_id = $2 AND state = 'accepted'
)";

/// [`has_local_followers`]: whether one of `followee_id`'s accepted
/// followers is an account this instance owns — the existence check the
/// ingest path's `ACCEPT_UNSOLICITED_PUBLIC` gate runs.
const HAS_LOCAL_FOLLOWERS_SQL: &str = "SELECT EXISTS (
    SELECT 1 FROM follows f
      JOIN users u ON u.id = f.follower_id
     WHERE f.followee_id = $1 AND f.state = 'accepted' AND u.local
)";

/// The relationship between the ordered pair (`follower_id`,
/// `target_id`), whatever its state, `None` when the pair has none.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, follower_id: Uuid, target_id: Uuid) -> AppResult<Option<Follow>> {
    let follow = sqlx::query_as::<_, Follow>(GET_SQL)
        .bind(follower_id)
        .bind(target_id)
        .fetch_optional(pool)
        .await?;
    Ok(follow)
}

/// [`get_by_ap_id`]: the one relationship of `follower_id` whose `Follow`
/// activity carries `ap_id` — the read the C2S outbox runs when an `Undo`
/// names the follow activity by its id alone (decision 6: a local
/// follower's `ap_id` is `{BASE_URL}/activities/{uuid}`). Scoping the
/// lookup to the follower keeps somebody else's activity id unfindable
/// here, so an `Undo` can only withdraw its own maker's follow.
const GET_BY_AP_ID_SQL: &str = "SELECT * FROM follows WHERE follower_id = $1 AND ap_id = $2";

/// [`find_by_ap_id`]: the one relationship carrying `ap_id` whatever pair
/// it joins. The inbound `Accept`, `Reject` and `Undo` that name the follow
/// they answer by its activity id alone read through here: the answering
/// account is the *followee*, whom the follower-scoped [`get_by_ap_id`]
/// could not key on. The column is `UNIQUE` (decision 6), so at most one
/// row can carry the id.
const FIND_BY_AP_ID_SQL: &str = "SELECT * FROM follows WHERE ap_id = $1";

/// [`set_ap_id`]: the `Follow` activity id onto the row — the mirror write
/// of a remote follower's delivered activity, and the newest id when a peer
/// re-sends the follow under one — refreshing `updated_at` the way every
/// update here does.
const SET_AP_ID_SQL: &str = "UPDATE follows SET ap_id = $2, updated_at = now() WHERE id = $1";

/// The relationship of `follower_id` whose `Follow` activity carries
/// `ap_id`, `None` when the account has no such follow.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_ap_id(
    pool: &PgPool,
    follower_id: Uuid,
    ap_id: &str,
) -> AppResult<Option<Follow>> {
    let follow = sqlx::query_as::<_, Follow>(GET_BY_AP_ID_SQL)
        .bind(follower_id)
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(follow)
}

/// The relationship whose `Follow` activity carries `ap_id`, `None` when no
/// stored row does.
///
/// The unscoped sibling of [`get_by_ap_id`], for the inbound federation
/// handlers: an `Accept` or `Reject` from the followed account names the
/// follow it answers by its activity id alone, and the answering account is
/// the followee, not the follower. [`follow`](super::follow) stores a remote follower's
/// delivered id here and the pair's newest id on every re-send (decision 6),
/// which is what makes an id the key the peers' answers find.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn find_by_ap_id(pool: &PgPool, ap_id: &str) -> AppResult<Option<Follow>> {
    let follow = sqlx::query_as::<_, Follow>(FIND_BY_AP_ID_SQL)
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(follow)
}

/// Records `ap_id` as the `Follow` activity the relationship `id` stands
/// for: the mirror write of the inbound federation handler (decision 6).
///
/// [`follow`](super::follow) leaves a remote follower's row without an id, because that id
/// is the one the follower's own instance minted, and this is where the
/// delivered activity's id lands on the row — including over an older one
/// when the peer re-sends the same follow under a fresh id. An unknown `id`
/// matches no row and changes nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding, an `ap_id` another row
/// already carries) arrives as [`AppError::Database`](crate::error::AppError).
pub async fn set_ap_id(pool: &PgPool, id: Uuid, ap_id: &str) -> AppResult<()> {
    sqlx::query(SET_AP_ID_SQL)
        .bind(id)
        .bind(ap_id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The ids of the users `follower_id` follows, accepted relationships only,
/// oldest follow first (decision 4).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn following_ids(pool: &PgPool, follower_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(FOLLOWING_IDS_SQL)
        .bind(follower_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Whether `follower` has an accepted follow of `target`.
///
/// A pending request is not following, and a rejected or absent one neither.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn is_following(pool: &PgPool, follower: &User, target: &User) -> AppResult<bool> {
    has_accepted_follow(pool, follower.id, target.id).await
}

/// Whether the account `follower_id` has an accepted follow of
/// `target_id` — [`is_following`] keyed on ids, for callers that hold
/// only the primary keys.
///
/// The notification path reads it that way: it knows the recipient and
/// the actor ids without loading either row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn has_accepted_follow(
    pool: &PgPool,
    follower_id: Uuid,
    target_id: Uuid,
) -> AppResult<bool> {
    let row = sqlx::query_as::<_, (bool,)>(IS_FOLLOWING_SQL)
        .bind(follower_id)
        .bind(target_id)
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// Whether any local account has an accepted follow of `target_id`.
///
/// This is the one read the ingest path's `ACCEPT_UNSOLICITED_PUBLIC` gate
/// asks when a remote post's audience is not settled by a mention or a
/// replied-to local status (see [`crate::services::ingest`]).
///
/// A pending request does not count: the relationship is not open yet, so
/// the follower has not asked to see this account's posts. The follower's
/// own activation is not checked — deactivating an account does not delete
/// its follows, and an operator who suspends a mirror's follower and wants
/// the mirror's posts to stop arriving should block the domain, not
/// deactivate one account.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn has_local_followers(pool: &PgPool, target_id: Uuid) -> AppResult<bool> {
    let row = sqlx::query_as::<_, (bool,)>(HAS_LOCAL_FOLLOWERS_SQL)
        .bind(target_id)
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// The relationship behind a primary key.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub(super) async fn by_id(pool: &PgPool, follow_id: Uuid) -> AppResult<Option<Follow>> {
    let follow = sqlx::query_as::<_, Follow>(BY_ID_SQL)
        .bind(follow_id)
        .fetch_optional(pool)
        .await?;
    Ok(follow)
}

/// Turns a row a statement did not guarantee into a result, so a relationship
/// a concurrent write removed answers like any other absent record: 404.
pub(super) fn required(follow: Option<Follow>) -> AppResult<Follow> {
    follow.map_or_else(|| Err(AppError::NotFound), Ok)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::follow::{FollowState, accept, follow, test_support::*},
        util::id::new_id,
    };

    #[tokio::test]
    async fn following_ids_lists_accepted_follows_oldest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;

        let granted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the first follow is recorded");
        let waiting = follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("the second follow is recorded");
        let other = follow(&pool, TEST_BASE_URL, &alice, &dave)
            .await
            .expect("the third follow is recorded");
        let mut listed = vec![granted, other];
        assert_eq!(
            following_ids(&pool, alice.id).await.unwrap(),
            followed_oldest_first(&listed),
            "the two granted follows, oldest relationship first"
        );
        assert!(
            following_ids(&pool, bob.id).await.unwrap().is_empty(),
            "an account nobody follows has an empty list"
        );

        let answered = accept(&pool, waiting.id)
            .await
            .expect("the request is granted");
        assert_eq!(
            answered.state,
            FollowState::Accepted,
            "answering carol's request is what let it into the list"
        );
        listed.push(relation(&pool, &alice, &carol).await);
        assert_eq!(
            following_ids(&pool, alice.id).await.unwrap(),
            followed_oldest_first(&listed),
            "once answered, all three are listed"
        );
    }

    #[test]
    fn a_missing_relationship_is_answered_not_found() {
        assert!(
            matches!(required(None), Err(AppError::NotFound)),
            "a row a concurrent write removed is a 404"
        );
        let stored = Follow {
            id: new_id(),
            follower_id: new_id(),
            followee_id: new_id(),
            state: FollowState::Accepted,
            ap_id: None,
            show_reblogs: true,
            notify: false,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(
            required(Some(stored.clone())).unwrap(),
            stored,
            "a row that is there passes through"
        );
    }

    /// [`get_by_ap_id`]: the C2S `Undo` read — the follower's own
    /// relationship answers its minted activity id, another account
    /// quoting the same id finds nothing, and an unknown id is `None`.
    #[tokio::test]
    async fn a_follow_answers_its_maker_by_activity_id_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let carol = local_user(&pool, "carol").await;
        let bob = local_user(&pool, "bob").await;

        let created = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the follow stores");
        let ap_id = created
            .ap_id
            .expect("a local follow carries an activity id");
        let found = get_by_ap_id(&pool, alice.id, &ap_id)
            .await
            .expect("the read runs")
            .expect("the maker finds its own follow");
        assert_eq!(found.id, created.id, "and reads the row behind the id");
        assert!(
            get_by_ap_id(&pool, carol.id, &ap_id)
                .await
                .expect("the read runs")
                .is_none(),
            "somebody else quoting the id names none of their own follows"
        );
        assert!(
            get_by_ap_id(
                &pool,
                alice.id,
                &format!("{TEST_BASE_URL}/activities/never")
            )
            .await
            .expect("the read runs")
            .is_none(),
            "and an id no row carries answers nothing"
        );
    }

    /// [`set_ap_id`] and [`find_by_ap_id`]: the mirror write lands the
    /// delivered activity id on a remote follower's id-less row, the
    /// unscoped read keys on it, a re-send replaces the older id, and an
    /// unknown row or id changes and answers nothing.
    #[tokio::test]
    async fn the_mirror_id_write_lands_and_the_unscoped_read_finds_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mallory = remote_user(&pool, "mallory").await;
        let bob = local_user(&pool, "bob").await;

        let created = follow(&pool, TEST_BASE_URL, &mallory, &bob)
            .await
            .expect("the remote follow stores");
        assert!(
            created.ap_id.is_none(),
            "a remote follower's row starts without an id"
        );

        let first = "https://remote.example/activities/follow-1";
        set_ap_id(&pool, created.id, first)
            .await
            .expect("the mirror write runs");
        assert_eq!(
            find_by_ap_id(&pool, first)
                .await
                .expect("the read runs")
                .map(|row| row.id),
            Some(created.id),
            "the delivered id keys the row"
        );

        let second = "https://remote.example/activities/follow-2";
        set_ap_id(&pool, created.id, second)
            .await
            .expect("the re-send's write runs");
        assert!(
            find_by_ap_id(&pool, first)
                .await
                .expect("the read runs")
                .is_none(),
            "the older id keys nothing once the newest took its place"
        );
        assert_eq!(
            find_by_ap_id(&pool, second)
                .await
                .expect("the read runs")
                .map(|row| row.ap_id),
            Some(Some(second.to_owned())),
            "and the newest id is the stored one"
        );

        set_ap_id(&pool, new_id(), "https://remote.example/activities/nowhere")
            .await
            .expect("a write onto an unknown row is a no-op, not a failure");
        assert!(
            find_by_ap_id(&pool, "https://remote.example/activities/nowhere")
                .await
                .expect("the read runs")
                .is_none(),
            "and it stored nothing"
        );
    }
}
