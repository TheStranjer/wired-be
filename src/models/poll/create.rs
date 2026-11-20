//! Writing polls: [`create`], born inside the caller's status transaction,
//! and [`record_remote_totals`] — the wholesale restatement of a mirrored
//! poll's numbers by the inbound `Question` update.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

use super::{Poll, PollParams};

/// The `INSERT` behind [`create`]: the minted id, the status named, and what
/// [`PollParams`] holds — including the two counters, which a local create
/// leaves at their zero defaults and the ingest path fills from the remote's
/// reported numbers — with `inserted_at` stamped by the database's `now()`
/// (the column has no default) and [`ended_notified`](Poll::ended_notified)
/// at its default, since a fresh poll has not closed.
const INSERT_POLL_SQL: &str = "INSERT INTO polls (
     id, status_id, expires_at, multiple, hide_totals,
     voters_count, votes_count, inserted_at
) VALUES (
     $1, $2, $3, $4, $5, $6, $7, now()
) RETURNING id, status_id, expires_at, multiple, hide_totals,
            voters_count, votes_count, ended_notified";

/// The option rows behind [`create`]: one per title, in the order given, each
/// with the id minted for it, a `position` counting from zero — `(ord - 1)`
/// of [`WITH ORDINALITY`] — and its opening vote count. [`create`] hands the
/// third array one count per option (all zeroes for a local create, the
/// remote's per-option tallies for a mirrored poll), so the whole list still
/// goes in one statement and a poll's options appear all at once with it.
///
/// [`WITH ORDINALITY`]: https://www.postgresql.org/docs/current/functions-array.html
const INSERT_OPTIONS_SQL: &str =
    "INSERT INTO poll_options (id, poll_id, position, title, votes_count)
     SELECT o.id, $1, (o.ord - 1)::integer, o.title, o.votes
       FROM unnest($2::uuid[], $3::text[], $4::integer[])
            WITH ORDINALITY AS o(id, title, votes, ord)";

/// Creates the poll `params` describes for status `status_id`, on `conn`, and
/// returns its row.
///
/// The poll's id and one id per option are fresh `UUIDv7`s (decision 4); the
/// options are written in the order given, at positions counting from zero, in
/// one statement on `conn` — so a caller that inserts a status there gets
/// status, poll and options in one transaction, and a rollback takes all three
/// back. The two counters and the per-option counts are `params`' values:
/// [`PollParams::default`] leaves every one at zero, which is what a poll
/// born from `POST /api/v1/statuses` starts with, and the ingest path hands
/// over the numbers a remote `Question` reports beside its mirrored rows.
/// `inserted_at` is the database's `now()`.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError). Two constraint failures
/// are reachable: `status_id` naming no status is the foreign key, and it
/// fails the whole write, while a status that already has a poll is the
/// unique key on `polls.status_id`.
pub async fn create(
    conn: &mut PgConnection,
    status_id: Uuid,
    params: &PollParams,
) -> AppResult<Poll> {
    let poll = sqlx::query_as::<_, Poll>(INSERT_POLL_SQL)
        .bind(new_id())
        .bind(status_id)
        .bind(params.expires_at)
        .bind(params.multiple)
        .bind(params.hide_totals)
        .bind(params.voters_count)
        .bind(params.votes_count)
        .fetch_one(&mut *conn)
        .await?;
    let ids: Vec<Uuid> = params.options.iter().map(|_| new_id()).collect();
    let option_votes: Vec<i32> = params
        .options
        .iter()
        .enumerate()
        .map(|(index, _)| params.option_votes.get(index).copied().unwrap_or(0))
        .collect();
    sqlx::query(INSERT_OPTIONS_SQL)
        .bind(poll.id)
        .bind(&ids)
        .bind(&params.options)
        .bind(&option_votes)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(poll)
}

/// The totals a remote instance reports for one of its own polls, as the
/// inbound `Update` of a `Question` carries them
/// ([`crate::federation::handlers::note`]).
#[derive(Clone, Debug, Default)]
pub struct RemoteTotals {
    /// Each stored option's title with the vote count the remote gives it.
    /// A title that matches no stored option writes nothing — the options
    /// of a running poll are not the update's to replace.
    pub options: Vec<(String, i32)>,
    /// The remote's distinct-voter count for the poll.
    pub voters_count: i32,
    /// The remote's total-votes count for the poll.
    pub votes_count: i32,
    /// The remote's voting deadline; `None` leaves the stored one alone —
    /// an update that stops naming `endTime` is not a poll that stops
    /// expiring.
    pub expires_at: Option<DateTime<Utc>>,
}

/// [`record_remote_totals`]'s poll write: the totals as the remote states
/// them, the deadline only when the update names one.
const REMOTE_TOTALS_SQL: &str = "UPDATE polls
     SET votes_count = $2,
         voters_count = $3,
         expires_at = COALESCE($4, expires_at)
   WHERE id = $1";

/// [`record_remote_totals`]'s option write: one option's count, keyed by
/// the title the remote repeats; a vote cast here on that option is left
/// in place, this restates the remote's total and nothing else.
const REMOTE_OPTION_SQL: &str = "UPDATE poll_options
     SET votes_count = $3
   WHERE poll_id = $1 AND title = $2";

/// Overwrites poll `poll_id`'s numbers with the totals its remote author
/// reports — the remote-poll-results path of the inbound `Question` update.
///
/// This is not [`cast_vote`](crate::models::poll::cast_vote): nothing was cast here. A
/// mirrored poll's counts are whatever the author's instance last said they
/// were, and the update replaces them wholesale, so a peer whose count went
/// *down* moves this one's down too. Both statements run on one transaction;
/// a poll id naming no stored poll matches nothing and changes nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); the rollback leaves the
/// poll and its options exactly as they were.
pub async fn record_remote_totals(
    pool: &PgPool,
    poll_id: Uuid,
    totals: &RemoteTotals,
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(REMOTE_TOTALS_SQL)
        .bind(poll_id)
        .bind(totals.votes_count)
        .bind(totals.voters_count)
        .bind(totals.expires_at)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    for (title, votes) in &totals.options {
        sqlx::query(REMOTE_OPTION_SQL)
            .bind(poll_id)
            .bind(title)
            .bind(*votes)
            .execute(&mut *tx)
            .await
            .map(drop)?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;
    use sqlx::{Error as SqlxError, PgPool};

    use super::*;
    use crate::{
        error::AppError,
        models::{
            poll::{get_for_status, options, test_support::*},
            status::{self, NewStatus},
        },
    };

    #[tokio::test]
    async fn create_round_trips_the_poll_and_starts_at_the_defaults() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let posted = create_status(&pool, &alice).await;
        let created = create_for(&pool, posted.id, &params(&["yes", "no"])).await;

        assert!(created.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(created.status_id, posted.id, "the poll names its status");
        assert!(
            created.expires_at.is_none(),
            "no expiry given, no deadline stored"
        );
        assert!(!created.multiple, "single choice by default");
        assert!(!created.hide_totals, "totals shown by default");
        assert!(
            !created.ended_notified.get(),
            "a fresh poll was never swept"
        );
        assert_eq!(created.voters_count, 0, "a fresh poll has no voters");
        assert_eq!(created.votes_count, 0, "a fresh poll has no votes");

        let read = get_for_status(&pool, posted.id)
            .await
            .expect("the poll reads")
            .expect("the created poll is there");
        assert_eq!(read, created, "get_for_status answers the same row");
        let stamped = sqlx::query_scalar::<_, bool>(
            "SELECT inserted_at <= now() AND inserted_at >= now() - interval '1 minute'
               FROM polls WHERE id = $1",
        )
        .bind(created.id)
        .fetch_one(&pool)
        .await
        .expect("the timestamp reads");
        assert!(
            stamped,
            "`create` stamps `inserted_at`, which has no default"
        );

        assert!(
            get_for_status(&pool, new_id())
                .await
                .expect("reads")
                .is_none(),
            "a status id that names no status has no poll"
        );
    }

    #[tokio::test]
    async fn create_stores_every_parameter_it_is_given() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let posted = create_status(&pool, &alice).await;
        let expires = Utc.with_ymd_and_hms(2026, 12, 1, 12, 0, 0).unwrap();
        let created = create_for(
            &pool,
            posted.id,
            &PollParams {
                options: vec!["tea".to_owned(), "coffee".to_owned()],
                expires_at: Some(expires),
                multiple: true,
                hide_totals: true,
                ..PollParams::default()
            },
        )
        .await;

        assert_eq!(
            created.expires_at,
            Some(expires),
            "the deadline round trips"
        );
        assert!(created.multiple, "many choices per voter round trip");
        assert!(created.hide_totals, "the hidden-totals flag round trips");
    }

    #[tokio::test]
    async fn create_runs_on_the_caller_s_transaction() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let mut tx = pool.begin().await.expect("a transaction opens");
        let posted = status::insert_conn(&mut tx, BASE, &NewStatus::local(&alice, "poll me"))
            .await
            .expect("the status inserts on the transaction");
        let created = create(&mut tx, posted.id, &params(&["yes", "no"]))
            .await
            .expect("the poll inserts on the same transaction");
        assert_eq!(created.status_id, posted.id, "the poll names that status");
        tx.rollback().await.expect("the rollback runs");

        assert!(
            status::get_by_id(&pool, posted.id)
                .await
                .expect("reads")
                .is_none(),
            "the rolled-back status was never stored"
        );
        assert!(
            get_for_status(&pool, posted.id)
                .await
                .expect("reads")
                .is_none(),
            "and neither was its poll: one transaction, one outcome"
        );
        assert!(
            options(&pool, created.id).await.expect("reads").is_empty(),
            "nor the options it wrote"
        );
    }

    #[tokio::test]
    async fn create_needs_a_status_that_exists_and_is_the_status_s_only_one() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let err = create_for_err(&pool, new_id(), &params(&["a"])).await;
        let AppError::Database(SqlxError::Database(db_err)) = err else {
            panic!("a status_id collision must surface as a database error")
        };
        assert_eq!(
            db_err.code().as_deref(),
            Some("23503"),
            "the failure is the statuses foreign key"
        );

        let alice = create_user(&pool, "alice").await;
        let posted = create_status(&pool, &alice).await;
        let existing = create_for(&pool, posted.id, &params(&["a"])).await;
        assert_eq!(existing.status_id, posted.id, "the first poll is stored");
        let again = create_for_err(&pool, posted.id, &params(&["b"])).await;
        let AppError::Database(SqlxError::Database(duplicate)) = again else {
            panic!("a second poll on one status must surface as a database error")
        };
        assert_eq!(
            duplicate.code().as_deref(),
            Some("23505"),
            "and the failure is the unique key on status_id"
        );
    }

    /// [`create_for`] that expects the write to fail, for the two constraint
    /// tests.
    async fn create_for_err(pool: &PgPool, status_id: Uuid, given: &PollParams) -> AppError {
        let mut conn = pool.acquire().await.expect("slot connection");
        create(&mut conn, status_id, given)
            .await
            .expect_err("this create must fail")
    }

    #[tokio::test]
    async fn a_position_is_taken_once_per_poll() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let posted = create_status(&pool, &alice).await;
        let created = create_for(&pool, posted.id, &params(&["one"])).await;

        let err = sqlx::query(
            "INSERT INTO poll_options (id, poll_id, position, title) VALUES ($1, $2, $3, $4)",
        )
        .bind(new_id())
        .bind(created.id)
        .bind(0_i32)
        .bind("also one")
        .execute(&pool)
        .await
        .expect_err("one poll cannot hold position 0 twice");
        let SqlxError::Database(db_err) = err else {
            panic!("a position collision must surface as a database error")
        };
        assert_eq!(
            db_err.code().as_deref(),
            Some("23505"),
            "the failure is the (poll_id, position) unique key"
        );
    }

    #[tokio::test]
    async fn remote_totals_overwrite_the_counts_by_title_and_keep_the_deadline() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let author = create_user(&pool, "remote_poll_author").await;
        let posted = create_status(&pool, &author).await;
        let stand = create_for(&pool, posted.id, &params(&["Tea", "Coffee"])).await;

        // A local mark exists; the remote restatement is not asked to keep
        // it, and a title it does not name moves nothing.
        record_remote_totals(
            &pool,
            stand.id,
            &RemoteTotals {
                options: vec![
                    ("Tea".to_owned(), 30),
                    ("Coffee".to_owned(), 15),
                    ("Nobody".to_owned(), 99),
                ],
                voters_count: 42,
                votes_count: 45,
                expires_at: None,
            },
        )
        .await
        .expect("the remote totals write");

        let reloaded = get_for_status(&pool, posted.id)
            .await
            .expect("the poll read")
            .expect("the poll is stored");
        assert_eq!(reloaded.voters_count, 42, "the voter count is replaced");
        assert_eq!(reloaded.votes_count, 45, "and the total");
        let rows = options(&pool, stand.id).await.expect("the options read");
        let counts: Vec<i32> = rows.iter().map(|option| option.votes_count).collect();
        assert_eq!(counts, vec![30, 15], "each named option, title by title");
        // A further update names a deadline: it lands, and a lower count
        // restates downward — the remote's word is authoritative.
        let deadline = Utc.with_ymd_and_hms(2026, 12, 2, 12, 0, 0).unwrap();
        record_remote_totals(
            &pool,
            stand.id,
            &RemoteTotals {
                options: vec![("Tea".to_owned(), 5)],
                voters_count: 6,
                votes_count: 6,
                expires_at: Some(deadline),
            },
        )
        .await
        .expect("the second restatement writes");
        let restated = get_for_status(&pool, posted.id)
            .await
            .expect("the poll read")
            .expect("the poll is stored");
        assert_eq!(restated.votes_count, 6, "a total may go down");
        assert_eq!(restated.expires_at, Some(deadline), "and a deadline lands");
        let options = options(&pool, stand.id).await.expect("the options read");
        assert_eq!(
            options[1].votes_count, 15,
            "an option the update omits is left alone"
        );
    }

    #[tokio::test]
    async fn remote_totals_move_nothing_for_an_unknown_poll() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        record_remote_totals(
            &pool,
            new_id(),
            &RemoteTotals {
                options: vec![("Tea".to_owned(), 1)],
                ..RemoteTotals::default()
            },
        )
        .await
        .expect("the write runs against no row");
    }
}
