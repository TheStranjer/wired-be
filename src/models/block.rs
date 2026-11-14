//! The `blocks` table: which account refuses which, and what writing that
//! fact does to the follows between the pair.
//!
//! Per `docs/ARCHITECTURE.md` decision 6 a block is one of the relational
//! facts that federate, so a row carries the `ActivityPub` id of the `Block`
//! activity behind it: `{BASE_URL}/activities/{uuid}` for a blocker this
//! instance owns and `None` for a remote one until the federation ticket that
//! mirrors an incoming activity records the id it sent. The reference
//! behaviour is Pleroma's `User.block` / `User.unblock`
//! (`lib/pleroma/user.ex`) over the blocking edge of
//! `Pleroma.UserRelationship` (`lib/pleroma/user_relationship.ex`), reached
//! from the Mastodon `AccountController`'s `block`/`unblock` actions
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`).
//!
//! Blocking severs the pair (`User.block`'s "sever any follow relationships to
//! prevent leaks per activitypub"): the two accounts' accepted follows of each
//! other and both directions' pending follow requests are removed, with the
//! denormalised counters stepped down for every accepted relationship that
//! goes. [`block`] does that in one transaction, so the block row, the follow
//! rows it deletes and the counters those rows fed are written or rolled back
//! together; Pleroma performs the same set as a sequence of separate writes.
//! The follow-side half of that write belongs to the table it owns and lives
//! in [`crate::models::follow::sever_between`].
//!
//! Both writes are idempotent on the ticket's `(blocker_id, blocked_id)` key:
//! a pair that is already blocked keeps its row — and its id, so the listing's
//! cursor does not move, and its `ap_id`, so the activity already federated is
//! not replaced — and a repeat call writes nothing, while [`unblock`] deletes
//! whatever pair it is given and is content with none. Deleting either account
//! takes its blocks through the foreign key's cascade.
//!
//! The federation mirror of an inbound `Block` activity adds the two writes
//! that fact needs: [`set_ap_id`] records the remote activity id on the row
//! [`block`] left without one (a remote blocker's id is the one its own
//! instance minted), and [`get_by_ap_id`] reads the block back by that id —
//! what an `Undo` that names only the activity resolves through.
//!
//! The three reads are the ones the endpoints and the guards need:
//! [`is_blocking`] the one directional fact, [`blocked_between`] the same
//! question in either direction (what [`follow`] refuses on), and
//! [`blocked_ids`] / [`blocker_ids`] the flat sets a timeline or an inbox
//! filters by. [`blocking_page`] gives `GET /api/v1/blocks` its page, each
//! entry led by the [`Block`] row next to the [`User`] behind it — the row id
//! is what the cursor and the `Link` header key on (decision 4).
//!
//! Signatures name the two ends of a block `blocker` and `target` (the word
//! [`follow`] borrows for the account it is about), because `blocker` next to
//! `blocked` is one Clippy `similar_names` error; the row keeps the table's
//! own `blocker_id` and `blocked_id` names.

use chrono::{DateTime, Utc};
use sqlx::{Executor, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::{follow, user::User},
    pagination::PageParams,
    util::id::new_id,
};

/// 422 message [`block`] answers when an account asks to block itself, the
/// wording this ticket pins for the endpoint (Pleroma's changeset refuses the
/// self relationship for every edge type the same way).
const SELF_BLOCK: &str = "Can not block yourself";

/// [`block`]'s write when the pair has no row yet: the id this module mints
/// (decision 4) and the pair the caller names, `inserted_at` left to the
/// column default. `ON CONFLICT (blocker_id, blocked_id) DO NOTHING` is the
/// race guard: a concurrent block of the same pair wins, this statement writes
/// nothing and returns no row, and the winner's row is read back instead —
/// which is also why its severing must not run a second time.
const INSERT_SQL: &str = "INSERT INTO blocks (id, blocker_id, blocked_id, ap_id)
     VALUES ($1, $2, $3, $4)
ON CONFLICT (blocker_id, blocked_id) DO NOTHING
RETURNING *";

/// [`unblock`]: the pair's row, which comes back so the caller can report
/// whether a block was there to lift. A never-blocked (or already unblocked)
/// pair deletes nothing, so unblocking is idempotent.
const UNBLOCK_SQL: &str = "DELETE FROM blocks WHERE blocker_id = $1 AND blocked_id = $2
 RETURNING *";

/// [`get`]: the one block of an ordered pair.
const GET_SQL: &str = "SELECT * FROM blocks WHERE blocker_id = $1 AND blocked_id = $2";

/// [`get_by_ap_id`]: the one block behind a `Block` activity id — the
/// `ap_id` unique index answers the lookup directly.
const GET_BY_AP_ID_SQL: &str = "SELECT * FROM blocks WHERE ap_id = $1";

/// [`set_ap_id`]: the mirror write of an inbound `Block` — the row keeps its
/// identity, only the activity id it stands for arrives. Unknown ids match
/// no row and change nothing.
const SET_AP_ID_SQL: &str = "UPDATE blocks SET ap_id = $2 WHERE id = $1";

/// [`is_blocking`]: the one directional fact, asked as an existence check so a
/// blocker never costs more than a `true`.
const IS_BLOCKING_SQL: &str = "SELECT EXISTS (
    SELECT 1 FROM blocks WHERE blocker_id = $1 AND blocked_id = $2
)";

/// [`blocked_between`]: the same question in either direction, in one
/// statement — the pair is blocked as soon as one of the two rows is there.
const BLOCKED_BETWEEN_SQL: &str = "SELECT EXISTS (
    SELECT 1 FROM blocks
     WHERE (blocker_id = $1 AND blocked_id = $2)
        OR (blocker_id = $2 AND blocked_id = $1)
)";

/// [`blocked_ids`]: every account one blocker has blocked, oldest block first
/// (the `UUIDv7` text sorts in creation order, decision 4). The order is a
/// stability promise, not semantics: callers filter by membership and page
/// through [`blocking_page`].
const BLOCKED_IDS_SQL: &str = "SELECT blocked_id FROM blocks WHERE blocker_id = $1 ORDER BY id";

/// [`blocker_ids`]: every account that blocked one blocked account, oldest
/// block first — the incoming side of the same table, which is what the
/// `blocked_id` index serves.
const BLOCKER_IDS_SQL: &str = "SELECT blocker_id FROM blocks WHERE blocked_id = $1 ORDER BY id";

/// The leading text of [`blocking_page`]: the blocked accounts of one blocker,
/// each entry led by the columns of the `blocks` row that put them on the page
/// (aliased, because [`User`] has its own `id`, `ap_id` and `inserted_at`),
/// joined to the account on the other end. The statement stops where its one
/// placeholder belongs, because that is where [`blocking_page`] pushes its bind
/// and the builder is what numbers it `$1`; the keyset
/// [`PageParams::apply_keyset`] then appends runs on the block row's own `id`.
const PAGE_SQL: &str = "SELECT b.id AS block_id, b.blocker_id, b.blocked_id,
                               b.ap_id AS block_ap_id,
                               b.inserted_at AS block_inserted_at, u.*
                          FROM blocks b
                          JOIN users u ON u.id = b.blocked_id
                         WHERE b.blocker_id = ";

/// One row of `blocks`: the `blocker_id` account blocks the `blocked_id`
/// one, plus the `ActivityPub` id of the `Block` activity behind it.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Block {
    /// `UUIDv7` primary key, minted by [`block`] (decision 4); the id
    /// `GET /api/v1/blocks` pages by.
    pub id: Uuid,
    /// The blocking account; every read and write of a block is scoped to it.
    pub blocker_id: Uuid,
    /// The blocked account.
    pub blocked_id: Uuid,
    /// `ActivityPub` id of the `Block` activity behind this row (decision 6):
    /// a `{BASE_URL}/activities/{uuid}` id for a local blocker, the remote id
    /// for a mirrored one, `None` until an activity exists to record.
    pub ap_id: Option<String>,
    /// When the row was inserted; a block is a plain fact, so it carries no
    /// `updated_at` and [`block`] keeps the stored row of an already blocked
    /// pair exactly as it is.
    pub inserted_at: DateTime<Utc>,
}

/// One fetched row of [`blocking_page`]: the [`Block`] columns the page keys on
/// next to the [`User`] they point at, read off [`PAGE_SQL`]'s join and folded
/// into that pair by [`into_entry`].
#[derive(Debug, sqlx::FromRow)]
struct BlockUserRow {
    /// Primary key of the `blocks` row behind this entry, aliased because the
    /// joined account row carries its own `id`.
    block_id: Uuid,
    /// The blocker: `blocks.blocker_id`, the account the page is scoped to.
    blocker_id: Uuid,
    /// The blocked account: `blocks.blocked_id`, the user this entry is about.
    blocked_id: Uuid,
    /// The block's `ActivityPub` id, aliased because [`User`] has its own
    /// `ap_id` (and needs the unaliased column to decode itself).
    block_ap_id: Option<String>,
    /// The block's `inserted_at`, aliased because the user row carries its own
    /// creation timestamp under that name.
    block_inserted_at: DateTime<Utc>,
    /// The blocked account, decoded from the join's unaliased `u.*`.
    #[sqlx(flatten)]
    user: User,
}

/// Records that `blocker` blocks `target`, severing the pair's follows, and
/// returns the block.
///
/// The write is the whole fact this ticket stores: the row (idempotent on the
/// pair — an already blocked pair keeps its row, its id and its
/// [`Block::ap_id`]) and everything Pleroma's `User.block` drops with it, both
/// directions' accepted follows and both directions' pending requests, the
/// denormalised counters of [`crate::models::user::User::followers_count`] and
/// [`following_count`](crate::models::user::User::following_count) fixed for
/// every accepted row that goes. All of it is one transaction: the block row
/// and the follows it severs can never be seen apart.
///
/// `base_url` is the instance origin, i.e. [`Config::base_url`], which the
/// model layer is handed rather than reading configuration itself; a blocker
/// this instance owns mints a fresh `{base_url}/activities/{uuid}` for its
/// `Block` activity (decision 6), a remote one stores `None` until the
/// federation ticket mirrors the activity it arrived with.
///
/// [`Config::base_url`]: crate::config::Config::base_url
/// [`following_count`]: crate::models::user::User::following_count
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `"Can not block yourself"` when the two
/// users are the same account; [`AppError::NotFound`] when a concurrent block
/// of the same pair won the key and then rolled back, leaving no row to read
/// from it. Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn block(
    pool: &PgPool,
    base_url: &str,
    blocker: &User,
    target: &User,
) -> AppResult<Block> {
    if blocker.id == target.id {
        return Err(AppError::Unprocessable(SELF_BLOCK.to_owned()));
    }
    match get(pool, blocker.id, target.id).await? {
        // Already blocked: this call changes nothing, and the pair's follows
        // went when the block was first written.
        Some(existing) => Ok(existing),
        None => write_block(pool, base_url, blocker, target).await,
    }
}

/// The write of one new block: the row and the severing it implies, in one
/// transaction, with the row coming back. A concurrent block of the same pair
/// that won the unique key instead is answered its row, unchanged and
/// unsevered twice, because that call owns the whole write.
async fn write_block(
    pool: &PgPool,
    base_url: &str,
    blocker: &User,
    target: &User,
) -> AppResult<Block> {
    let mut tx = pool.begin().await?;
    let created = sqlx::query_as::<_, Block>(INSERT_SQL)
        .bind(new_id())
        .bind(blocker.id)
        .bind(target.id)
        .bind(local_activity_id(base_url, blocker).as_deref())
        .fetch_optional(&mut *tx)
        .await?;
    let Some(block) = created else {
        // Nothing was written here: the winner's row is the answer, read on
        // this transaction's own connection so the fallback never needs a
        // second one, and the empty transaction ends with the connection
        // (`follow::remove_follower` does the same).
        return required(get(&mut *tx, blocker.id, target.id).await?);
    };
    follow::sever_between(&mut tx, blocker.id, target.id).await?;
    tx.commit().await?;
    Ok(block)
}

/// Lifts `blocker`'s block on `target`, reporting the removed row.
///
/// Unblocking does not restore anything: a severed follow stays severed, the
/// same way Pleroma's `User.unblock` only deletes the edge. A pair with no
/// block (or one already lifted) is the `None` answer and changes nothing, so
/// the write is idempotent.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unblock(pool: &PgPool, blocker: &User, target: &User) -> AppResult<Option<Block>> {
    let removed = sqlx::query_as::<_, Block>(UNBLOCK_SQL)
        .bind(blocker.id)
        .bind(target.id)
        .fetch_optional(pool)
        .await?;
    Ok(removed)
}

/// Records `ap_id` as the `Block` activity the stored block `id` stands
/// for: the federation mirror's write (decision 6).
///
/// [`block`] leaves a remote blocker's row without an id because that id
/// is the one the blocker's own instance minted, and this is where the
/// inbound `Block` activity's id lands on it. The unique `ap_id` index
/// refuses a second block under one id, and an unknown `id` matches no row
/// and changes nothing.
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

/// The block whose `Block` activity carries `ap_id`, `None` when no stored
/// block does: how the federation `Undo` that names only the activity id
/// finds the pair to lift.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> AppResult<Option<Block>> {
    let block = sqlx::query_as::<_, Block>(GET_BY_AP_ID_SQL)
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(block)
}

/// Whether `blocker` has blocked `target`.
///
/// The one directional fact the guards ask for: the follow guard in
/// [`crate::models::follow`] uses it to tell whose block refused a follow, and
/// a pair that blocked each other both ways answers it `true` in both
/// directions.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn is_blocking(pool: &PgPool, blocker: Uuid, target: Uuid) -> AppResult<bool> {
    let row = sqlx::query_scalar::<_, bool>(IS_BLOCKING_SQL)
        .bind(blocker)
        .bind(target)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Whether `first` and `second` have blocked each other, in either direction.
///
/// The question [`follow`] refuses on and the later delivery filters ask: one
/// existence check over the pair's two possible rows, so neither direction is
/// privileged. An unblocked pair — including one account with itself, which
/// has no row to find — answers `false`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn blocked_between(pool: &PgPool, first: Uuid, second: Uuid) -> AppResult<bool> {
    let row = sqlx::query_scalar::<_, bool>(BLOCKED_BETWEEN_SQL)
        .bind(first)
        .bind(second)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The ids of the accounts `blocker` has blocked, oldest block first.
///
/// The whole set in one statement: [`crate::models::relationship`] answers
/// `blocking` from the same table, and the timeline and delivery filters that
/// hide a blocked account's posts take this flat form. A blocker with nothing
/// blocked answers the empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn blocked_ids(pool: &PgPool, blocker: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(BLOCKED_IDS_SQL)
        .bind(blocker)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(blocked_id,)| blocked_id).collect())
}

/// The ids of the accounts that have blocked `blocked`, oldest block first.
///
/// The incoming side of [`blocked_ids`], served by the table's `blocked_id`
/// index: what the `blocked_by` flag, the follow guard and (later) the inbox
/// that must not deliver a blocker's posts ask. An account nobody blocked
/// answers the empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn blocker_ids(pool: &PgPool, blocked: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(BLOCKER_IDS_SQL)
        .bind(blocked)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(blocker_id,)| blocker_id).collect())
}

/// The accounts `blocker` blocks, newest block first and `page`'s `limit` at a
/// time, each entry the [`Block`] row next to the [`User`] it names.
///
/// Like the relationship pages, the cursor and the `Link` header key on the
/// *block row* id and not on the accounts (decision 4), which is why the row
/// comes back beside the user. [`PageParams::finish`] flips a `min_id` fetch
/// back to newest-first, so every page is answered newest block first.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn blocking_page(
    pool: &PgPool,
    blocker: Uuid,
    page: PageParams,
) -> AppResult<Vec<(Block, User)>> {
    let mut builder = QueryBuilder::<Postgres>::new(PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(blocker);
    page.apply_keyset(&mut builder, "b.id");
    let rows = builder
        .build_query_as::<BlockUserRow>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(rows).into_iter().map(into_entry).collect())
}

/// The `ap_id` to store for a new block: a freshly minted
/// `{base_url}/activities/{uuid}` (decision 6) for a blocker this instance
/// owns, `None` for a remote blocker, whose own activity id the federation
/// layer records from the `Block` activity it processed.
fn local_activity_id(base_url: &str, blocker: &User) -> Option<String> {
    blocker
        .local
        .get()
        .then(|| format!("{base_url}/activities/{}", new_id()))
}

/// The block of the ordered pair (`blocker`, `target`), whatever wrote it:
/// [`block`]'s read before and after its own write, on the pool or on the open
/// transaction it runs in.
async fn get<'e, E>(executor: E, blocker: Uuid, target: Uuid) -> AppResult<Option<Block>>
where
    E: Executor<'e, Database = Postgres>,
{
    let block = sqlx::query_as::<_, Block>(GET_SQL)
        .bind(blocker)
        .bind(target)
        .fetch_optional(executor)
        .await?;
    Ok(block)
}

/// Splits [`BlockUserRow`] back into the pair [`blocking_page`] answers: the
/// [`Block`] of the aliased columns, next to the [`User`] the join flattened.
fn into_entry(row: BlockUserRow) -> (Block, User) {
    let block = Block {
        id: row.block_id,
        blocker_id: row.blocker_id,
        blocked_id: row.blocked_id,
        ap_id: row.block_ap_id,
        inserted_at: row.block_inserted_at,
    };
    (block, row.user)
}

/// Turns a row a statement did not guarantee into a result, so a block a
/// concurrent write removed answers like any other absent record: 404.
fn required(block: Option<Block>) -> AppResult<Block> {
    block.map_or_else(|| Err(AppError::NotFound), Ok)
}

#[cfg(test)]
mod tests {
    use sqlx::{Error as SqlxError, PgPool};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            follow::{self, FollowState},
            user::{Flag, NewUser, get_by_id, insert, local_ap_id},
        },
        pagination::MAX_LIMIT,
        util::id::parse_id,
    };

    /// The `ActivityPub` path segment a local blocker's `ap_id` is built
    /// under, as [`block`] must write it (decision 6).
    const ACTIVITY_PREFIX: &str = "/activities/";

    /// Inserts a local, unlocked user named `nickname`.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Inserts a local user named `nickname` whose account is locked, so a
    /// follow of it waits for an answer.
    async fn locked_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                locked: Flag::new(true),
                ..NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
            },
        )
        .await
        .expect("the locked fixture inserts")
    }

    /// Inserts a mirror of `nickname` on `remote.example`: the account this
    /// instance does not own, whose `Block` activity id is not ours to mint.
    async fn remote_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id: format!("https://remote.example/users/{nickname}"),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// The row of user `id` as it stands now, for reading counters back.
    async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
        get_by_id(pool, id).await.unwrap().expect("the user exists")
    }

    /// Blocks `target` for `blocker` through the write under test and checks
    /// the row it wrote: the fixture every read below starts from.
    async fn seed_block(pool: &PgPool, blocker: &User, target: &User) {
        let row = block(pool, TEST_BASE_URL, blocker, target)
            .await
            .expect("the fixture block stores");
        assert_eq!(
            row.blocked_id, target.id,
            "the row is the pair that was asked for"
        );
    }

    /// Lifts `blocker`'s block on `target`, asserting one was there to lift.
    async fn seed_unblock(pool: &PgPool, blocker: &User, target: &User) {
        let removed = unblock(pool, blocker, target)
            .await
            .expect("the fixture unblock runs");
        assert!(removed.is_some(), "the pair's block was there to lift");
    }

    /// Records `follower`'s follow of the open local `target`, asserting the
    /// answer was granted on the spot: the fixture the severing checks.
    async fn seed_follow(pool: &PgPool, follower: &User, target: &User) {
        let row = follow::follow(pool, TEST_BASE_URL, follower, target)
            .await
            .expect("the fixture follow records");
        assert_eq!(
            row.state,
            FollowState::Accepted,
            "an unlocked local account answers at once"
        );
    }

    /// The stored block of the pair, which the test expects to exist.
    async fn stored_block(pool: &PgPool, blocker: &User, target: &User) -> Block {
        get(pool, blocker.id, target.id)
            .await
            .unwrap()
            .expect("the block exists")
    }

    /// Every row of the table, newest block first — the order a page walks,
    /// read straight from storage so same-millisecond `UUIDv7`s cannot make an
    /// expectation depend on the order the fixtures were written in.
    async fn stored_rows(pool: &PgPool) -> Vec<(Uuid, Uuid, Uuid)> {
        sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
            "SELECT id, blocker_id, blocked_id FROM blocks ORDER BY id DESC",
        )
        .fetch_all(pool)
        .await
        .expect("the rows read")
    }

    /// The number of rows in the table.
    async fn block_count(pool: &PgPool) -> i64 {
        let row: (i64,) = sqlx::query_as("SELECT count(*) FROM blocks")
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    /// Page params built from a query string, the way the extractor makes them.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture page parses")
    }

    /// The `{TEST_BASE_URL}/activities/{uuid}` id [`block`] minted for a local
    /// blocker, checked for its shape and reduced to the id part.
    fn activity_id(row: &Block) -> String {
        let prefix = format!("{TEST_BASE_URL}{ACTIVITY_PREFIX}");
        let ap_id = row
            .ap_id
            .clone()
            .expect("a local blocker's row carries a minted activity id");
        assert!(
            ap_id.starts_with(&prefix),
            "the activity id is this instance's, got {ap_id}"
        );
        let tail = ap_id
            .strip_prefix(&prefix)
            .expect("the prefix was just checked");
        assert!(
            parse_id(tail).is_some(),
            "the tail of the activity id is an id: {tail}"
        );
        tail.to_owned()
    }

    /// The SQLSTATE a database failure carries, or an empty string when the
    /// failure is not the database reporting a constraint.
    fn sqlstate_of(err: SqlxError) -> String {
        if let SqlxError::Database(db_err) = err {
            return db_err.code().unwrap_or_default().into_owned();
        }
        String::new()
    }

    /// The blocked ids of `entries`, in the order they came back.
    fn blocked_ids_of(entries: &[(Block, User)]) -> Vec<Uuid> {
        entries.iter().map(|(_, user)| user.id).collect()
    }

    /// The block row ids of `entries`, in the order they came back.
    fn row_ids_of(entries: &[(Block, User)]) -> Vec<Uuid> {
        entries.iter().map(|(row, _)| row.id).collect()
    }

    #[tokio::test]
    async fn a_block_is_written_and_read_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let row = block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the fixture block stores");

        assert!(row.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(row.blocker_id, alice.id, "who blocks");
        assert_eq!(row.blocked_id, bob.id, "who is blocked");
        assert!(row.inserted_at <= Utc::now(), "the database stamps it");
        let tail = activity_id(&row);
        assert!(parse_id(&tail).is_some(), "the minted id is a UUID: {tail}");
        assert_eq!(
            stored_block(&pool, &alice, &bob).await,
            row,
            "get() reads back the row it wrote"
        );

        assert!(
            is_blocking(&pool, alice.id, bob.id).await.unwrap(),
            "alice blocks bob"
        );
        assert!(
            !is_blocking(&pool, bob.id, alice.id).await.unwrap(),
            "blocking is directional"
        );
        assert!(
            blocked_between(&pool, alice.id, bob.id).await.unwrap(),
            "either direction answers the pair blocked"
        );
        assert!(
            blocked_between(&pool, bob.id, alice.id).await.unwrap(),
            "and the question does not care which way it is asked"
        );
        assert_eq!(
            blocked_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "bob is on alice's list"
        );
        assert!(
            blocked_ids(&pool, bob.id).await.unwrap().is_empty(),
            "and alice is on nobody's"
        );
        assert_eq!(
            blocker_ids(&pool, bob.id).await.unwrap(),
            vec![alice.id],
            "alice is on bob's blockers"
        );
        assert!(
            blocker_ids(&pool, alice.id).await.unwrap().is_empty(),
            "and nobody blocks alice"
        );

        let entries = blocking_page(&pool, alice.id, page("")).await.unwrap();
        assert_eq!(
            blocked_ids_of(&entries),
            vec![bob.id],
            "the page names the blocked account"
        );
        assert_eq!(
            row_ids_of(&entries),
            vec![row.id],
            "and carries the block row beside it"
        );
        assert_eq!(entries[0].1.id, bob.id, "the user is the blocked one");
        assert_eq!(entries[0].0, row, "and the row is the stored one");
        assert!(
            blocking_page(&pool, bob.id, page(""))
                .await
                .unwrap()
                .is_empty(),
            "bob's own page holds nothing"
        );
    }

    #[tokio::test]
    async fn a_remote_blocker_mints_no_activity_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = remote_user(&pool, "carol").await;
        let alice = local_user(&pool, "alice").await;

        let row = block(&pool, TEST_BASE_URL, &carol, &alice)
            .await
            .expect("the fixture block stores");
        assert!(
            row.ap_id.is_none(),
            "a remote blocker's id is the one its own instance minted"
        );
        assert!(
            stored_block(&pool, &carol, &alice).await.ap_id.is_none(),
            "and nothing was stored in its place"
        );
        assert!(
            is_blocking(&pool, carol.id, alice.id).await.unwrap(),
            "the fact itself is stored the same way"
        );
    }

    #[tokio::test]
    async fn blocking_twice_is_one_row_that_keeps_its_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let first = block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the fixture block stores");

        let again = block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the repeat block is not an error");
        assert_eq!(again, first, "the stored row is the answer");
        assert_eq!(block_count(&pool).await, 1, "and nothing was written twice");

        let erin = local_user(&pool, "erin").await;
        let other = block(&pool, TEST_BASE_URL, &alice, &erin)
            .await
            .expect("the fixture block stores");
        assert_ne!(
            other.ap_id, first.ap_id,
            "a new block mints a new activity id"
        );
    }

    /// The severing [`block`] owes the pair: both directions' accepted follows
    /// are gone and their counters stepped down, while another account's
    /// follow of the blocker and the blocker's own unrelated follow stand.
    #[tokio::test]
    async fn a_block_severs_both_follows_and_fixes_the_counts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        seed_follow(&pool, &alice, &bob).await;
        seed_follow(&pool, &bob, &alice).await;
        seed_follow(&pool, &carol, &bob).await;
        seed_follow(&pool, &alice, &carol).await;
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            2,
            "the fixtures count two followers"
        );

        seed_block(&pool, &alice, &bob).await;

        assert!(
            follow::get(&pool, alice.id, bob.id)
                .await
                .unwrap()
                .is_none(),
            "alice's follow of bob is gone"
        );
        assert!(
            follow::get(&pool, bob.id, alice.id)
                .await
                .unwrap()
                .is_none(),
            "and bob's follow of alice"
        );
        assert_eq!(
            (
                fresh_user(&pool, bob.id).await.followers_count,
                fresh_user(&pool, alice.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count,
                fresh_user(&pool, bob.id).await.following_count
            ),
            (1, 0, 1, 0),
            "carol still follows bob and alice still follows carol: only the \
             severed relationships stepped their counts down"
        );
        assert_eq!(
            follow::following_ids(&pool, carol.id).await.unwrap(),
            vec![bob.id],
            "carol's follow stands"
        );
    }

    /// Both directions' *pending* requests go with the block too, and a
    /// request never counted, so no counter moves for it.
    #[tokio::test]
    async fn a_block_withdraws_pending_requests_both_ways() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = locked_user(&pool, "alice").await;
        let carol = locked_user(&pool, "carol").await;
        let from_alice = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("alice asks carol");
        let from_carol = follow::follow(&pool, TEST_BASE_URL, &carol, &alice)
            .await
            .expect("and carol asks alice");
        assert_eq!(
            (from_alice.state, from_carol.state),
            (FollowState::Pending, FollowState::Pending),
            "two locked accounts answer their own requests"
        );

        seed_block(&pool, &alice, &carol).await;

        assert!(
            follow::get(&pool, alice.id, carol.id)
                .await
                .unwrap()
                .is_none(),
            "alice's request is withdrawn"
        );
        assert!(
            follow::get(&pool, carol.id, alice.id)
                .await
                .unwrap()
                .is_none(),
            "and carol's"
        );
        assert_eq!(
            (
                fresh_user(&pool, carol.id).await.followers_count,
                fresh_user(&pool, alice.id).await.followers_count
            ),
            (0, 0),
            "a request never counted, so nothing steps down"
        );
    }

    /// A mirrored `rejected` row is nobody's active relationship: the block
    /// leaves it in place, exactly as [`follow::unfollow`] does.
    #[tokio::test]
    async fn a_block_leaves_a_rejected_row_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dave = local_user(&pool, "dave").await;
        let carol = local_user(&pool, "carol").await;
        let rejected = sqlx::query_as::<_, follow::Follow>(
            "INSERT INTO follows (id, follower_id, followee_id, state)
             VALUES ($1, $2, $3, 'rejected')
             RETURNING *",
        )
        .bind(new_id())
        .bind(dave.id)
        .bind(carol.id)
        .fetch_one(&pool)
        .await
        .unwrap();

        seed_block(&pool, &dave, &carol).await;

        assert_eq!(
            follow::get(&pool, dave.id, carol.id).await.unwrap(),
            Some(rejected),
            "the rejected row is still there, unchanged"
        );
    }

    #[tokio::test]
    async fn unblocking_reports_the_row_it_removed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;

        assert!(
            unblock(&pool, &alice, &bob).await.unwrap().is_none(),
            "a pair that was never blocked has nothing to lift"
        );

        let row = block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the fixture block stores");
        let removed = unblock(&pool, &alice, &bob).await.unwrap();
        assert_eq!(
            removed.as_ref(),
            Some(&row),
            "the removed row comes back as it stood"
        );
        assert!(
            !is_blocking(&pool, alice.id, bob.id).await.unwrap(),
            "and the fact is gone"
        );
        assert!(
            !blocked_between(&pool, alice.id, bob.id).await.unwrap(),
            "in either direction"
        );
        assert!(
            unblock(&pool, &alice, &bob).await.unwrap().is_none(),
            "lifting it twice finds nothing the second time"
        );
        assert_eq!(block_count(&pool).await, 0, "and writes nothing");

        // Unblocking does not rewind the pair: it only lifts the guard, so a
        // follow that the block had severed is free to be granted again.
        seed_block(&pool, &alice, &carol).await;
        seed_unblock(&pool, &alice, &carol).await;
        let renewed = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("a lifted block lets the follow through again");
        assert_eq!(
            renewed.state,
            FollowState::Accepted,
            "and it is an ordinary follow"
        );
    }

    #[tokio::test]
    async fn blocking_yourself_is_refused() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;

        let err = block(&pool, TEST_BASE_URL, &alice, &alice)
            .await
            .expect_err("nobody blocks themselves");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "a self-block is the 422 the endpoint renders: {err}"
        );
        assert_eq!(err.to_string(), "Can not block yourself", "its wording");
        assert_eq!(block_count(&pool).await, 0, "and it stored nothing");
        assert!(
            !blocked_between(&pool, alice.id, alice.id).await.unwrap(),
            "the pair question agrees"
        );
    }

    #[tokio::test]
    async fn blocking_an_unknown_account_fails() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        // The endpoint hands the model rows it just resolved; this is the row
        // that vanished in between. The foreign key is the model's only
        // guard, so a pair that names no live account is a database error,
        // never a silently stored orphan.
        let gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the fixture target deletes");
        assert_eq!(gone.rows_affected(), 1, "one account was removed");

        let err = block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect_err("the foreign key refuses");
        assert!(
            matches!(err, AppError::Database(_)),
            "a constraint failure is the database variant: {err}"
        );
        assert_eq!(block_count(&pool).await, 0, "nothing was stored");
    }

    /// The pair key and the unique activity id: the facts the model builds
    /// on, checked against the table that stores them.
    #[tokio::test]
    async fn the_table_constraints_are_the_ones_the_model_relies_on() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let row = block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the fixture block stores");

        let duplicate =
            sqlx::query("INSERT INTO blocks (id, blocker_id, blocked_id) VALUES ($1, $2, $3)")
                .bind(new_id())
                .bind(alice.id)
                .bind(bob.id)
                .execute(&pool)
                .await
                .expect_err("one row per pair");
        assert_eq!(
            sqlstate_of(duplicate),
            "23505",
            "the pair is unique, which is what block()'s idempotency builds on"
        );

        let taken = sqlx::query(
            "INSERT INTO blocks (id, blocker_id, blocked_id, ap_id) VALUES ($1, $2, $3, $4)",
        )
        .bind(new_id())
        .bind(bob.id)
        .bind(alice.id)
        .bind(row.ap_id.as_deref())
        .execute(&pool)
        .await
        .expect_err("an activity id names one block");
        assert_eq!(
            sqlstate_of(taken),
            "23505",
            "the `ap_id` is unique, as decision 6 requires"
        );
    }

    /// Deleting either account takes the block through the foreign key's
    /// cascade, from both ends of the pair.
    #[tokio::test]
    async fn deleting_either_side_takes_the_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        seed_block(&pool, &alice, &bob).await;
        seed_block(&pool, &carol, &alice).await;

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the blocked account deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");
        assert_eq!(
            blocked_ids(&pool, alice.id).await.unwrap(),
            Vec::<Uuid>::new(),
            "alice's block of the deleted account went with it"
        );

        let gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the blocking account deletes");
        assert_eq!(gone.rows_affected(), 1, "and so did the other blocker");
        assert_eq!(
            blocker_ids(&pool, alice.id).await.unwrap(),
            Vec::<Uuid>::new(),
            "carol's block of the deleted account cascaded too"
        );
    }

    #[tokio::test]
    async fn blocks_are_each_account_s_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        seed_block(&pool, &alice, &carol).await;
        seed_block(&pool, &bob, &carol).await;

        assert!(
            blocked_ids(&pool, carol.id).await.unwrap().is_empty(),
            "carol holds nothing of the blocks made against her"
        );
        assert_eq!(
            blocker_ids(&pool, carol.id).await.unwrap(),
            stored_rows(&pool)
                .await
                .into_iter()
                .map(|(_, blocker, _)| blocker)
                .rev()
                .collect::<Vec<Uuid>>(),
            "both blockers are listed, oldest block first"
        );

        seed_unblock(&pool, &alice, &carol).await;
        assert!(
            blocked_ids(&pool, alice.id).await.unwrap().is_empty(),
            "alice's withdrawal left her own set empty"
        );
        assert_eq!(
            blocked_ids(&pool, bob.id).await.unwrap(),
            vec![carol.id],
            "and bob's alone"
        );
    }

    #[tokio::test]
    async fn a_page_of_blocks_newest_first_walks_by_row_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let erin = local_user(&pool, "erin").await;
        for nickname in ["bob", "carol", "dave"] {
            let blocked = local_user(&pool, nickname).await;
            seed_block(&pool, &alice, &blocked).await;
        }
        let stored = stored_rows(&pool).await;
        assert_eq!(stored.len(), 3, "three fixture blocks");

        let first = blocking_page(&pool, alice.id, page("limit=2"))
            .await
            .expect("the first page reads");
        assert_eq!(
            row_ids_of(&first),
            vec![stored[0].0, stored[1].0],
            "the two newest rows"
        );
        assert_eq!(
            blocked_ids_of(&first),
            vec![stored[0].2, stored[1].2],
            "and their blocked accounts, newest block first"
        );

        let older = blocking_page(&pool, alice.id, page(&format!("max_id={}", stored[1].0)))
            .await
            .expect("the continuation reads");
        assert_eq!(
            row_ids_of(&older),
            vec![stored[2].0],
            "max_id continues where the page ended"
        );

        let newer = blocking_page(&pool, alice.id, page(&format!("min_id={}", stored[2].0)))
            .await
            .expect("the forward page reads");
        assert_eq!(
            row_ids_of(&newer),
            vec![stored[0].0, stored[1].0],
            "a min_id page is answered newest-first too"
        );

        let tail = blocking_page(&pool, alice.id, page(&format!("max_id={}", stored[2].0)))
            .await
            .expect("walking off the end is no error");
        assert!(tail.is_empty(), "and answers nothing");

        let other = blocking_page(&pool, erin.id, page(""))
            .await
            .expect("a blocker with nothing blocked pages");
        assert!(other.is_empty(), "and holds nothing of alice's blocks");
    }

    /// The federation mirror's pair of writes: [`set_ap_id`] lands the remote
    /// activity id on the row [`block`] left without one, and [`get_by_ap_id`]
    /// reads that row back by the id — the path an `Undo` of a bare activity
    /// id resolves through.
    #[tokio::test]
    async fn an_activity_id_lands_on_the_row_and_reads_it_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = remote_user(&pool, "carol").await;
        let alice = local_user(&pool, "alice").await;
        let row = block(&pool, TEST_BASE_URL, &carol, &alice)
            .await
            .expect("the fixture block stores");
        assert!(row.ap_id.is_none(), "the remote blocker's row starts bare");

        let activity = "https://remote.example/activities/block-77";
        set_ap_id(&pool, row.id, activity).await.unwrap();
        let mirrored = stored_block(&pool, &carol, &alice).await;
        assert_eq!(
            mirrored.ap_id.as_deref(),
            Some(activity),
            "the delivered activity id is the row's now"
        );
        assert_eq!(mirrored.id, row.id, "and the row itself did not move");
        let found = get_by_ap_id(&pool, activity).await.unwrap();
        assert_eq!(
            found
                .as_ref()
                .map(|found| (found.id, found.ap_id.as_deref())),
            Some((row.id, Some(activity))),
            "the id reads the block back, now carrying that id"
        );

        set_ap_id(&pool, row.id, activity)
            .await
            .expect("restating the same id is no error");
        assert_eq!(
            stored_block(&pool, &carol, &alice).await.ap_id.as_deref(),
            Some(activity),
            "and writes the same value"
        );
        assert!(
            get_by_ap_id(&pool, "https://remote.example/activities/never")
                .await
                .unwrap()
                .is_none(),
            "an id no block carries resolves to nothing"
        );
        set_ap_id(&pool, new_id(), "https://remote.example/activities/orphan")
            .await
            .expect("an unknown block id changes nothing and is no error");
    }

    /// One `Block` activity ids one block: the unique `ap_id` refuses a
    /// second row under the same id.
    #[tokio::test]
    async fn one_activity_ids_one_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = remote_user(&pool, "carol").await;
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let row = block(&pool, TEST_BASE_URL, &carol, &alice)
            .await
            .expect("the fixture block stores");
        let activity = "https://remote.example/activities/block-78";
        set_ap_id(&pool, row.id, activity).await.unwrap();

        let second = block(&pool, TEST_BASE_URL, &carol, &bob)
            .await
            .expect("the second pair blocks");
        let taken = set_ap_id(&pool, second.id, activity)
            .await
            .expect_err("the id is spent on one row");
        assert!(
            matches!(taken, AppError::Database(_)),
            "the unique index is the guard: {taken}"
        );
        assert_eq!(
            stored_block(&pool, &carol, &bob).await.ap_id,
            None,
            "and the refused write stored nothing"
        );
    }
}
