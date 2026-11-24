//! The `status_hashtags` edge: which status carries which tag.
//!
//! A link is written by [`link_names_conn`] (on the caller's connection, in
//! the status's own transaction) and is idempotent: the pair the status
//! already has is skipped, so linking twice changes nothing and the edge
//! carries no position — the entity renders [`for_statuses`]' name order. An
//! edit rewrites the whole set through [`replace_names_conn`], and
//! [`for_statuses`] is the batched read that renders a page of statuses
//! without one query per row.

use std::collections::HashMap;

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::AppResult;

use super::{Hashtag, StatusHashtagRow, tags::upsert_many_conn};

/// The statement behind `link_conn`: one pair per id, and a pair the status
/// already has is skipped, so linking twice changes nothing.
const LINK_SQL: &str = "INSERT INTO status_hashtags (status_id, hashtag_id)
     SELECT $1::uuid, t.hashtag_id FROM unnest($2::uuid[]) AS t(hashtag_id)
ON CONFLICT (status_id, hashtag_id) DO NOTHING";

/// The statement behind [`replace_names_conn`]: the old set goes first, and
/// the freshly upserted names are linked after it, on the same connection.
const DELETE_FOR_STATUS_SQL: &str = "DELETE FROM status_hashtags WHERE status_id = $1";

/// The leading text of [`for_statuses`]: every tag of the given statuses,
/// each row led by the status it tags, ordered status first and then by
/// tag name — the array order the entity renders.
const FOR_STATUSES_SQL: &str = "SELECT sh.status_id, h.* FROM status_hashtags sh
      JOIN hashtags h ON h.id = sh.hashtag_id
     WHERE sh.status_id = ANY($1)
     ORDER BY sh.status_id, h.name";

/// Links the tags in `hashtag_ids` to status `status_id`, on `conn`: the
/// statement runs on the caller's transaction.
///
/// Pairs the status already has are skipped, so linking twice changes
/// nothing, and an id repeated within one call is folded by the key. Order
/// is not kept: the entity renders [`for_statuses`]' name order.
///
/// An empty batch short-circuits: no statement, no round trip, and no
/// foreign-key check either (nothing is written, so an unknown `status_id`
/// with nothing to link is no error).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); an unknown status or tag
/// arrives as the foreign-key violation the database reports.
pub(super) async fn link_conn(
    conn: &mut PgConnection,
    status_id: Uuid,
    hashtag_ids: &[Uuid],
) -> AppResult<()> {
    if hashtag_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(LINK_SQL)
        .bind(status_id)
        .bind(hashtag_ids)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// Upserts `names` and links every tag they name to status `status_id`, on
/// `conn`: the pair of writes [`crate::models::status::insert`] runs inside
/// the status's transaction.
///
/// # Errors
///
/// Those of `upsert_many_conn` and `link_conn`: any database failure
/// (connection, encoding) arrives as [`AppError::Database`](crate::error::AppError),
/// and an unknown `status_id` beside at least one usable name is the
/// foreign-key violation the database reports.
pub async fn link_names_conn(
    conn: &mut PgConnection,
    status_id: Uuid,
    names: &[String],
) -> AppResult<()> {
    let hashtags = upsert_many_conn(conn, names).await?;
    let ids: Vec<Uuid> = hashtags.iter().map(|hashtag| hashtag.id).collect();
    link_conn(conn, status_id, &ids).await
}

/// Replaces the whole tag set of status `status_id` with `names`, on `conn`.
///
/// The replacement runs on the caller's transaction, which is how
/// [`crate::models::status_edit::update_status`] rewrites a status and its
/// tags together. This is the shape an edit of a post needs: every link the status has is
/// deleted and the upserted names of `names` are linked in their place.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); an unknown `status_id`
/// with at least one usable name is the foreign-key violation the database
/// reports, which rolls the replacement back when `conn` is a transaction.
pub async fn replace_names_conn(
    conn: &mut PgConnection,
    status_id: Uuid,
    names: &[String],
) -> AppResult<()> {
    sqlx::query(DELETE_FOR_STATUS_SQL)
        .bind(status_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    link_names_conn(conn, status_id, names).await
}

/// The tags of every status in `ids`, keyed by status id, each entry
/// ordered by name.
///
/// Statuses the caller asked about that carry no tags are absent from the
/// map, and an empty `ids` answers the empty map; this is the batch read
/// that renders a page of statuses without one query per row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn for_statuses(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, Vec<Hashtag>>> {
    let ids = ids.to_vec();
    let rows = sqlx::query_as::<_, StatusHashtagRow>(FOR_STATUSES_SQL)
        .bind(&ids)
        .fetch_all(pool)
        .await?;
    let mut grouped: HashMap<Uuid, Vec<Hashtag>> = HashMap::new();
    for row in rows {
        grouped.entry(row.status_id).or_default().push(row.hashtag);
    }
    Ok(grouped)
}

#[cfg(test)]
mod tests {
    use sqlx::Error as SqlxError;

    use super::*;
    use crate::{
        error::AppError,
        models::hashtag::{find_or_create, test_support::*},
    };

    /// The stored links of status `status_id` as tag names, ordered by name.
    async fn stored_links(pool: &PgPool, status_id: Uuid) -> Vec<String> {
        sqlx::query_as::<_, (String,)>(
            "SELECT h.name FROM status_hashtags sh
               JOIN hashtags h ON h.id = sh.hashtag_id
              WHERE sh.status_id = $1
              ORDER BY h.name",
        )
        .bind(status_id)
        .fetch_all(pool)
        .await
        .expect("the stored links read")
        .into_iter()
        .map(|row| row.0)
        .collect()
    }

    /// The SQLSTATE a database failure carries.
    fn sqlstate_of(err: AppError) -> String {
        if let AppError::Database(SqlxError::Database(db_err)) = err {
            return db_err.code().unwrap_or_default().into_owned();
        }
        String::new()
    }

    #[tokio::test]
    async fn link_conn_is_idempotent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let status = post(&pool, "alice", "hello").await;
        let foo = find_or_create(&pool, "#foo").await.unwrap().expect("foo");
        let bar = find_or_create(&pool, "bar").await.unwrap().expect("bar");
        let mut conn = pool.acquire().await.expect("a pooled connection");

        link_conn(&mut conn, status.id, &[foo.id, bar.id, foo.id])
            .await
            .expect("the first link stores");
        link_conn(&mut conn, status.id, &[bar.id])
            .await
            .expect("a repeat link changes nothing");
        assert_eq!(
            stored_links(&pool, status.id).await,
            vec!["bar", "foo"],
            "one row per pair, the doubled id folded"
        );

        link_conn(&mut conn, status.id, &[])
            .await
            .expect("linking nothing is no work");
    }

    #[tokio::test]
    async fn link_names_conn_upserts_and_links_idempotently() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let status = post(&pool, "alice", "hello").await;
        let mut conn = pool.acquire().await.expect("a pooled connection");

        link_names_conn(&mut conn, status.id, &tags(&["#Foo", "bar", "FOO", " "]))
            .await
            .expect("the names are upserted and linked");
        link_names_conn(&mut conn, status.id, &tags(&["foo"]))
            .await
            .expect("a repeat changes nothing");
        assert_eq!(
            stored_links(&pool, status.id).await,
            vec!["bar", "foo"],
            "one link per usable name, spellings folded, the blank dropped"
        );
        assert_eq!(stored_tags(&pool).await.len(), 2, "two tag rows");

        link_names_conn(&mut conn, status.id, &[])
            .await
            .expect("linking no names is no work");
    }

    #[tokio::test]
    async fn an_unknown_side_of_a_link_is_the_foreign_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let status = post(&pool, "alice", "hello").await;
        let mut conn = pool.acquire().await.expect("a pooled connection");
        let err = link_conn(&mut conn, status.id, &[Uuid::now_v7()])
            .await
            .expect_err("the foreign key refuses an unknown tag");
        assert_eq!(sqlstate_of(err), "23503", "a missing tag is an FK");
        assert!(
            stored_links(&pool, status.id).await.is_empty(),
            "nothing stored"
        );

        let status_err = link_conn(&mut conn, Uuid::now_v7(), &[status.id])
            .await
            .expect_err("and so is an unknown status");
        assert_eq!(
            sqlstate_of(status_err),
            "23503",
            "the other side is checked too"
        );
    }

    #[tokio::test]
    async fn replace_names_conn_rewrites_the_whole_set() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let status = post(&pool, "alice", "#old #shared").await;
        let mut conn = pool.acquire().await.expect("a pooled connection");
        assert_eq!(
            stored_links(&pool, status.id).await,
            vec!["old", "shared"],
            "the fixture came tagged"
        );

        replace_names_conn(&mut conn, status.id, &tags(&["new", "SHARED"]))
            .await
            .expect("the replacement runs");
        assert_eq!(
            stored_links(&pool, status.id).await,
            vec!["new", "shared"],
            "the old set is gone, the shared tag kept its row"
        );
        assert_eq!(
            stored_tags(&pool).await.len(),
            3,
            "the abandoned `old` tag row itself stays"
        );

        replace_names_conn(&mut conn, status.id, &[])
            .await
            .expect("replacing with nothing runs");
        assert!(
            stored_links(&pool, status.id).await.is_empty(),
            "and an empty replacement clears the status"
        );
    }

    #[tokio::test]
    async fn for_statuses_groups_by_status_in_name_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let tagged = post(&pool, "alice", "#beta #alpha #beta").await;
        let shared = post(&pool, "bob", "#alpha").await;
        let quiet = post(&pool, "carol", "no tags").await;

        let map = for_statuses(&pool, &[tagged.id, shared.id, quiet.id, Uuid::now_v7()])
            .await
            .expect("the batch read works");
        assert_eq!(
            names(map.get(&tagged.id).expect("the tagged status")),
            vec!["alpha", "beta"],
            "each entry is ordered by name, repeats folded"
        );
        assert_eq!(
            names(map.get(&shared.id).expect("the shared status")),
            vec!["alpha"],
            "and a shared tag is the same row under both statuses"
        );
        assert_eq!(
            map.get(&shared.id).expect("shared")[0].id,
            map.get(&tagged.id).expect("tagged")[0].id,
            "one `alpha` for both"
        );
        assert!(
            !map.contains_key(&quiet.id),
            "a status without tags is absent, not an empty entry"
        );
        assert_eq!(map.len(), 2, "and an unknown id is absent as well");
        assert!(
            for_statuses(&pool, &[]).await.unwrap().is_empty(),
            "asking about nothing answers nothing"
        );
    }

    #[tokio::test]
    async fn deleting_a_status_cascades_its_links() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let one = post(&pool, "alice", "#shared #mine").await;
        let two = post(&pool, "bob", "#shared").await;

        let gone = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(one.id)
            .execute(&pool)
            .await
            .expect("the fixture status hard-deletes");
        assert_eq!(gone.rows_affected(), 1, "one status went");
        assert!(
            stored_links(&pool, one.id).await.is_empty(),
            "its links cascaded with it"
        );
        assert_eq!(
            stored_links(&pool, two.id).await,
            vec!["shared"],
            "the other status keeps its link"
        );
        assert_eq!(
            stored_tags(&pool).await.len(),
            2,
            "and the tag rows stay for reuse"
        );
    }
}
