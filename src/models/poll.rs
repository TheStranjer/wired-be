//! The `polls`, `poll_options` and `poll_votes` tables: the question a status
//! asks and the votes cast on it.
//!
//! Reference: Pleroma keeps a poll inside the status's object — a `Question`
//! whose `oneOf`/`anyOf` list its options, built by `make_poll_data` of
//! `lib/pleroma/web/common_api/utils.ex` and read back by
//! `lib/pleroma/web/mastodon_api/views/poll_view.ex`, with each vote an
//! `Answer` activity in the object's collection. This module lays the same
//! thing out relationally (`docs/ARCHITECTURE.md` decisions 3 and 6): one poll
//! per status, one row per option, one row per mark, so ticking a box counts
//! a row instead of rewriting the post.
//!
//! Every id is a `UUIDv7` made in Rust (decision 4), which is why
//! [`create`] takes `&mut PgConnection` rather than a pool: a poll is born
//! inside the transaction that inserts its status, and a borrow of a pooled
//! connection or of an open transaction both reach it the same way.
//!
//! The numbers the Mastodon `Poll` entity reports — the poll's
//! [`voters_count`](Poll::voters_count) and [`votes_count`](Poll::votes_count)
//! and each option's [`votes_count`](PollOption::votes_count) — are columns,
//! not computed reads: the vote endpoint's transaction moves them. This layer
//! stores what it is given, including
//! [`expires_at`](PollParams::expires_at): the API's `poll[expires_in]`
//! seconds-to-instant conversion belongs to the status-creation ticket, not
//! here.
//!
//! Alongside the writes, the reads the entity needs: [`get_for_status`] for
//! one status, [`for_statuses`] for a batch of them (the timeline's single
//! query, mirroring [`crate::models::media_attachment::for_statuses`]),
//! [`options`] for a poll's choices in order, and
//! [`voted_option_positions`] — added by this ticket too — for the positions
//! the viewer marked, which is the entity's `voted` flag and `own_votes`
//! array. The render batch's page-shaped mates, [`options_for_statuses`] and
//! [`votes_by_status`], answer the whole page with the same reads in one
//! statement each, keyed by status id like [`for_statuses`].
//!
//! The vote endpoint's ticket (WBE-106) lands the reads and the write the
//! `POST /api/v1/polls/{id}/votes` transaction is made of: [`get_by_id`]
//! resolves the poll the route is keyed on, [`has_voted`] answers the
//! endpoint's already-voted refusal, and [`cast_vote`] is the one
//! transaction that inserts the voter's marks and moves every counter the
//! entity reports — locking the poll row first, so two concurrent ballots
//! from one account serialize and the second is refused rather than
//! double-counted.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    util::id::new_id,
};

/// The `INSERT` behind [`create`]: the minted id, the status named, and what
/// [`PollParams`] holds, with `inserted_at` stamped by the database's `now()`
/// (the column has no default) and the seven read columns brought back as
/// stored — the two counters at their column defaults, since a fresh poll has
/// no votes.
const INSERT_POLL_SQL: &str = "INSERT INTO polls (
     id, status_id, expires_at, multiple, hide_totals, inserted_at
) VALUES (
     $1, $2, $3, $4, $5, now()
) RETURNING id, status_id, expires_at, multiple, hide_totals,
            voters_count, votes_count";

/// The option rows behind [`create`]: one per title, in the order given, each
/// with the id minted for it and a `position` counting from zero — `(ord - 1)`
/// of [`WITH ORDINALITY`]. The whole list goes in one statement, so a poll's
/// options appear all at once with it.
///
/// [`WITH ORDINALITY`]: https://www.postgresql.org/docs/current/functions-array.html
const INSERT_OPTIONS_SQL: &str = "INSERT INTO poll_options (id, poll_id, position, title)
     SELECT o.id, $1, (o.ord - 1)::integer, o.title
       FROM unnest($2::uuid[], $3::text[]) WITH ORDINALITY AS o(id, title, ord)";

/// One row of `polls`: the poll a status carries, without its options or its
/// voters.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Poll {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4).
    pub id: Uuid,
    /// The status asking the question; unique, so a status has one poll at
    /// most and deleting it takes the poll with it.
    pub status_id: Uuid,
    /// When voting closes; `None` for a poll that never expires.
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether a voter may mark more than one option.
    pub multiple: bool,
    /// Whether per-option counts stay hidden while the poll runs.
    pub hide_totals: bool,
    /// Distinct accounts that have voted; moved by the vote transaction.
    pub voters_count: i32,
    /// Votes cast in total, which passes [`voters_count`](Self::voters_count)
    /// as soon as [`multiple`](Self::multiple) lets a voter tick twice.
    pub votes_count: i32,
}

/// One row of `poll_options`: one choice of a poll.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct PollOption {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); a vote names
    /// this id.
    pub id: Uuid,
    /// The poll this option belongs to; deleting the poll deletes its options.
    pub poll_id: Uuid,
    /// 0-based order within the poll: the index a client votes by and the
    /// order [`options`] answers in.
    pub position: i32,
    /// The option's text as its author wrote it.
    pub title: String,
    /// Votes this option has; moved by the vote transaction, `0` when the
    /// poll was created.
    pub votes_count: i32,
}

/// What to create a poll from: every column of `polls` except the id
/// [`create`] mints, the `status_id` it is handed and the counters the
/// database starts at zero.
#[derive(Clone, Debug, Default)]
pub struct PollParams {
    /// The choices, in the order voters see them — [`create`] writes one
    /// `poll_options` row each, at position `0`, `1`, … Empty is allowed
    /// here (the endpoint's minimum-options check is the API's, not this
    /// layer's).
    pub options: Vec<String>,
    /// When voting closes; `None` for a poll that never expires. The seconds
    /// a client sends are converted before this struct is built.
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether a voter may mark more than one option; `false` by default.
    pub multiple: bool,
    /// Whether per-option counts stay hidden until the poll closes; `false`
    /// by default.
    pub hide_totals: bool,
}

/// Creates the poll `params` describes for status `status_id`, on `conn`, and
/// returns its row.
///
/// The poll's id and one id per option are fresh `UUIDv7`s (decision 4); the
/// options are written in the order given, at positions counting from zero, in
/// one statement on `conn` — so a caller that inserts a status there gets
/// status, poll and options in one transaction, and a rollback takes all three
/// back. Both counters start at the column defaults (`0`), and `inserted_at` is
/// the database's `now()`.
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
        .fetch_one(&mut *conn)
        .await?;
    let ids: Vec<Uuid> = params.options.iter().map(|_| new_id()).collect();
    sqlx::query(INSERT_OPTIONS_SQL)
        .bind(poll.id)
        .bind(&ids)
        .bind(&params.options)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(poll)
}

/// Fetches the poll of status `status_id`, `None` when that status carries
/// none (or is not stored at all).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_for_status(pool: &PgPool, status_id: Uuid) -> AppResult<Option<Poll>> {
    let stored = sqlx::query_as::<_, Poll>(
        "SELECT id, status_id, expires_at, multiple, hide_totals, voters_count, votes_count
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
        "SELECT id, status_id, expires_at, multiple, hide_totals, voters_count, votes_count
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
                                       voters_count, votes_count
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
/// [`cast_vote`] runs the same probe again inside its transaction, so the
/// endpoint's answer stays honest under concurrent requests.
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

/// [`cast_vote`]'s first read: the poll's own row, locked for the rest of
/// the transaction. Every ballot on one poll takes this lock first, so two
/// voters (or one voter's two racing requests) move the counters one at a
/// time, and a poll deleted between the endpoint's reads and this lock is
/// caught here.
const LOCK_POLL_SQL: &str = "SELECT id, status_id, expires_at, multiple, hide_totals,
                                    voters_count, votes_count
                               FROM polls
                              WHERE id = $1
                                FOR UPDATE";

/// [`cast_vote`]'s in-transaction rerun of [`HAS_VOTED_SQL`]: taken under
/// the poll's row lock, it is what makes the endpoint's already-voted
/// refusal hold even for two requests that raced past the cheap read.
const VOTER_MARKED_SQL: &str = HAS_VOTED_SQL;

/// [`cast_vote`]'s marks: one `poll_votes` row per chosen option, all in
/// one statement on the transaction — the same [`WITH ORDINALITY`]-style
/// `unnest` write [`create`] uses for the options, without the position
/// because the triple `(poll_id, option_id, user_id)` is the whole fact.
///
/// [`WITH ORDINALITY`]: https://www.postgresql.org/docs/current/functions-array.html
const INSERT_MARKS_SQL: &str = "INSERT INTO poll_votes (poll_id, option_id, user_id, inserted_at)
     SELECT $1, o.id, $2, now()
       FROM unnest($3::uuid[]) AS o(id)";

/// [`cast_vote`]'s per-option bump: each chosen option gains exactly one
/// vote. The `poll_id` guard keeps a mistargeted option id from moving a
/// row of another poll; the endpoint only ever passes ids read from this
/// poll's own [`options`].
const BUMP_OPTIONS_SQL: &str = "UPDATE poll_options SET votes_count = votes_count + 1
     WHERE poll_id = $1 AND id = ANY($2)";

/// [`cast_vote`]'s roll-up bump: `votes_count` gains one per mark,
/// `voters_count` gains the one new voter. Both moves are in-statement
/// arithmetic on the stored column, so racing ballots on one poll sum
/// instead of overwriting each other — on top of the row lock that makes
/// every voter's marks land as one indivisible ballot.
const BUMP_POLL_SQL: &str = "UPDATE polls SET votes_count = votes_count + $2,
        voters_count = voters_count + 1
    WHERE id = $1";

/// Records `user_id`'s ballot on poll `poll_id` as one transaction.
///
/// The write is the marks themselves, each chosen option's
/// [`votes_count`](PollOption::votes_count) stepped, and the poll's
/// roll-up columns — [`votes_count`](Poll::votes_count) by the number of
/// options marked, [`voters_count`](Poll::voters_count) by the one new
/// voter.
///
/// `option_ids` must name options of `poll_id`, deduplicated by the caller
/// (a voter cannot tick the same option twice, and the primary key would
/// refuse the attempt anyway); the endpoint derives them from the parsed
/// choices before it gets here. The answer is `true` when the ballot was
/// recorded and `false` when the voter already had a mark on this poll —
/// detected under the poll's row lock, which is what serializes racing
/// requests into one ballot and one refusal. A poll that was deleted
/// between the caller's read and this write is [`AppError::NotFound`];
/// nothing was changed in either refused case, because the whole write
/// rolls back.
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
    if locked.is_none() {
        tx.rollback().await?;
        return Err(AppError::NotFound);
    }
    let already_voted: bool = sqlx::query_scalar::<_, bool>(VOTER_MARKED_SQL)
        .bind(poll_id)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    if already_voted {
        tx.rollback().await?;
        return Ok(false);
    }
    let marks = i32::try_from(option_ids.len()).map_err(|_overflow| {
        AppError::Internal(format!(
            "ballot of {} marks is longer than an i32",
            option_ids.len()
        ))
    })?;
    sqlx::query(INSERT_MARKS_SQL)
        .bind(poll_id)
        .bind(user_id)
        .bind(option_ids)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    sqlx::query(BUMP_OPTIONS_SQL)
        .bind(poll_id)
        .bind(option_ids)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    sqlx::query(BUMP_POLL_SQL)
        .bind(poll_id)
        .bind(marks)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;
    use sqlx::{Error as SqlxError, PgPool};

    use super::*;
    use crate::{
        error::AppError,
        models::{
            status::{self, NewStatus, Status},
            user::{NewUser, User, insert as insert_user, local_ap_id},
        },
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = "https://test.example";

    /// Inserts a local user named `nickname`, because a vote names one.
    async fn create_user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("fixture user inserts")
    }

    /// Inserts a local status of `author`, because `status_id` is a foreign
    /// key every poll names.
    async fn create_status(pool: &PgPool, author: &User) -> Status {
        status::insert(pool, BASE, &NewStatus::local(author, "a poll question"))
            .await
            .expect("fixture status inserts")
    }

    /// [`PollParams`] for the given option titles, nothing else set.
    fn params(titles: &[&str]) -> PollParams {
        let options = titles.iter().map(|title| (*title).to_owned()).collect();
        PollParams {
            options,
            ..PollParams::default()
        }
    }

    /// [`create`] through a connection of the pool's own, the way a caller
    /// outside a transaction reaches it.
    async fn create_for(pool: &PgPool, status_id: Uuid, params: &PollParams) -> Poll {
        let mut conn = pool.acquire().await.expect("slot connection");
        create(&mut conn, status_id, params)
            .await
            .expect("fixture poll creates")
    }

    /// Records a mark straight into `poll_votes`: the vote endpoint that
    /// normally writes it is a later ticket, and the cascade and the
    /// voter's-flag reads need votes to exist.
    async fn vote(pool: &PgPool, poll_id: Uuid, option_id: Uuid, user_id: Uuid) {
        let inserted = sqlx::query(
            "INSERT INTO poll_votes (poll_id, option_id, user_id, inserted_at)
             VALUES ($1, $2, $3, now())",
        )
        .bind(poll_id)
        .bind(option_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("fixture vote inserts");
        assert_eq!(inserted.rows_affected(), 1, "the mark was written");
    }

    /// Runs a one-binding `SELECT count(*)` — the shape every cascade test
    /// asks ("how many rows of this table still name that id?"). The query is
    /// a literal at every call site, which is what lets it reach
    /// `sqlx::query_scalar` as `'static` text.
    async fn count_for(pool: &PgPool, sql: &'static str, id: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("the count reads")
    }

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

    /// The options mapped for `status_id`, as `(position, title)` pairs —
    /// the shape the batched grouping tests compare.
    fn positions_and_titled(
        map: &HashMap<Uuid, Vec<PollOption>>,
        status_id: Uuid,
    ) -> Vec<(i32, String)> {
        map.get(&status_id)
            .map(|options| {
                options
                    .iter()
                    .map(|option| (option.position, option.title.clone()))
                    .collect()
            })
            .unwrap_or_default()
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

    /// The options of `poll_id` as `(position, votes_count)`, the shape the
    /// bump assertions compare.
    async fn option_counts(pool: &PgPool, poll_id: Uuid) -> Vec<(i32, i32)> {
        options(pool, poll_id)
            .await
            .expect("the options read")
            .into_iter()
            .map(|option| (option.position, option.votes_count))
            .collect()
    }

    #[tokio::test]
    async fn cast_vote_records_the_ballot_and_moves_every_count() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let posted = create_status(&pool, &alice).await;
        let stand = create_for(
            &pool,
            posted.id,
            &PollParams {
                options: vec!["tea".to_owned(), "coffee".to_owned(), "water".to_owned()],
                multiple: true,
                ..PollParams::default()
            },
        )
        .await;
        let choices = options(&pool, stand.id).await.expect("options read");
        let ids: Vec<Uuid> = choices.iter().map(|choice| choice.id).collect();

        // Bob marks the first and the third option in one ballot.
        assert!(
            cast_vote(&pool, stand.id, bob.id, &[ids[0], ids[2]])
                .await
                .expect("the ballot is recorded"),
            "a first ballot answers recorded, not refused"
        );
        let after = get_by_id(&pool, stand.id)
            .await
            .expect("the poll reads")
            .expect("the poll stands");
        assert_eq!(after.votes_count, 2, "one vote per mark");
        assert_eq!(after.voters_count, 1, "one voter however many marks");
        assert_eq!(
            option_counts(&pool, stand.id).await,
            vec![(0, 1), (1, 0), (2, 1)],
            "each chosen option gained exactly one"
        );
        assert_eq!(
            voted_option_positions(&pool, stand.id, bob.id)
                .await
                .expect("the marks read"),
            vec![0, 2],
            "the marks sit on the chosen options under the voter"
        );
        assert_eq!(
            count_for(
                &pool,
                "SELECT count(*) FROM poll_votes WHERE poll_id = $1",
                stand.id
            )
            .await,
            2,
            "one row per mark"
        );
        let stamped = sqlx::query_scalar::<_, bool>(
            "SELECT bool_and(inserted_at <= now() AND inserted_at >= now() - interval '1 minute')
               FROM poll_votes WHERE poll_id = $1",
        )
        .bind(stand.id)
        .fetch_one(&pool)
        .await
        .expect("the timestamps read");
        assert!(
            stamped,
            "the write stamps `inserted_at`, which has no default"
        );
    }

    #[tokio::test]
    async fn cast_vote_sums_separate_ballots_into_the_same_counts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let carol = create_user(&pool, "carol").await;
        let posted = create_status(&pool, &alice).await;
        let stand = create_for(&pool, posted.id, &params(&["tea", "coffee"])).await;
        let choices = options(&pool, stand.id).await.expect("options read");
        let ids: Vec<Uuid> = choices.iter().map(|choice| choice.id).collect();

        assert!(
            cast_vote(&pool, stand.id, bob.id, &[ids[0]])
                .await
                .expect("the first ballot is recorded"),
            "bob's mark lands"
        );
        assert!(
            cast_vote(&pool, stand.id, carol.id, &[ids[1]])
                .await
                .expect("the second ballot is recorded"),
            "a different voter is no collision"
        );
        let summed = get_by_id(&pool, stand.id)
            .await
            .expect("the poll reads")
            .expect("the poll stands");
        assert_eq!(summed.votes_count, 2, "one vote per ballot");
        assert_eq!(summed.voters_count, 2, "each voter counts once");
        assert_eq!(
            option_counts(&pool, stand.id).await,
            vec![(0, 1), (1, 1)],
            "and the options sum the ballots"
        );
    }

    #[tokio::test]
    async fn cast_vote_refuses_the_second_ballot_of_one_voter_without_effects() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let posted = create_status(&pool, &alice).await;
        let stand = create_for(&pool, posted.id, &params(&["tea", "coffee"])).await;
        let choices = options(&pool, stand.id).await.expect("options read");
        let ids: Vec<Uuid> = choices.iter().map(|choice| choice.id).collect();

        assert!(
            cast_vote(&pool, stand.id, bob.id, &[ids[0]])
                .await
                .expect("the first ballot is recorded"),
            "the first ballot lands"
        );

        assert!(
            !cast_vote(&pool, stand.id, bob.id, &[ids[1]])
                .await
                .expect("the second ballot is answered"),
            "the same voter's second ballot is refused, whichever option it names"
        );
        let after = get_by_id(&pool, stand.id)
            .await
            .expect("the poll reads")
            .expect("the poll stands");
        assert_eq!(after.votes_count, 1, "the refusal changed no count");
        assert_eq!(after.voters_count, 1, "in either column");
        assert_eq!(
            option_counts(&pool, stand.id).await,
            vec![(0, 1), (1, 0)],
            "nor any option's share"
        );
        assert_eq!(
            voted_option_positions(&pool, stand.id, bob.id)
                .await
                .expect("the marks read"),
            vec![0],
            "and wrote no second mark"
        );
    }

    #[tokio::test]
    async fn cast_vote_needs_a_poll_that_is_still_stored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;

        let err = cast_vote(&pool, new_id(), bob.id, &[])
            .await
            .expect_err("a poll that is not stored cannot take a ballot");
        assert!(
            matches!(err, AppError::NotFound),
            "the gone poll is the 404, not a database failure: {err}"
        );

        // And a poll deleted after the endpoint read it: the lock catches
        // it, and the half-open transaction leaves nothing behind.
        let posted = create_status(&pool, &alice).await;
        let stand = create_for(&pool, posted.id, &params(&["tea"])).await;
        let choice = options(&pool, stand.id)
            .await
            .expect("options read")
            .first()
            .expect("the fixture poll has its option")
            .id;
        let removed = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(posted.id)
            .execute(&pool)
            .await
            .expect("the status delete runs");
        assert_eq!(removed.rows_affected(), 1, "the fixture status was live");
        let cascaded = cast_vote(&pool, stand.id, bob.id, &[choice])
            .await
            .expect_err("the cascaded poll is gone at the lock");
        assert!(
            matches!(cascaded, AppError::NotFound),
            "a poll deleted under the endpoint is the 404: {cascaded}"
        );
    }
}
