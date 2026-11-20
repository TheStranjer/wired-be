//! Reading a voter's marks: one poll's positions
//! ([`voted_option_positions`]), the render batch's page-shaped
//! [`votes_by_status`], and the vote endpoint's [`has_voted`] probe.

use std::collections::HashMap;

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

/// [`voted_option_positions`]: one account's marks on one poll, walked through
/// the primary key's `poll_id` lead and joined to `poll_options` because the
/// answer is the option's `position`, not the row that stored the mark.
const VOTES_SQL: &str = "SELECT o.position FROM poll_votes v
      JOIN poll_options o ON o.id = v.option_id
       WHERE v.poll_id = $1 AND v.user_id = $2
    ORDER BY o.position";

/// The positions within poll `poll_id` that `user_id` voted for, ascending.
///
/// This is the Mastodon `Poll` entity's `voted` flag (the list is non-empty)
/// and its `own_votes` array (the list itself) for the viewer of a status.
/// Nobody's marks are a fact to report: an account that never voted, a poll
/// that does not exist and a poll somebody else voted all answer the empty
/// `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn voted_option_positions(
    pool: &PgPool,
    poll_id: Uuid,
    user_id: Uuid,
) -> AppResult<Vec<i32>> {
    Ok(sqlx::query_scalar::<_, i32>(VOTES_SQL)
        .bind(poll_id)
        .bind(user_id)
        .fetch_all(pool)
        .await?)
}

/// [`votes_by_status`]: one account's marks on the polls of a whole batch
/// of statuses, walked from `poll_votes` through `poll_options` (for the
/// position) and `polls` (for the status the map is keyed by). Ordering by
/// status then position makes each entry of the grouped answer ascending,
/// the same shape [`voted_option_positions`] answers in for one poll.
const VOTES_BY_STATUS_SQL: &str = "SELECT p.status_id, o.position
    FROM poll_votes v
    JOIN poll_options o ON o.id = v.option_id
    JOIN polls p ON p.id = v.poll_id
   WHERE p.status_id = ANY($1) AND v.user_id = $2
ORDER BY p.status_id, o.position";

/// The positions `user_id` marked on the poll of every status in
/// `status_ids`, keyed by status and ascending by position within each
/// entry.
///
/// This is the batched mate of [`voted_option_positions`] — the render
/// batch's one read behind every poll entity's `voted` flag and
/// `own_votes` array. A status whose poll the viewer said nothing on (or
/// that carries no poll) gets no entry in the map, and an empty
/// `status_ids` yields an empty map; nobody's marks stay nobody's facts.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn votes_by_status(
    pool: &PgPool,
    status_ids: &[Uuid],
    user_id: Uuid,
) -> AppResult<HashMap<Uuid, Vec<i32>>> {
    let status_ids = status_ids.to_vec();
    let rows = sqlx::query_as::<_, (Uuid, i32)>(VOTES_BY_STATUS_SQL)
        .bind(&status_ids)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    let mut grouped: HashMap<Uuid, Vec<i32>> = HashMap::new();
    for (status_id, position) in rows {
        grouped.entry(status_id).or_default().push(position);
    }
    Ok(grouped)
}

/// [`has_voted`]: one probe of the primary key's `poll_id, option_id`
/// prefix restricted to the voter — the walk the entity's `voted` flag
/// already makes, stopped at its first row because only the existence of
/// a mark is asked.
const HAS_VOTED_SQL: &str =
    "SELECT EXISTS (SELECT 1 FROM poll_votes WHERE poll_id = $1 AND user_id = $2)";

/// Whether `user_id` has marked anything on poll `poll_id`.
///
/// This is the vote endpoint's already-voted refusal: a voter with any
/// mark on the poll is turned away, whichever option it was. An unknown
/// poll, an account that never voted and a poll only others voted on all
/// answer `false` — nobody else's ballot is this voter's fact.
///
/// [`cast_vote`](crate::models::poll::cast_vote) reads the voter's marks again inside its
/// transaction (the same `poll_id, user_id` walk), so the endpoint's
/// answer stays honest under concurrent requests.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn has_voted(pool: &PgPool, poll_id: Uuid, user_id: Uuid) -> AppResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(HAS_VOTED_SQL)
        .bind(poll_id)
        .bind(user_id)
        .fetch_one(pool)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::poll::{options, test_support::*},
        util::id::new_id,
    };

    #[tokio::test]
    async fn deleting_a_poll_takes_its_options_and_votes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let posted = create_status(&pool, &alice).await;
        let created = create_for(&pool, posted.id, &params(&["yes", "no"])).await;
        let options = options(&pool, created.id).await.expect("options read");
        for option in &options {
            vote(&pool, created.id, option.id, alice.id).await;
        }
        assert_eq!(
            voted_option_positions(&pool, created.id, alice.id)
                .await
                .expect("the voter's marks read"),
            vec![0, 1],
            "the fixture marks are there to be taken"
        );

        let removed = sqlx::query("DELETE FROM polls WHERE id = $1")
            .bind(created.id)
            .execute(&pool)
            .await
            .expect("the delete runs");
        assert_eq!(removed.rows_affected(), 1, "the fixture poll existed");
        assert_eq!(
            count_for(
                &pool,
                "SELECT count(*) FROM poll_options WHERE poll_id = $1",
                created.id
            )
            .await,
            0,
            "the options cascaded with the poll"
        );
        assert_eq!(
            count_for(
                &pool,
                "SELECT count(*) FROM poll_votes WHERE poll_id = $1",
                created.id
            )
            .await,
            0,
            "and so did the votes"
        );
    }

    #[tokio::test]
    async fn deleting_one_option_takes_its_votes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let posted = create_status(&pool, &alice).await;
        let created = create_for(&pool, posted.id, &params(&["tea", "coffee"])).await;
        let options = options(&pool, created.id).await.expect("options read");
        let (tea, coffee) = (
            options.first().expect("the first option exists"),
            options.get(1).expect("the second option exists"),
        );
        vote(&pool, created.id, tea.id, alice.id).await;
        vote(&pool, created.id, tea.id, bob.id).await;
        vote(&pool, created.id, coffee.id, bob.id).await;

        let removed = sqlx::query("DELETE FROM poll_options WHERE id = $1")
            .bind(tea.id)
            .execute(&pool)
            .await
            .expect("the delete runs");
        assert_eq!(removed.rows_affected(), 1, "the fixture option existed");

        assert_eq!(
            voted_option_positions(&pool, created.id, bob.id)
                .await
                .expect("the voter's marks read"),
            vec![1],
            "bob keeps his other mark, alice's only mark is gone"
        );
        assert!(
            voted_option_positions(&pool, created.id, alice.id)
                .await
                .expect("the voter's marks read")
                .is_empty(),
            "and alice has voted for nothing now"
        );
        assert_eq!(
            count_for(
                &pool,
                "SELECT count(*) FROM polls WHERE id = $1",
                created.id
            )
            .await,
            1,
            "the poll itself is untouched"
        );
    }

    #[tokio::test]
    async fn voted_option_positions_are_per_voter_and_per_poll() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let first = create_status(&pool, &alice).await;
        let second = create_status(&pool, &alice).await;
        let first_poll = create_for(&pool, first.id, &params(&["a", "b", "c"])).await;
        let second_poll = create_for(&pool, second.id, &params(&["d"])).await;
        let choices = options(&pool, first_poll.id).await.expect("options read");

        // Voted out of order: the answer is by position, not by cast time.
        vote(&pool, first_poll.id, choices[2].id, alice.id).await;
        vote(&pool, first_poll.id, choices[0].id, alice.id).await;
        vote(&pool, first_poll.id, choices[1].id, bob.id).await;

        assert_eq!(
            voted_option_positions(&pool, first_poll.id, alice.id)
                .await
                .expect("the voter's marks read"),
            vec![0, 2],
            "alice's two marks, lowest position first"
        );
        assert_eq!(
            voted_option_positions(&pool, first_poll.id, bob.id)
                .await
                .expect("the voter's marks read"),
            vec![1],
            "bob's one mark is nobody else's"
        );
        assert!(
            voted_option_positions(&pool, first_poll.id, new_id())
                .await
                .expect("the voter's marks read")
                .is_empty(),
            "an account that never voted has no marks"
        );
        assert!(
            voted_option_positions(&pool, second_poll.id, alice.id)
                .await
                .expect("the voter's marks read")
                .is_empty(),
            "and a poll she said nothing on has none either"
        );
        assert!(
            voted_option_positions(&pool, new_id(), alice.id)
                .await
                .expect("the unknown poll reads")
                .is_empty(),
            "an unknown poll answers no positions"
        );
    }

    #[tokio::test]
    async fn votes_by_status_groups_the_viewer_s_positions_per_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let first = create_status(&pool, &alice).await;
        let second = create_status(&pool, &alice).await;
        let plain = create_status(&pool, &alice).await;
        let first_poll = create_for(&pool, first.id, &params(&["a", "b", "c"])).await;
        let second_poll = create_for(&pool, second.id, &params(&["d", "e"])).await;
        let choices = options(&pool, first_poll.id).await.expect("options read");
        let other_choices = options(&pool, second_poll.id).await.expect("options read");

        // Voted out of order: each entry answers ascending by position.
        vote(&pool, first_poll.id, choices[2].id, alice.id).await;
        vote(&pool, first_poll.id, choices[0].id, alice.id).await;
        vote(&pool, second_poll.id, other_choices[1].id, bob.id).await;

        let for_alice =
            votes_by_status(&pool, &[first.id, second.id, plain.id, new_id()], alice.id)
                .await
                .expect("the batched votes read");
        assert_eq!(
            for_alice.get(&first.id),
            Some(&vec![0, 2]),
            "alice's two marks, lowest position first"
        );
        assert_eq!(
            for_alice.len(),
            1,
            "the poll she said nothing on, the poll-less status and the \
             unknown id all stay out of the map"
        );

        let for_bob = votes_by_status(&pool, &[first.id, second.id], bob.id)
            .await
            .expect("the batched votes read");
        assert_eq!(
            for_bob.get(&second.id),
            Some(&vec![1]),
            "bob's one mark is nobody else's"
        );
        assert!(
            !for_bob.contains_key(&first.id),
            "and he never touched the first poll"
        );

        let for_carol = create_user(&pool, "carol").await;
        assert!(
            votes_by_status(&pool, &[first.id, second.id], for_carol.id)
                .await
                .expect("a fresh account's votes read")
                .is_empty(),
            "an account that never voted has no entries"
        );
        assert!(
            votes_by_status(&pool, &[], alice.id)
                .await
                .expect("the empty batch reads")
                .is_empty(),
            "no statuses, no marks"
        );
    }

    #[tokio::test]
    async fn has_voted_reads_only_the_asking_account_s_ballot() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let posted = create_status(&pool, &alice).await;
        let created = create_for(&pool, posted.id, &params(&["tea", "coffee"])).await;
        let choices = options(&pool, created.id).await.expect("options read");

        assert!(
            !has_voted(&pool, created.id, bob.id)
                .await
                .expect("the flag reads"),
            "an account that never voted has no ballot"
        );
        vote(&pool, created.id, choices[1].id, bob.id).await;
        assert!(
            has_voted(&pool, created.id, bob.id)
                .await
                .expect("the flag reads"),
            "one mark on any option is a ballot"
        );
        assert!(
            !has_voted(&pool, created.id, alice.id)
                .await
                .expect("the flag reads"),
            "and it is nobody else's ballot — the author's included"
        );
        assert!(
            !has_voted(&pool, new_id(), bob.id)
                .await
                .expect("the flag reads"),
            "an unknown poll holds nobody's marks"
        );
    }
}
