//! The `status_mentions` table: which account a status @-mentions.
//!
//! Reference: Pleroma does not store this edge — it derives the Status
//! entity's `mentions` array from the activity's `to` addressing
//! (`lib/pleroma/web/mastodon_api/views/status_view.ex`, `build_mentions`)
//! and resolves `@nick` text into those recipients when formatting a post
//! (`lib/pleroma/formatter.ex`). Here each link is a row of its own, keyed
//! `(status_id, user_id)`, so the entity renders from storage and the
//! reverse lookup — the statuses mentioning one account — is an index seek
//! on `(user_id, status_id DESC)`.
//!
//! A row also carries its `position`, the order its account was linked in:
//! the `mentions` array keeps that order rather than any id or name order,
//! and later links continue it after the rows already stored. [`link`] is idempotent per pair — re-linking an account already
//! mentioned keeps its original place — and [`replace_for_status`] rewrites
//! the whole set of one status in one transaction, the shape an edit needs.
//!
//! Nothing here resolves text to users: [`crate::services::mentions`] turns
//! `@nick` references into accounts, and [`crate::models::status::insert`]
//! writes what that resolution found next to the status row, in one
//! transaction.

use std::collections::{HashMap, HashSet};

use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, models::user::User, pagination::PageParams};

/// [`link`]: one statement for a whole batch. Each account takes the next
/// free `position` of the status — the stored maximum plus its one-based
/// [`WITH ORDINALITY`] number — so repeat links continue the ordering
/// instead of starting a new 0-based block, and an account the status
/// already mentions is skipped by the key (`ON CONFLICT DO NOTHING`),
/// keeping its original place.
///
/// The double `$1` is deliberate: the same id both seeds the `next_pos`
/// subquery and lands in the inserted column; the `::uuid` cast is what
/// lets the parameter bind twice where Postgres cannot infer the type from
/// the subquery alone.
///
/// [`WITH ORDINALITY`]: https://www.postgresql.org/docs/current/functions-array.html
const LINK_SQL: &str = "INSERT INTO status_mentions (status_id, user_id, position)
     SELECT s.status_id, u.user_id, (s.next_pos + u.ord - 1)::int
       FROM (SELECT $1::uuid AS status_id,
                    COALESCE(MAX(m.position), -1) + 1 AS next_pos
               FROM status_mentions m WHERE m.status_id = $1) AS s,
            unnest($2::uuid[]) WITH ORDINALITY AS u(user_id, ord)
ON CONFLICT (status_id, user_id) DO NOTHING";

/// The statement behind [`replace_for_status`]: the old set goes first, so
/// the following [`LINK_SQL`] starts its positions at zero again.
const DELETE_FOR_STATUS_SQL: &str = "DELETE FROM status_mentions WHERE status_id = $1";

/// The leading text of [`for_statuses`]: every mentioned account of the
/// given statuses, each row led by the status it mentions, ordered status
/// first and then by the link's `position` — the array order the entity
/// renders.
///
/// The statement stops where its one placeholder belongs, because that is
/// where [`for_statuses`] binds and the plain (non-builder) query numbers
/// it `$1`.
const FOR_STATUSES_SQL: &str = "SELECT sm.status_id, u.* FROM status_mentions sm
      JOIN users u ON u.id = sm.user_id
     WHERE sm.status_id = ANY($1)
     ORDER BY sm.status_id, sm.position, sm.user_id";

/// The leading text of [`mentioned_status_ids`]: the statuses mentioning
/// one account, newest first off the `(user_id, status_id DESC)` index. It
/// ends on the placeholder [`mentioned_status_ids`] pushes, and the keyset
/// [`PageParams::apply_keyset`] then appends runs on `sm.status_id`.
const MENTIONED_SQL: &str = "SELECT sm.status_id FROM status_mentions sm WHERE sm.user_id = ";

/// The joined row of [`for_statuses`]: one mention link with the [`User`]
/// its `user_id` names, flattened.
#[derive(Debug, sqlx::FromRow)]
struct MentionedUserRow {
    /// The status this row links, from `status_mentions.status_id`.
    status_id: Uuid,
    /// The mentioned account, from the joined `users` row.
    #[sqlx(flatten)]
    user: User,
}

/// Links the accounts in `user_ids` to status `status_id`, in the order
/// given.
///
/// The first id becomes the status's first mention (`position` zero), and a
/// link of a status that already has mentions continues its numbering, so
/// reading [`for_statuses`] back always yields the order accounts were
/// first mentioned in. A pair already linked is skipped: re-mentioning an
/// account does not move it. An id repeated within one call is folded to
/// its first occurrence.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); an unknown status or
/// user arrives as the foreign-key violation the database reports, unless
/// the call links nothing at all (an empty `user_ids` touches no row and
/// checks no key).
pub async fn link(pool: &PgPool, status_id: Uuid, user_ids: &[Uuid]) -> AppResult<()> {
    let mut conn = pool.acquire().await?;
    link_conn(&mut conn, status_id, user_ids).await
}

/// [`link`] on an open connection: the statement runs on the caller's
/// transaction, which is how [`crate::models::status::insert`] writes a
/// status and its mentions together.
///
/// An empty batch short-circuits: no statement, no round trip, and no
/// foreign-key check either (nothing is written, so an unknown `status_id`
/// with nothing to link is no error).
pub(crate) async fn link_conn(
    conn: &mut PgConnection,
    status_id: Uuid,
    user_ids: &[Uuid],
) -> AppResult<()> {
    if user_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(LINK_SQL)
        .bind(status_id)
        .bind(dedupe(user_ids))
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// Replaces the whole mention set of status `status_id` with `user_ids`,
/// in the order given, in one transaction: every row the status has is
/// deleted and the new links take positions from zero again.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); as with [`link`], an
/// unknown account (or an unknown status with a non-empty `user_ids`) is
/// the foreign-key violation the database reports, and the transaction
/// rolls every part of the replacement back with it.
pub async fn replace_for_status(
    pool: &PgPool,
    status_id: Uuid,
    user_ids: &[Uuid],
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    replace_conn(&mut tx, status_id, user_ids).await?;
    tx.commit().await?;
    Ok(())
}

/// [`replace_for_status`] on an open connection: the whole replacement runs
/// on the caller's transaction, which is how
/// [`crate::models::status_edit::update_status`] rewrites a status and its
/// mentions together.
///
/// # Errors
///
/// Those of [`replace_for_status`].
pub(crate) async fn replace_conn(
    conn: &mut PgConnection,
    status_id: Uuid,
    user_ids: &[Uuid],
) -> AppResult<()> {
    sqlx::query(DELETE_FOR_STATUS_SQL)
        .bind(status_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    link_conn(conn, status_id, user_ids).await
}

/// The mentioned accounts of every status in `ids`, keyed by status id,
/// each entry in mention order (see [`link`]).
///
/// Statuses the caller asked about that have no mentions are absent from
/// the map, and an empty `ids` answers the empty map; this is the batch
/// read that renders a page of statuses without one query per row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn for_statuses(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, Vec<User>>> {
    let ids = ids.to_vec();
    let rows = sqlx::query_as::<_, MentionedUserRow>(FOR_STATUSES_SQL)
        .bind(&ids)
        .fetch_all(pool)
        .await?;
    let mut grouped: HashMap<Uuid, Vec<User>> = HashMap::new();
    for row in rows {
        grouped.entry(row.status_id).or_default().push(row.user);
    }
    Ok(grouped)
}

/// The ids of the statuses mentioning `user_id`, paged by `page` and
/// answered newest first (decision 4 keys the walk on the id itself, and
/// the `(user_id, status_id DESC)` index serves it).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mentioned_status_ids(
    pool: &PgPool,
    user_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<Uuid>> {
    let mut builder = QueryBuilder::<Postgres>::new(MENTIONED_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(user_id);
    page.apply_keyset(&mut builder, "sm.status_id");
    let rows = builder.build_query_as::<(Uuid,)>().fetch_all(pool).await?;
    Ok(page.finish(rows.into_iter().map(|(status_id,)| status_id).collect()))
}

/// `user_ids` with repeats folded to their first occurrence, in order:
/// what [`LINK_SQL`] inserts, so a doubled id never consumes a `position`
/// the batch does not store.
fn dedupe(user_ids: &[Uuid]) -> Vec<Uuid> {
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut unique: Vec<Uuid> = Vec::new();
    for &user_id in user_ids {
        if seen.insert(user_id) {
            unique.push(user_id);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use sqlx::{Error as SqlxError, PgPool};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::{
            status::{self, NewStatus},
            user::{NewUser, insert as insert_user, local_ap_id},
        },
        pagination::MAX_LIMIT,
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = TEST_BASE_URL;

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("fixture user inserts")
    }

    /// A local public status of `alice` mentioning nobody.
    async fn post(pool: &PgPool, author: &User) -> status::Status {
        status::insert(pool, BASE, &NewStatus::local(author, "hello"))
            .await
            .expect("fixture status inserts")
    }

    /// The users of `map` for `status_id`, in the order they came back,
    /// by id.
    fn mention_ids(map: &HashMap<Uuid, Vec<User>>, status_id: Uuid) -> Vec<Uuid> {
        map.get(&status_id)
            .map_or_default(|users| users.iter().map(|user| user.id).collect())
    }

    /// The table's rows as `(status_id, user_id, position)`, read straight
    /// from storage so assertions do not depend on fixture return values.
    async fn stored_rows(pool: &PgPool) -> Vec<(Uuid, Uuid, i32)> {
        sqlx::query_as::<_, (Uuid, Uuid, i32)>(
            "SELECT status_id, user_id, position FROM status_mentions
              ORDER BY status_id, position",
        )
        .fetch_all(pool)
        .await
        .expect("the stored rows read")
    }

    /// `page`'s query text parsed, with the default cap.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture page parses")
    }

    /// The SQLSTATE a database failure carries.
    fn sqlstate_of(err: AppError) -> String {
        if let AppError::Database(SqlxError::Database(db_err)) = err {
            return db_err.code().unwrap_or_default().into_owned();
        }
        String::new()
    }

    #[tokio::test]
    async fn link_keeps_the_given_order_and_zero_based_positions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let dave = user(&pool, "dave").await;
        let status = post(&pool, &alice).await;

        link(&pool, status.id, &[dave.id, bob.id, carol.id])
            .await
            .expect("the first link stores");

        let map = for_statuses(&pool, &[status.id])
            .await
            .expect("the batch read works");
        assert_eq!(
            mention_ids(&map, status.id),
            vec![dave.id, bob.id, carol.id],
            "the accounts come back in the order they were linked"
        );
        assert_eq!(
            stored_rows(&pool).await,
            vec![
                (status.id, dave.id, 0),
                (status.id, bob.id, 1),
                (status.id, carol.id, 2),
            ],
            "positions count from zero, in link order"
        );
    }

    #[tokio::test]
    async fn relinking_continues_the_order_without_moving_the_old_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let dave = user(&pool, "dave").await;
        let status = post(&pool, &alice).await;
        link(&pool, status.id, &[bob.id, carol.id]).await.unwrap();

        link(&pool, status.id, &[carol.id, dave.id])
            .await
            .expect("a repeat mention of an old account and one new account links");

        let map = for_statuses(&pool, &[status.id]).await.unwrap();
        assert_eq!(
            mention_ids(&map, status.id),
            vec![bob.id, carol.id, dave.id],
            "the old pair keeps its place; the new account joins at the end"
        );
        assert_eq!(
            stored_rows(&pool).await,
            vec![
                (status.id, bob.id, 0),
                (status.id, carol.id, 1),
                (status.id, dave.id, 3),
            ],
            "the skipped repeat still advanced the counter: dave lands at 3, \
             not at 2"
        );
    }

    #[tokio::test]
    async fn a_repeated_id_within_one_link_is_one_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let status = post(&pool, &alice).await;

        link(&pool, status.id, &[bob.id, carol.id, bob.id])
            .await
            .expect("the folded batch links");
        assert_eq!(
            stored_rows(&pool).await,
            vec![(status.id, bob.id, 0), (status.id, carol.id, 1)],
            "the first occurrence wins and positions stay tight"
        );
    }

    #[tokio::test]
    async fn linking_nothing_is_no_work() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status = post(&pool, &alice).await;
        link(&pool, status.id, &[]).await.expect("empty links fine");
        assert!(
            stored_rows(&pool).await.is_empty(),
            "an empty batch writes no row"
        );
    }

    #[tokio::test]
    async fn an_unknown_side_of_a_link_is_the_foreign_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status = post(&pool, &alice).await;
        let nobody = Uuid::now_v7();
        let err = link(&pool, status.id, std::slice::from_ref(&nobody))
            .await
            .expect_err("the foreign key refuses an unknown account");
        assert_eq!(
            sqlstate_of(err),
            "23503",
            "a missing user is an FK violation"
        );
        assert!(stored_rows(&pool).await.is_empty(), "nothing was stored");
    }

    #[tokio::test]
    async fn replace_rewrites_the_set_and_restarts_the_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let dave = user(&pool, "dave").await;
        let status = post(&pool, &alice).await;
        link(&pool, status.id, &[bob.id, carol.id]).await.unwrap();

        replace_for_status(&pool, status.id, &[dave.id, carol.id])
            .await
            .expect("the replacement runs");
        assert_eq!(
            stored_rows(&pool).await,
            vec![(status.id, dave.id, 0), (status.id, carol.id, 1)],
            "the old set is gone, the new one is ordered from zero"
        );

        replace_for_status(&pool, status.id, &[])
            .await
            .expect("replacing with nothing runs");
        assert!(
            stored_rows(&pool).await.is_empty(),
            "and an empty replacement clears the status"
        );
    }

    #[tokio::test]
    async fn a_bad_replace_keeps_the_old_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let status = post(&pool, &alice).await;
        link(&pool, status.id, &[bob.id]).await.unwrap();

        let err = replace_for_status(&pool, status.id, &[Uuid::now_v7()])
            .await
            .expect_err("an unknown account fails the transaction");
        assert!(
            matches!(err, AppError::Database(_)),
            "the failure is the database's: {err}"
        );
        assert_eq!(
            stored_rows(&pool).await,
            vec![(status.id, bob.id, 0)],
            "the rolled-back transaction left the original link in place"
        );
    }

    #[tokio::test]
    async fn deleting_a_status_or_an_account_takes_the_links() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let first = post(&pool, &alice).await;
        let second = post(&pool, &alice).await;
        link(&pool, first.id, &[bob.id, carol.id]).await.unwrap();
        link(&pool, second.id, &[bob.id]).await.unwrap();

        let gone = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(first.id)
            .execute(&pool)
            .await
            .expect("the fixture status hard-deletes");
        assert_eq!(gone.rows_affected(), 1, "one status went");
        assert_eq!(
            stored_rows(&pool).await,
            vec![(second.id, bob.id, 0)],
            "its mention rows cascaded with it"
        );

        let dropped = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the fixture account deletes");
        assert_eq!(dropped.rows_affected(), 1, "and so did the other end");
        assert!(
            stored_rows(&pool).await.is_empty(),
            "mentions of the deleted account cascaded too"
        );
    }

    #[tokio::test]
    async fn for_statuses_groups_by_status_and_skips_the_quiet_ones() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let one = post(&pool, &alice).await;
        let two = post(&pool, &alice).await;
        let quiet = post(&pool, &alice).await;
        link(&pool, one.id, &[bob.id]).await.unwrap();
        link(&pool, two.id, &[carol.id, bob.id]).await.unwrap();

        let map = for_statuses(&pool, &[one.id, two.id, quiet.id, Uuid::now_v7()])
            .await
            .expect("the batch read works");
        assert_eq!(
            mention_ids(&map, one.id),
            vec![bob.id],
            "the first status's mention"
        );
        assert_eq!(
            mention_ids(&map, two.id),
            vec![carol.id, bob.id],
            "the second status keeps its own order"
        );
        assert!(
            !map.contains_key(&quiet.id),
            "a status with no mentions is absent, not an empty entry"
        );
        assert_eq!(map.len(), 2, "and an unknown id is absent as well");
        assert!(
            for_statuses(&pool, &[]).await.unwrap().is_empty(),
            "asking about nothing answers nothing"
        );
    }

    #[tokio::test]
    async fn mentioned_status_ids_pages_newest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        for _ in 0..3 {
            let status = post(&pool, &alice).await;
            link(&pool, status.id, &[bob.id]).await.unwrap();
        }
        // Same-millisecond UUIDv7s do not strictly order by insert time, so
        // the expectation is the stored order itself, read from storage.
        let stored: Vec<Uuid> = sqlx::query_scalar::<_, Uuid>(
            "SELECT status_id FROM status_mentions WHERE user_id = $1 \
             ORDER BY status_id DESC",
        )
        .bind(bob.id)
        .fetch_all(&pool)
        .await
        .expect("the stored mentions read");
        assert_eq!(stored.len(), 3, "the fixture mentions three statuses");

        let first = mentioned_status_ids(&pool, bob.id, page("limit=2"))
            .await
            .expect("the first page reads");
        assert_eq!(first, vec![stored[0], stored[1]], "the two newest");

        let older = mentioned_status_ids(&pool, bob.id, page(&format!("max_id={}", stored[1])))
            .await
            .expect("the continuation reads");
        assert_eq!(older, vec![stored[2]], "max_id walks to the older rows");

        let newer = mentioned_status_ids(&pool, bob.id, page(&format!("min_id={}", stored[2])))
            .await
            .expect("the forward page reads");
        assert_eq!(
            newer,
            vec![stored[0], stored[1]],
            "and is answered newest-first too"
        );

        let alice_mentions = mentioned_status_ids(&pool, alice.id, page(""))
            .await
            .expect("an unmentioned account pages fine");
        assert!(alice_mentions.is_empty(), "and answers nothing");
    }
}
