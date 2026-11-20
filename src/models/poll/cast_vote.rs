//! The vote endpoint's one transaction: [`cast_vote`] locking the poll row,
//! deciding with [`fresh_marks`] what the ballot actually adds, inserting
//! those marks and moving every counter the `Poll` entity reports.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

use super::Poll;

/// [`cast_vote`]'s first read: the poll's own row, locked for the rest of
/// the transaction. Every ballot on one poll takes this lock first, so two
/// voters (or one voter's two racing requests) move the counters one at a
/// time, and a poll deleted between the endpoint's reads and this lock is
/// caught here.
const LOCK_POLL_SQL: &str = "SELECT id, status_id, expires_at, multiple, hide_totals,
                                    voters_count, votes_count, ended_notified
                               FROM polls
                              WHERE id = $1
                                FOR UPDATE";

/// [`cast_vote`]'s in-transaction read of the voter's own marks: which
/// options of the poll that account has already ticked, taken under the
/// poll's row lock. Its emptiness is what makes a ballot a voter's *first* —
/// the only one that raises `voters_count` — and its contents are what no
/// later ballot may tick twice.
const VOTER_MARKS_SQL: &str =
    "SELECT option_id FROM poll_votes WHERE poll_id = $1 AND user_id = $2";

/// [`cast_vote`]'s marks: one `poll_votes` row per chosen option, all in
/// one statement on the transaction — the same [`WITH ORDINALITY`]-style
/// `unnest` write [`create`](crate::models::poll::create) uses for the options, without
/// the position because the triple `(poll_id, option_id, user_id)` is the
/// whole fact.
///
/// [`WITH ORDINALITY`]: https://www.postgresql.org/docs/current/functions-array.html
const INSERT_MARKS_SQL: &str = "INSERT INTO poll_votes (poll_id, option_id, user_id, inserted_at)
     SELECT $1, o.id, $2, now()
       FROM unnest($3::uuid[]) AS o(id)";

/// [`cast_vote`]'s per-option bump: each chosen option gains exactly one
/// vote. The `poll_id` guard keeps a mistargeted option id from moving a
/// row of another poll; both callers only ever pass ids read from this
/// poll's own [`options`](crate::models::poll::options).
const BUMP_OPTIONS_SQL: &str = "UPDATE poll_options SET votes_count = votes_count + 1
     WHERE poll_id = $1 AND id = ANY($2)";

/// [`cast_vote`]'s roll-up bump: `votes_count` gains one per mark,
/// `voters_count` gains the one voter a first ballot brings — `$3`, which is
/// `0` for every later mark the same account adds to a
/// [`multiple`](Poll::multiple) poll, because that voter is already counted.
/// Both moves are in-statement arithmetic on the stored column, so racing
/// ballots on one poll sum instead of overwriting each other — on top of the
/// row lock that makes every ballot's marks land as one indivisible write.
const BUMP_POLL_SQL: &str = "UPDATE polls SET votes_count = votes_count + $2,
        voters_count = voters_count + $3
    WHERE id = $1";

/// Whether [`cast_vote`] would record `option_ids` for `user_id`, and the
/// marks it would actually write: `Some` of the options the voter has not
/// ticked yet, `None` when the ballot marks nothing new. A poll that is not
/// [`Poll::multiple`] takes one ballot per voter, so any earlier mark
/// refuses a later one whole; a [`multiple`](Poll::multiple) poll lets a
/// voter's later ballots add options, which is how a federated vote arrives
/// — one `Create` per chosen option — and why the voter's marks, not the
/// voter, are what a repeat cannot double.
fn fresh_marks(multiple: bool, marked: &[Uuid], option_ids: &[Uuid]) -> Option<Vec<Uuid>> {
    if !marked.is_empty() && !multiple {
        return None;
    }
    let fresh: Vec<Uuid> = option_ids
        .iter()
        .copied()
        .filter(|id| !marked.contains(id))
        .collect();
    (!fresh.is_empty()).then_some(fresh)
}

/// Records `user_id`'s ballot on poll `poll_id` as one transaction.
///
/// The write is the marks themselves, each chosen option's
/// [`votes_count`](crate::models::poll::PollOption::votes_count) stepped, and the poll's
/// roll-up columns — [`votes_count`](Poll::votes_count) by the number of
/// options marked, [`voters_count`](Poll::voters_count) by the one voter a
/// first ballot brings. A voter's later marks on a [`multiple`](Poll::multiple)
/// poll therefore move the counts of the options they name and the poll's
/// vote total without counting the voter twice, which is what an inbound
/// [`Create`](crate::federation::handlers::vote) of one answer needs and what
/// [`crate::federation::handlers::vote`] lands through this one function
/// beside the endpoint.
///
/// `option_ids` must name options of `poll_id`, deduplicated by the caller
/// (a voter cannot tick the same option twice, and the primary key would
/// refuse the attempt anyway); the endpoint derives them from the parsed
/// choices before it gets here. The answer is `true` when the ballot was
/// recorded and `false` when nothing new was: a voter with an earlier mark
/// on a poll that is not [`multiple`](Poll::multiple), or a ballot naming
/// only options that voter has ticked already. Both are detected under the
/// poll's row lock, which is what serializes racing requests into one write
/// per ballot and one refusal. A poll that was deleted between the caller's
/// read and this write is [`AppError::NotFound`]; nothing was changed in
/// either refused case, because the whole write rolls back.
///
/// # Errors
///
/// [`AppError::NotFound`] when the poll row is gone at the lock;
/// [`AppError::Internal`] for a ballot too long to count in an `i32`
/// (unreachable through the API, whose polls cap at twenty options);
/// any other database failure (connection, constraint, encoding) arrives
/// as [`AppError::Database`](crate::error::AppError).
pub async fn cast_vote(
    pool: &PgPool,
    poll_id: Uuid,
    user_id: Uuid,
    option_ids: &[Uuid],
) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    let locked: Option<Poll> = sqlx::query_as::<_, Poll>(LOCK_POLL_SQL)
        .bind(poll_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(stand) = locked else {
        tx.rollback().await?;
        return Err(AppError::NotFound);
    };
    let marked: Vec<Uuid> = sqlx::query_scalar::<_, Uuid>(VOTER_MARKS_SQL)
        .bind(poll_id)
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await?;
    let Some(fresh) = fresh_marks(stand.multiple, &marked, option_ids) else {
        tx.rollback().await?;
        return Ok(false);
    };
    let marks = i32::try_from(fresh.len()).map_err(|_overflow| {
        AppError::Internal(format!(
            "ballot of {} marks is longer than an i32",
            option_ids.len()
        ))
    })?;
    let voter = i32::from(marked.is_empty());
    sqlx::query(INSERT_MARKS_SQL)
        .bind(poll_id)
        .bind(user_id)
        .bind(&fresh)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    sqlx::query(BUMP_OPTIONS_SQL)
        .bind(poll_id)
        .bind(&fresh)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    sqlx::query(BUMP_POLL_SQL)
        .bind(poll_id)
        .bind(marks)
        .bind(voter)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests;
