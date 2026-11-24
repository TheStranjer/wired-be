//! The `endorsements` table: an account pinning another account's profile
//! to its own (Mastodon's "featured profiles").
//!
//! Reference: Pleroma's endorsement edges in `Pleroma.UserRelationship`
//! (`lib/pleroma/user_relationships.ex`), the write behind the Mastodon
//! `AccountController`'s `endorse`/`unendorse` actions. Pleroma stores the
//! fact as a row of the shared `user_relationships` table with a
//! `relationship` discriminator; this table is that one edge type laid out
//! on its own, keyed by the ticket's `(user_id, target_id)` pair.
//!
//! [`endorse`] and [`unendorse`] are both idempotent on that key: the
//! first insert of a pair writes the row and a repeat writes nothing
//! (`ON CONFLICT DO NOTHING`), the first delete of a pair removes the row
//! and a repeat removes nothing. What an endorsement *requires* — the
//! endorser's accepted follow of the target — is the endpoint's guard, not
//! the table's; the model stores what it is handed.
//!
//! [`endorsed_ids`] answers the whole set at once (the batch
//! [`Relationship`](crate::entities::relationship::Relationship) loader
//! reads its `endorsed` flag from it) and [`page_for`] gives the endpoint
//! the same accounts a keyset page at a time, each entry led by the
//! endorsement row's id — the cursor the listings walk by (decision 4).
//! Deleting either account takes the endorsement through the foreign
//! key's cascade; unfollowing the target also removes the endorsement
//! (see [`crate::models::follow::unfollow`]), and so does a block of the
//! pair, in both directions ([`remove_between`]).

use sqlx::{Executor, PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, models::user::User, pagination::PageParams, util::id::new_id};

/// [`endorse`]: the pair the caller names with `inserted_at` left to the
/// column default. `ON CONFLICT (user_id, target_id) DO NOTHING` is the
/// idempotence: re-endoring an endorsed pair writes nothing and keeps the
/// original row (and its cursor id).
const ENDORSE_SQL: &str = "INSERT INTO endorsements (id, user_id, target_id)
     VALUES ($1, $2, $3)
ON CONFLICT (user_id, target_id) DO NOTHING";

/// [`unendorse`]: one endorser's one pair; a never-endorsed or already
/// withdrawn pair deletes nothing, so withdrawing is idempotent too.
const UNENDORSE_SQL: &str = "DELETE FROM endorsements WHERE user_id = $1 AND target_id = $2";

/// [`remove_between`]: the endorsements of a pair in both directions, what a
/// block must not leave on either public profile.
const REMOVE_BETWEEN_SQL: &str = "DELETE FROM endorsements
     WHERE (user_id = $1 AND target_id = $2) OR (user_id = $2 AND target_id = $1)";

/// [`endorsed_ids`]: every endorsed account of one endorser, oldest
/// endorsement first (the `UUIDv7` text sorts in creation order,
/// decision 4). The order is a stability promise, not semantics: callers
/// ask set membership or page separately.
const ALL_IDS_SQL: &str = "SELECT target_id FROM endorsements WHERE user_id = $1 ORDER BY id";

/// The leading text of [`page_for`]: the endorsed accounts of one
/// endorser, each entry led by the `endorsement` row's own id — the id
/// the page keys on. The statement stops where its one placeholder
/// belongs, because that is where [`page_for`] pushes its bind and the
/// builder is what numbers it `$1`; the keyset
/// [`PageParams::apply_keyset`] then appends runs on the row id `e.id`.
const PAGE_SQL: &str = "SELECT e.id AS endorsement_id, u.* FROM endorsements e
     JOIN users u ON u.id = e.target_id
      WHERE e.user_id = ";

/// One entry of a paged endorsement list: the endorsed [`User`] plus the
/// id of the `endorsements` row that put them on the page.
///
/// The endorsement id — not the user's — is what [`page_for`] keysets on
/// (decision 4), so the listings' `max_id`/`min_id` cursors and `Link`
/// header carry these ids, as the relationship pages carry follow ids.
#[derive(Debug, sqlx::FromRow)]
pub struct EndorsementPageEntry {
    /// Primary key of the `endorsements` row behind this entry.
    pub endorsement_id: Uuid,
    /// The endorsed user.
    #[sqlx(flatten)]
    pub user: User,
}

/// Records that `user_id` endorses `target_id`, idempotently.
///
/// A pair that is already endorsed keeps its row — and its id, so the
/// listing's cursor does not move — and a repeat call writes nothing. The
/// endpoint guards that the endorser follows the target before calling
/// this; the model stores what it is handed.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn endorse(pool: &PgPool, user_id: Uuid, target_id: Uuid) -> AppResult<()> {
    sqlx::query(ENDORSE_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(target_id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// Removes `user_id`'s endorsement of `target_id`, idempotently.
///
/// A pair that was never endorsed (or already withdrawn) deletes nothing
/// and is not an error. This is also what an unfollow runs behind the
/// scenes (see [`crate::models::follow::unfollow`]).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unendorse<'e, E>(executor: E, user_id: Uuid, target_id: Uuid) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(UNENDORSE_SQL)
        .bind(user_id)
        .bind(target_id)
        .execute(executor)
        .await
        .map(drop)?;
    Ok(())
}

/// Removes the endorsements between `first` and `second` in both directions,
/// returning how many went away.
///
/// Runs on the caller's connection so a block severs the pair's endorsements in
/// the same transaction as its row. A pair with no endorsements matches nothing
/// and answers `0`; other accounts' endorsements are never touched.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn remove_between(conn: &mut PgConnection, first: Uuid, second: Uuid) -> AppResult<u64> {
    let deleted = sqlx::query(REMOVE_BETWEEN_SQL)
        .bind(first)
        .bind(second)
        .execute(&mut *conn)
        .await?;
    Ok(deleted.rows_affected())
}

/// The ids of the accounts `user_id` endorses, oldest endorsement first.
///
/// The whole set in one statement: [`crate::models::relationship`] turns
/// it into the `endorsed` flag of a batch of targets, and the endpoints
/// that only ask whether a pair is endorsed can check membership here too.
/// An endorser with nothing endorsed answers the empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn endorsed_ids(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(ALL_IDS_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(target_id,)| target_id).collect())
}

/// The accounts `user_id` endorses, newest endorsement first and `page`'s
/// `limit` at a time, each entry carrying the endorsement row's id next
/// to the [`User`] (see [`EndorsementPageEntry`]).
///
/// [`PageParams::finish`] flips a `min_id` fetch back to newest-first, so
/// every page is answered newest endorsement first like the relationship
/// listings are.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn page_for(
    pool: &PgPool,
    user_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<EndorsementPageEntry>> {
    let mut builder = QueryBuilder::<Postgres>::new(PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(user_id);
    page.apply_keyset(&mut builder, "e.id");
    let rows = builder
        .build_query_as::<EndorsementPageEntry>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(rows))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{error::AppError, pagination::MAX_LIMIT, test_fixtures::local_user as user};

    /// The ids of the rows of the table, descending — the expected cursor
    /// order for the pagination assertions, read straight from storage so
    /// same-millisecond `UUIDv7`s cannot make it depend on insert timing.
    async fn stored_ids(pool: &PgPool) -> Vec<Uuid> {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM endorsements ORDER BY id DESC")
            .fetch_all(pool)
            .await
            .expect("the ids read")
    }

    /// The `(id, target)` rows of the table, newest endorsement first.
    async fn stored_rows(pool: &PgPool) -> Vec<(Uuid, Uuid)> {
        sqlx::query_as::<_, (Uuid, Uuid)>("SELECT id, target_id FROM endorsements ORDER BY id DESC")
            .fetch_all(pool)
            .await
            .expect("the rows read")
    }

    /// `page`'s query text parsed, with the default cap.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture page parses")
    }

    /// The endorsed ids of `entries`, in the order they came back.
    fn entry_ids(entries: &[EndorsementPageEntry]) -> Vec<Uuid> {
        entries.iter().map(|entry| entry.user.id).collect()
    }

    /// The endorsement row ids of `entries`, in the order they came back.
    fn endorsement_ids(entries: &[EndorsementPageEntry]) -> Vec<Uuid> {
        entries.iter().map(|entry| entry.endorsement_id).collect()
    }

    #[tokio::test]
    async fn an_endorsement_is_written_and_read_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        endorse(&pool, alice.id, bob.id)
            .await
            .expect("the first endorsement stores");

        assert_eq!(
            endorsed_ids(&pool, alice.id).await.expect("the read works"),
            vec![bob.id],
            "the target comes back"
        );
        assert_eq!(stored_ids(&pool).await.len(), 1, "and it is one row");
    }

    #[tokio::test]
    async fn endorsing_twice_is_one_row_and_keeps_its_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        endorse(&pool, alice.id, bob.id).await.unwrap();
        let first = stored_ids(&pool).await;

        endorse(&pool, alice.id, bob.id)
            .await
            .expect("the repeat endorsement is not an error");
        assert_eq!(
            stored_ids(&pool).await,
            first,
            "the first row kept its id: nothing was rewritten"
        );
        assert_eq!(
            endorsed_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "still one entry"
        );
    }

    #[tokio::test]
    async fn withdrawing_removes_the_endorsement_and_repeating_is_fine() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        endorse(&pool, alice.id, bob.id).await.unwrap();

        unendorse(&pool, alice.id, bob.id)
            .await
            .expect("the withdrawal deletes");
        assert!(
            endorsed_ids(&pool, alice.id).await.unwrap().is_empty(),
            "the pair is no longer endorsed"
        );

        unendorse(&pool, alice.id, bob.id)
            .await
            .expect("withdrawing again is not an error");
        assert_eq!(stored_ids(&pool).await.len(), 0, "and deletes nothing");
    }

    #[tokio::test]
    async fn remove_between_takes_both_directions_and_only_the_pair() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        endorse(&pool, alice.id, bob.id).await.unwrap();
        endorse(&pool, bob.id, alice.id).await.unwrap();
        endorse(&pool, alice.id, carol.id).await.unwrap();
        endorse(&pool, carol.id, bob.id).await.unwrap();

        let mut conn = pool.acquire().await.expect("a connection");
        assert_eq!(
            remove_between(&mut conn, alice.id, bob.id)
                .await
                .expect("the pair's endorsements go"),
            2,
            "one each way"
        );
        assert_eq!(
            endorsed_ids(&pool, alice.id).await.unwrap(),
            vec![carol.id],
            "alice keeps her other endorsement"
        );
        assert!(
            endorsed_ids(&pool, bob.id).await.unwrap().is_empty(),
            "bob's is gone"
        );
        assert_eq!(
            endorsed_ids(&pool, carol.id).await.unwrap(),
            vec![bob.id],
            "a third account's endorsement is untouched"
        );
        assert_eq!(
            remove_between(&mut conn, bob.id, alice.id)
                .await
                .expect("repeating is not an error"),
            0,
            "and removes nothing the second time"
        );
    }

    #[tokio::test]
    async fn endorsements_are_each_account_s_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        endorse(&pool, alice.id, bob.id).await.unwrap();

        assert!(
            endorsed_ids(&pool, carol.id).await.unwrap().is_empty(),
            "carol has nothing of alice's endorsements"
        );
        unendorse(&pool, carol.id, bob.id)
            .await
            .expect("carol's withdrawal runs");
        assert_eq!(
            endorsed_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "and left alice's endorsement alone"
        );
        endorse(&pool, carol.id, bob.id)
            .await
            .expect("the same target endorsed by another account is a second row");
        assert_eq!(stored_ids(&pool).await.len(), 2, "one row per endorser");
    }

    #[tokio::test]
    async fn endorsed_ids_keeps_oldest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        for nickname in ["bob", "carol", "dave"] {
            let target = user(&pool, nickname).await;
            endorse(&pool, alice.id, target.id).await.unwrap();
        }
        // Same-millisecond UUIDv7s do not strictly order by insert time, so
        // the expectation is the stored id order itself, read from storage.
        let expected: Vec<Uuid> =
            sqlx::query_scalar::<_, Uuid>("SELECT target_id FROM endorsements ORDER BY id")
                .fetch_all(&pool)
                .await
                .expect("the stored order reads");
        assert_eq!(expected.len(), 3, "three fixture endorsements");

        assert_eq!(
            endorsed_ids(&pool, alice.id).await.unwrap(),
            expected,
            "creation order, oldest endorsement first"
        );
    }

    #[tokio::test]
    async fn a_page_newest_first_walks_by_row_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let dave = user(&pool, "dave").await;
        for target in [&bob, &carol, &dave] {
            endorse(&pool, alice.id, target.id).await.unwrap();
        }
        let stored = stored_rows(&pool).await;
        assert_eq!(stored.len(), 3, "three fixture endorsements");

        let first = page_for(&pool, alice.id, page("limit=2"))
            .await
            .expect("the first page reads");
        assert_eq!(
            endorsement_ids(&first),
            vec![stored[0].0, stored[1].0],
            "the two newest rows"
        );
        assert_eq!(
            entry_ids(&first),
            vec![stored[0].1, stored[1].1],
            "and their targets, newest endorsement first"
        );

        let older = page_for(&pool, alice.id, page(&format!("max_id={}", stored[1].0)))
            .await
            .expect("the continuation reads");
        assert_eq!(
            endorsement_ids(&older),
            vec![stored[2].0],
            "max_id continues where the page ended"
        );

        let newer = page_for(&pool, alice.id, page(&format!("min_id={}", stored[2].0)))
            .await
            .expect("the forward page reads");
        assert_eq!(
            endorsement_ids(&newer),
            vec![stored[0].0, stored[1].0],
            "a min_id page is answered newest-first too"
        );

        let tail = page_for(&pool, alice.id, page(&format!("max_id={}", stored[2].0)))
            .await
            .expect("walking off the end is no error");
        assert!(tail.is_empty(), "and answers nothing");
    }

    #[tokio::test]
    async fn an_endorser_with_nothing_endorsed_pages_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        assert!(
            page_for(&pool, alice.id, page(""))
                .await
                .expect("an empty listing is no error")
                .is_empty(),
            "nothing endorsed, nothing to page"
        );
        assert!(
            endorsed_ids(&pool, alice.id).await.unwrap().is_empty(),
            "and the flat read agrees"
        );
    }

    #[tokio::test]
    async fn deleting_either_side_takes_the_endorsement() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let dave = user(&pool, "dave").await;
        endorse(&pool, alice.id, bob.id).await.unwrap();
        endorse(&pool, carol.id, dave.id).await.unwrap();

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the fixture target deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");
        assert_eq!(
            endorsed_ids(&pool, alice.id).await.unwrap(),
            Vec::<Uuid>::new(),
            "the endorsement of the deleted account went with it"
        );

        let gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(carol.id)
            .execute(&pool)
            .await
            .expect("the fixture endorser deletes");
        assert_eq!(gone.rows_affected(), 1, "and so did the other endorser");
        assert_eq!(
            stored_ids(&pool).await.len(),
            0,
            "its endorsement cascaded too"
        );
    }

    #[tokio::test]
    async fn an_endorsement_for_no_stored_account_fails() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // The foreign key is the model's only guard: the endpoint hands
        // ids that name live users, and a fabricated one is a database
        // error, never a silently stored orphan.
        let alice = user(&pool, "alice").await;
        let nobody = Uuid::now_v7();
        let err = endorse(&pool, alice.id, nobody)
            .await
            .expect_err("the foreign key refuses");
        assert!(
            matches!(err, AppError::Database(_)),
            "a constraint failure is the database variant: {err}"
        );
        assert!(stored_ids(&pool).await.is_empty(), "nothing was stored");
    }
}
