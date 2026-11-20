//! Reading polls and options: one status's poll ([`get_for_status`]), one
//! poll's own row ([`get_by_id`]), the batched page reads ([`for_statuses`]
//! and [`options_for_statuses`]) and one poll's ordered [`options`].

use std::collections::HashMap;

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

use super::{Poll, PollOption};

/// Fetches the poll of status `status_id`, `None` when that status carries
/// none (or is not stored at all).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_for_status(pool: &PgPool, status_id: Uuid) -> AppResult<Option<Poll>> {
    let stored = sqlx::query_as::<_, Poll>(
        "SELECT id, status_id, expires_at, multiple, hide_totals, voters_count, votes_count,
                ended_notified
           FROM polls WHERE status_id = $1",
    )
    .bind(status_id)
    .fetch_optional(pool)
    .await?;
    Ok(stored)
}

/// Fetches the poll whose own primary key is `poll_id`, `None` when no such
/// poll is stored.
///
/// This is the read behind `GET /api/v1/polls/{id}` and the vote endpoint:
/// those routes are keyed on the poll's own id (decision 4), not on the
/// status's that [`get_for_status`] answers from.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, poll_id: Uuid) -> AppResult<Option<Poll>> {
    let stored = sqlx::query_as::<_, Poll>(
        "SELECT id, status_id, expires_at, multiple, hide_totals, voters_count, votes_count,
                ended_notified
           FROM polls WHERE id = $1",
    )
    .bind(poll_id)
    .fetch_optional(pool)
    .await?;
    Ok(stored)
}

/// [`for_statuses`]: the polls of a whole batch of statuses in one walk of the
/// unique key on `status_id`.
const FOR_STATUSES_SQL: &str = "SELECT id, status_id, expires_at, multiple, hide_totals,
                                       voters_count, votes_count, ended_notified
                                FROM polls
                               WHERE status_id = ANY($1)
                            ORDER BY status_id";

/// Fetches the poll of every status in `status_ids`, keyed by status.
///
/// Statuses that carry no poll get no entry in the map at all — a renderer
/// asks [`HashMap::get`] and shows no poll when the answer is `None` — and an
/// empty `status_ids` yields an empty map. This is the batched read that lets
/// a page of statuses come with its polls in one query, the shape
/// [`crate::models::media_attachment::for_statuses`] takes for media.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn for_statuses(pool: &PgPool, status_ids: &[Uuid]) -> AppResult<HashMap<Uuid, Poll>> {
    let status_ids = status_ids.to_vec();
    let rows = sqlx::query_as::<_, Poll>(FOR_STATUSES_SQL)
        .bind(&status_ids)
        .fetch_all(pool)
        .await?;
    // One row per status at most — `polls.status_id` is unique — so the
    // pairs collected here are the whole map, and a status with no row is
    // simply absent from it.
    let grouped: HashMap<Uuid, Poll> = rows
        .into_iter()
        .map(|stored| (stored.status_id, stored))
        .collect();
    Ok(grouped)
}

/// [`options`]: one poll's choices, `position` first — the order a voter sees
/// them in and the order `own_votes` indexes.
const OPTIONS_SQL: &str = "SELECT id, poll_id, position, title, votes_count
                             FROM poll_options
                            WHERE poll_id = $1
                         ORDER BY position";

/// Fetches the options of poll `poll_id`, ordered by
/// [`position`](PollOption::position).
///
/// A poll was created with at least one option and options are only ever
/// written with it, so the list is what its author chose; a poll id that was
/// never created (or one since deleted) answers the empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn options(pool: &PgPool, poll_id: Uuid) -> AppResult<Vec<PollOption>> {
    Ok(sqlx::query_as::<_, PollOption>(OPTIONS_SQL)
        .bind(poll_id)
        .fetch_all(pool)
        .await?)
}

/// [`options_for_statuses`]: the options of a whole batch of statuses'
/// polls in one walk. `poll_options` keys to the poll, not the status, so
/// the statement joins through `polls` to reach the `status_id` the map is
/// keyed by; `poll_id, position` order keeps each poll's choices together,
/// lowest position first.
const OPTIONS_FOR_STATUSES_SQL: &str = "SELECT o.id, o.poll_id, o.position, o.title,
        o.votes_count, p.status_id
   FROM poll_options o
   JOIN polls p ON p.id = o.poll_id
  WHERE p.status_id = ANY($1)
  ORDER BY o.poll_id, o.position";

/// The joined row of [`OPTIONS_FOR_STATUSES_SQL`]: one option with the
/// status its poll belongs to, the key [`options_for_statuses`] groups by.
#[derive(Debug, sqlx::FromRow)]
struct OptionForStatusRow {
    /// The status the option's poll belongs to, from the joined `polls`
    /// row; the map key.
    status_id: Uuid,
    /// The option itself, from the `poll_options` row.
    #[sqlx(flatten)]
    option: PollOption,
}

/// Fetches the options of the poll of every status in `status_ids`, keyed
/// by status and, within each entry, ordered by
/// [`position`](PollOption::position).
///
/// Statuses that carry no poll get no entry in the map at all, and an
/// empty `status_ids` yields an empty map — the batched mate of [`options`]
/// in the shape [`for_statuses`] takes for the polls themselves: one query
/// for a whole page of choices.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn options_for_statuses(
    pool: &PgPool,
    status_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<PollOption>>> {
    let status_ids = status_ids.to_vec();
    let rows = sqlx::query_as::<_, OptionForStatusRow>(OPTIONS_FOR_STATUSES_SQL)
        .bind(&status_ids)
        .fetch_all(pool)
        .await?;
    // Rows arrive grouped by poll (one poll per status) and ordered by
    // position, so pushing in read order hands each entry its choices in
    // the order voters see them.
    let mut grouped: HashMap<Uuid, Vec<PollOption>> = HashMap::new();
    for row in rows {
        grouped.entry(row.status_id).or_default().push(row.option);
    }
    Ok(grouped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{models::poll::test_support::*, util::id::new_id};

    /// The options of `poll_id` as `(position, title)`, the pair the order
    /// tests compare.
    async fn positions_and_titles(pool: &PgPool, poll_id: Uuid) -> Vec<(i32, String)> {
        options(pool, poll_id)
            .await
            .expect("the options read")
            .into_iter()
            .map(|option| (option.position, option.title))
            .collect()
    }

    #[tokio::test]
    async fn options_come_back_in_position_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let posted = create_status(&pool, &alice).await;
        let created = create_for(&pool, posted.id, &params(&["first", "second", "third"])).await;

        let stored = options(&pool, created.id).await.expect("options read");
        assert_eq!(stored.len(), 3, "one row per title");
        assert_eq!(
            positions_and_titles(&pool, created.id).await,
            vec![
                (0, "first".to_owned()),
                (1, "second".to_owned()),
                (2, "third".to_owned()),
            ],
            "positions count from zero in the order given"
        );
        assert!(
            stored.iter().all(|option| option.votes_count == 0),
            "every fresh option has no votes"
        );
        assert!(
            stored
                .iter()
                .all(|option| option.poll_id == created.id && option.id.get_version_num() == 7),
            "each row belongs to the poll and carries its own UUIDv7"
        );

        // The order is the column's, not the insert order or the id order:
        // moving the first option to the end reorders the read.
        let moved = sqlx::query("UPDATE poll_options SET position = 3 WHERE id = $1")
            .bind(stored[0].id)
            .execute(&pool)
            .await
            .expect("the position moves");
        assert_eq!(moved.rows_affected(), 1, "the option was there to move");
        assert_eq!(
            positions_and_titles(&pool, created.id).await,
            vec![
                (1, "second".to_owned()),
                (2, "third".to_owned()),
                (3, "first".to_owned()),
            ],
            "the read follows `position`, not creation order"
        );

        // Nothing else's options: a second poll reads its own list, and a
        // poll id that was never created reads none.
        let other = create_status(&pool, &alice).await;
        let other_poll = create_for(&pool, other.id, &params(&["only"])).await;
        assert_eq!(
            positions_and_titles(&pool, other_poll.id).await,
            vec![(0, "only".to_owned())],
            "the second poll sees only its own option"
        );
        assert!(
            options(&pool, new_id()).await.expect("reads").is_empty(),
            "an unknown poll has no options"
        );
    }

    #[tokio::test]
    async fn for_statuses_groups_by_status_and_omits_the_poll_less() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let first = create_status(&pool, &alice).await;
        let second = create_status(&pool, &alice).await;
        let plain = create_status(&pool, &alice).await;
        let first_poll = create_for(&pool, first.id, &params(&["a", "b"])).await;
        let second_poll = create_for(&pool, second.id, &params(&["c"])).await;

        let grouped = for_statuses(&pool, &[first.id, second.id, plain.id, new_id()])
            .await
            .expect("the batch reads");
        assert_eq!(grouped.len(), 2, "the poll-less status has no key");
        assert!(
            !grouped.contains_key(&plain.id),
            "and neither has an unknown id"
        );
        assert_eq!(
            grouped.get(&first.id),
            Some(&first_poll),
            "the first status maps to its own poll"
        );
        assert_eq!(
            grouped.get(&second.id),
            Some(&second_poll),
            "and the second status to the second poll"
        );

        assert!(
            for_statuses(&pool, &[])
                .await
                .expect("the empty batch reads")
                .is_empty(),
            "no statuses, no polls"
        );
    }

    #[tokio::test]
    async fn deleting_a_status_takes_its_poll_and_below_it_the_rest() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let posted = create_status(&pool, &alice).await;
        let created = create_for(&pool, posted.id, &params(&["yes", "no"])).await;
        let options = options(&pool, created.id).await.expect("options read");
        let first = options.first().expect("the fixture poll has an option");
        vote(&pool, created.id, first.id, alice.id).await;

        let removed = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(posted.id)
            .execute(&pool)
            .await
            .expect("the hard delete runs");
        assert_eq!(removed.rows_affected(), 1, "the fixture status existed");

        assert!(
            get_for_status(&pool, posted.id)
                .await
                .expect("reads")
                .is_none(),
            "the poll went with its status"
        );
        assert_eq!(
            count_for(
                &pool,
                "SELECT count(*) FROM poll_options WHERE poll_id = $1",
                created.id
            )
            .await,
            0,
            "the poll's options cascaded with it"
        );
        assert_eq!(
            count_for(
                &pool,
                "SELECT count(*) FROM poll_votes WHERE poll_id = $1",
                created.id
            )
            .await,
            0,
            "and so did the mark on them"
        );
    }

    /// The options mapped for `status_id`, as `(position, title)` pairs —
    /// the shape the batched grouping tests compare.
    fn positions_and_titled(
        map: &HashMap<Uuid, Vec<PollOption>>,
        status_id: Uuid,
    ) -> Vec<(i32, String)> {
        map.get(&status_id).map_or_default(|options| {
            options
                .iter()
                .map(|option| (option.position, option.title.clone()))
                .collect()
        })
    }

    #[tokio::test]
    async fn options_for_statuses_groups_by_status_in_position_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let first = create_status(&pool, &alice).await;
        let second = create_status(&pool, &alice).await;
        let plain = create_status(&pool, &alice).await;
        let first_poll = create_for(&pool, first.id, &params(&["a", "b", "c"])).await;
        let second_poll = create_for(&pool, second.id, &params(&["x"])).await;

        let grouped = options_for_statuses(&pool, &[first.id, second.id, plain.id, new_id()])
            .await
            .expect("the batched options read");
        assert_eq!(grouped.len(), 2, "the poll-less status has no key");
        assert!(
            !grouped.contains_key(&plain.id),
            "and neither has an unknown id"
        );
        assert_eq!(
            positions_and_titled(&grouped, first.id),
            vec![
                (0, "a".to_owned()),
                (1, "b".to_owned()),
                (2, "c".to_owned())
            ],
            "the first status's options, lowest position first"
        );
        assert_eq!(
            positions_and_titled(&grouped, second.id),
            vec![(0, "x".to_owned())],
            "the second status sees only its own choices"
        );
        let rows = grouped
            .get(&first.id)
            .expect("the first status's options are grouped");
        assert!(
            rows.iter().all(|option| option.poll_id == first_poll.id),
            "each entry's rows name their own poll"
        );
        let moved = sqlx::query("UPDATE poll_options SET position = 5 WHERE id = $1")
            .bind(
                rows.first()
                    .expect("the first status's options are grouped")
                    .id,
            )
            .execute(&pool)
            .await
            .expect("the position moves");
        assert_eq!(moved.rows_affected(), 1, "the option was there to move");
        let again = options_for_statuses(&pool, &[first.id])
            .await
            .expect("the re-read works");
        assert_eq!(
            positions_and_titled(&again, first.id),
            vec![
                (1, "b".to_owned()),
                (2, "c".to_owned()),
                (5, "a".to_owned())
            ],
            "the read follows `position`, not creation order"
        );

        // Every option row carries its own identity: the ids round trip and
        // name the right poll, and the stored counters ride along.
        let whole = options_for_statuses(&pool, std::slice::from_ref(&second.id))
            .await
            .expect("the batched options read");
        let only = whole
            .get(&second.id)
            .and_then(|options| options.first())
            .expect("the second poll's option is grouped");
        assert_eq!(only.poll_id, second_poll.id, "the row names its poll");
        assert!(
            only.id.get_version_num() == 7,
            "the option carries its UUIDv7"
        );
        assert_eq!(only.votes_count, 0, "a fresh option has no votes");

        assert!(
            options_for_statuses(&pool, &[])
                .await
                .expect("the empty batch reads")
                .is_empty(),
            "no statuses, no options"
        );
    }

    #[tokio::test]
    async fn get_by_id_reads_the_poll_by_its_own_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let posted = create_status(&pool, &alice).await;
        let created = create_for(&pool, posted.id, &params(&["tea", "coffee"])).await;

        let read = get_by_id(&pool, created.id)
            .await
            .expect("the poll reads")
            .expect("the created poll is there");
        assert_eq!(read, created, "the row round trips through its own id");

        assert!(
            get_by_id(&pool, new_id()).await.expect("reads").is_none(),
            "a poll id that names no poll has no row — the status's id is
               not the poll's key either, and a fresh uuid stands in for both"
        );
    }
}
