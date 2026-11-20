//! The audience reads the federation, notification and streaming paths run
//! over the `follows` table: [`move_followers_of`], [`remote_followers`],
//! [`remote_followees`], [`subscribers`] and [`accepted_local_follower_ids`].

use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, models::user::User};

/// [`move_followers_of`]: the followers whose follow of the moving account
/// an account move may carry over — accepted relationships held by *local*
/// accounts that still allow their following list to move (Pleroma's
/// `FollowingRelationship.move_following` filters `f.local == true` and
/// `f.allow_following_move == true`). Oldest relationship first.
const MOVE_FOLLOWERS_SQL: &str = "SELECT u.* FROM follows f
     JOIN users u ON u.id = f.follower_id
      WHERE f.followee_id = $1 AND f.state = 'accepted'
        AND u.local = true AND u.allow_following_move = true
   ORDER BY f.id";

/// [`remote_followers`]: accepted relationships held by *remote*, active
/// mirrors of the bound account — the followers collection exactly as the
/// sender-side fan-out needs it (Pleroma's `User.get_external_followers`,
/// which its publisher expands into follower inboxes). Local followers are
/// not in it; they receive the activity through their timelines, never
/// through an HTTP delivery, and a deactivated mirror's inbox is nobody's
/// delivery target either. Oldest relationship first.
const REMOTE_FOLLOWERS_SQL: &str = "SELECT u.* FROM follows f
     JOIN users u ON u.id = f.follower_id
      WHERE f.followee_id = $1 AND f.state = 'accepted'
        AND u.local = false AND u.is_active = true
   ORDER BY f.id";

/// [`remote_followees`]: accepted relationships held *toward* remote, active
/// mirrors of the bound account — the followee-side mirror of
/// [`REMOTE_FOLLOWERS_SQL`], with the same accepted-and-active gate.
const REMOTE_FOLLOWEES_SQL: &str = "SELECT u.* FROM follows f
     JOIN users u ON u.id = f.followee_id
      WHERE f.follower_id = $1 AND f.state = 'accepted'
        AND u.local = false AND u.is_active = true
   ORDER BY f.id";

/// [`subscribers`]: the *local*, active followers whose accepted follow of
/// the bound account has its `notify` toggle on — Pleroma's
/// `User.subscriber_users` (`lib/pleroma/user.ex`), the list its
/// `get_notified_subscribers` walks to tell subscribers of every new post.
/// Remote subscribers are deliberately absent: they are told by federation,
/// not by a row (the rule [`crate::services::notify`] states). Oldest
/// relationship first.
const SUBSCRIBERS_SQL: &str = "SELECT u.* FROM follows f
     JOIN users u ON u.id = f.follower_id
      WHERE f.followee_id = $1 AND f.state = 'accepted'
        AND f.notify = true AND u.local = true AND u.is_active = true
   ORDER BY f.id";

/// [`accepted_local_follower_ids`]: the accepted relationships of
/// `followee_id` held by accounts this instance owns that are still active,
/// oldest relationship first. `reblogs_only` ($2) drops the followers who
/// turned [`show_reblogs`] off — a bound `false` parameter lets the whole
/// predicate pass, so one statement serves both the plain and the
/// boosts-hiding audience.
///
/// [`show_reblogs`]: Follow::show_reblogs
const ACCEPTED_LOCAL_FOLLOWER_IDS_SQL: &str = "SELECT f.follower_id FROM follows f
     JOIN users u ON u.id = f.follower_id
      WHERE f.followee_id = $1 AND f.state = 'accepted'
        AND u.local = true AND u.is_active = true
        AND ($2 = false OR f.show_reblogs = true)
   ORDER BY f.id";
/// The local followers of `user_id` whose follow an account move carries
/// over.
///
/// Accepted relationships held by accounts this instance owns that still
/// allow their following list to move
/// ([`allow_following_move`](crate::models::user::User::allow_following_move)),
/// oldest relationship first. This is the candidate set Pleroma's
/// `FollowingRelationship.move_following` walks when `move_account`
/// transfers followers to the move target; the endpoint that consumes it
/// ([`crate::api::pleroma_account_settings`]) does the re-following.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn move_followers_of(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(MOVE_FOLLOWERS_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The remote users with an accepted follow of `user_id`, active mirrors
/// only, oldest relationship first.
///
/// This is the followers collection read from the *sending* side: when this
/// instance publishes an activity addressed to `user_id`'s followers URL,
/// these are the accounts standing behind it, and their inboxes are what
/// the fan-out addresses
/// ([`crate::federation::publisher`] expands the collection through this
/// query). Pleroma's `User.get_external_followers` is the reference; local
/// followers are deliberately absent — they get the activity through their
/// timelines, not a delivery — and so are deactivated mirrors, which can
/// receive nothing.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn remote_followers(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(REMOTE_FOLLOWERS_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// [`remote_followees`]: the *remote*, active mirrors `user_id` follows.
///
/// Accepted relationships held toward those mirrors — the followee-side
/// mirror of `REMOTE_FOLLOWERS_SQL`, with the same accepted-and-active
/// gate. The account `Delete` fan-out reads it: the accounts this one
/// followed learned something from it too, and both sides of every
/// relationship must learn it is gone.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn remote_followees(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(REMOTE_FOLLOWEES_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The local, active users holding a *subscribed* accepted follow of
/// `user_id`.
///
/// These are the accounts [`crate::services::notify`] tells `status`
/// notifications of every post this account makes — Pleroma's
/// `User.subscriber_users`, which its `get_notified_subscribers` reads the
/// same way: `notify` true on a granted follow.
///
/// A pending request with its toggle on is not a subscriber yet, a plain
/// accepted follow is nobody's subscriber, and a deactivated or remote
/// account cannot be told from here.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn subscribers(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(SUBSCRIBERS_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The ids of the **local, active, accepted** followers of `followee_id`,
/// oldest relationship first.
///
/// The follower side of a streaming audience (see
/// [`crate::services::streaming`]): a remote mirror holds no socket on this
/// instance to push to, a deactivated account must not be handed new events,
/// and a not-yet-accepted relationship has granted nothing — so the query
/// keeps only accepted relationships whose follower is local and active.
/// `reblogs_only` additionally drops the followers who turned
/// [`show_reblogs`](super::Follow::show_reblogs) off for the followee, which is the
/// home rule a boost follows: the caller sets it when the post whose
/// audience is being computed is a reblog row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn accepted_local_follower_ids(
    pool: &PgPool,
    followee_id: Uuid,
    reblogs_only: bool,
) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(ACCEPTED_LOCAL_FOLLOWER_IDS_SQL)
        .bind(followee_id)
        .bind(reblogs_only)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            follow::{
                FollowState, FollowToggles, follow, get, set_notify, set_toggles, test_support::*,
            },
            user::{Flag, NewUser, insert},
        },
        util::id::new_id,
    };

    /// The move-follower query keeps exactly the accounts the move may
    /// carry: local, with an accepted follow of the moving account, and
    /// still allowing their following list to move. Remote mirrors,
    /// rejected relationships and opted-out followers stay out of the set.
    #[tokio::test]
    async fn move_followers_of_selects_local_accepted_permitters() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mover = local_user(&pool, "mover").await;
        let carried = local_user(&pool, "carried").await;
        let opted_out = insert(
            &pool,
            &NewUser {
                allow_following_move: Flag::new(false),
                ..local_new("opted_out")
            },
        )
        .await
        .expect("the opted-out fixture inserts");
        let rejected = local_user(&pool, "rejected").await;
        let remote = remote_user(&pool, "remote_fan").await;

        let _accepted = follow(&pool, TEST_BASE_URL, &carried, &mover)
            .await
            .expect("the carried follow is accepted");
        let _opted_out = follow(&pool, TEST_BASE_URL, &opted_out, &mover)
            .await
            .expect("the opted-out follow is accepted");
        let _stale = rejected_relation(&pool, &rejected, &mover).await;
        let _remote = follow(&pool, TEST_BASE_URL, &remote, &mover)
            .await
            .expect("the remote follow is accepted");
        // An accepted local follow of someone else must not qualify either.
        let other = local_user(&pool, "other").await;
        let _unrelated = follow(&pool, TEST_BASE_URL, &carried, &other)
            .await
            .expect("the unrelated follow is accepted");

        assert_eq!(
            ids(&move_followers_of(&pool, mover.id).await.unwrap()),
            vec![carried.id],
            "only the local, accepted, non-opted-out follower of the mover"
        );
        assert!(
            move_followers_of(&pool, new_id()).await.unwrap().is_empty(),
            "an account nobody follows has an empty move set"
        );
    }

    /// `accepted_local_follower_ids` keeps exactly the streaming audience's
    /// follower side: local, active, accepted follows of the account — and,
    /// with `reblogs_only`, only the ones still showing its boosts.
    #[tokio::test]
    async fn accepted_local_follower_ids_selects_local_active_accepted_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let author = local_user(&pool, "author").await;
        let fan = local_user(&pool, "fan").await;
        let quiet = local_user(&pool, "quiet").await;
        let ghost = insert(
            &pool,
            &NewUser {
                is_active: Flag::new(false),
                ..local_new("ghost")
            },
        )
        .await
        .expect("the deactivated fixture inserts");
        let rejected = local_user(&pool, "rejected").await;
        let remote = remote_user(&pool, "remote_fan").await;

        let _fan = follow(&pool, TEST_BASE_URL, &fan, &author)
            .await
            .expect("the fan's follow is accepted");
        let _quiet = follow(&pool, TEST_BASE_URL, &quiet, &author)
            .await
            .expect("the quiet follower's follow is accepted");
        let toggled = set_toggles(
            &pool,
            quiet.id,
            author.id,
            FollowToggles {
                show_reblogs: Some(false),
                notify: None,
            },
        )
        .await
        .unwrap()
        .expect("the accepted row takes the toggle");
        assert!(!toggled.show_reblogs, "the stored row hides boosts");
        let _ghost = follow(&pool, TEST_BASE_URL, &ghost, &author)
            .await
            .expect("the deactivated follow is accepted");
        let _stale = rejected_relation(&pool, &rejected, &author).await;
        let _remote = follow(&pool, TEST_BASE_URL, &remote, &author)
            .await
            .expect("the remote follow is accepted");

        let mut expected = vec![fan.id, quiet.id];
        expected.sort_unstable();
        let mut with_hidden = accepted_local_follower_ids(&pool, author.id, false)
            .await
            .unwrap();
        with_hidden.sort_unstable();
        assert_eq!(
            with_hidden, expected,
            "the two local active accepted followers, boosts-hidden one included"
        );
        assert_eq!(
            accepted_local_follower_ids(&pool, author.id, true)
                .await
                .unwrap(),
            vec![fan.id],
            "reblogs_only drops the follower who turned boosts off, and with them \
             the remote mirror, the deactivated account and the rejected row"
        );
    }

    /// A pending request has granted nothing, so a locked account's
    /// follower queue is empty to the streaming audience either way.
    #[tokio::test]
    async fn accepted_local_follower_ids_skips_pending_relationships() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let idol = locked_user(&pool, "idol").await;
        let requester = local_user(&pool, "requester").await;
        let _pending = follow(&pool, TEST_BASE_URL, &requester, &idol)
            .await
            .expect("the locked account's follow waits");
        assert_eq!(
            get(&pool, requester.id, idol.id)
                .await
                .unwrap()
                .expect("the request exists")
                .state,
            FollowState::Pending,
            "the fixture waits, unanswered"
        );

        assert!(
            accepted_local_follower_ids(&pool, idol.id, false)
                .await
                .unwrap()
                .is_empty(),
            "a waiting request is nobody's audience"
        );
    }

    #[tokio::test]
    async fn subscribers_are_the_local_active_notify_flagged_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let author = local_user(&pool, "author").await;
        let subscriber = local_user(&pool, "subscriber").await;
        let plain = local_user(&pool, "plain").await;

        // The subscribed local follower: the only one the read answers.
        let _open = follow(&pool, TEST_BASE_URL, &subscriber, &author)
            .await
            .expect("the subscriber's follow is granted");
        let _toggled = set_notify(&pool, subscriber.id, author.id, true)
            .await
            .expect("the toggle runs")
            .expect("the accepted pair takes the flag");
        // A plain accepted follow of the same author stays out.
        let _quiet = follow(&pool, TEST_BASE_URL, &plain, &author)
            .await
            .expect("the plain follow is granted");
        // A remote subscriber is told by federation, never by a row.
        let alien = remote_user(&pool, "alien").await;
        let _mirrored = follow(&pool, TEST_BASE_URL, &alien, &author)
            .await
            .expect("the remote follow is granted");
        let _alien_toggled = set_notify(&pool, alien.id, author.id, true)
            .await
            .expect("the remote toggle runs")
            .expect("the remote pair takes the flag");
        // A pending request whose toggle is on has not subscribed yet: the
        // lock refuses `set_notify`, so the toggles write stands in.
        let waiting = local_user(&pool, "waiting").await;
        let gated = locked_user(&pool, "gated").await;
        let _request = follow(&pool, TEST_BASE_URL, &waiting, &gated)
            .await
            .expect("the request waits");
        let _open_toggles = set_toggles(
            &pool,
            waiting.id,
            gated.id,
            FollowToggles {
                show_reblogs: None,
                notify: Some(true),
            },
        )
        .await
        .expect("the toggles run")
        .expect("the open pair takes them");
        assert!(
            relation(&pool, &waiting, &gated).await.notify,
            "the pending row carries the flag the read must still refuse"
        );
        // A deactivated local subscriber cannot be told either.
        let ghost = insert(
            &pool,
            &NewUser {
                is_active: Flag::new(false),
                ..local_new("ghost")
            },
        )
        .await
        .expect("the deactivated fixture inserts");
        let _ghosted = follow(&pool, TEST_BASE_URL, &ghost, &author)
            .await
            .expect("the deactivated follow is granted");
        let _ghost_toggled = set_notify(&pool, ghost.id, author.id, true)
            .await
            .expect("the ghost's toggle runs")
            .expect("its accepted pair takes the flag");

        assert_eq!(
            ids(&subscribers(&pool, author.id).await.expect("the read runs")),
            vec![subscriber.id],
            "only the local, active, granted, notified follower"
        );
        assert!(
            subscribers(&pool, gated.id)
                .await
                .expect("the locked account reads")
                .is_empty(),
            "a pending request has subscribed nobody yet"
        );
        assert!(
            subscribers(&pool, new_id())
                .await
                .expect("an unknown id reads")
                .is_empty(),
            "and an account nobody follows has no subscribers"
        );
    }
}
