//! The deferred quote link: a remote status that quotes an object this
//! instance did not have keeps the object's `uri` in `quote_uri`, and the
//! link — `quote_of_id` plus the original's `quotes_count` — is made later,
//! either by the `federator.quote_backfill` job ([`link_backfilled_quote`])
//! or when the quoted status itself arrives ([`link_quotes_of`]).

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::AppResult;

use super::{Counter, Status, counters::step_counter};

/// [`pending_quote_uri`]: the quote reference of a live, still unlinked row.
const PENDING_QUOTE_URI_SQL: &str = "SELECT quote_uri FROM statuses
    WHERE id = $1 AND deleted_at IS NULL AND quote_of_id IS NULL";

/// [`link_backfilled_quote`]: the live-and-unlinked guard makes the write
/// idempotent, as the reply link's is — a second pass matches no row.
const LINK_QUOTE_SQL: &str = "UPDATE statuses
      SET quote_of_id = $2, updated_at = now()
    WHERE id = $1 AND deleted_at IS NULL AND quote_of_id IS NULL
  RETURNING id";

/// [`link_quotes_of`]: every live unlinked row waiting for `$2`, except the
/// original itself (a status that names its own `uri` quotes nothing).
const LINK_WAITING_SQL: &str = "UPDATE statuses
      SET quote_of_id = $1, updated_at = now()
    WHERE quote_uri = $2 AND quote_of_id IS NULL AND deleted_at IS NULL AND id <> $1
  RETURNING id";

/// The quoted object's `uri` that the live status `status_id` waits for.
///
/// `None` when the row is gone, tombstoned, already linked or quotes nothing.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn pending_quote_uri(pool: &PgPool, status_id: Uuid) -> AppResult<Option<String>> {
    let uri = sqlx::query_scalar::<_, Option<String>>(PENDING_QUOTE_URI_SQL)
        .bind(status_id)
        .fetch_optional(pool)
        .await?;
    Ok(uri.flatten())
}

/// Links `quoting_id` to the stored original `quoted`, once.
///
/// The original's `quotes_count` steps with the link. `true` says this call
/// wrote it, `false` that the row was already linked, tombstoned or gone.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); the rollback leaves the
/// quote unlinked and the counter untouched.
pub async fn link_backfilled_quote(
    pool: &PgPool,
    quoting_id: Uuid,
    quoted: &Status,
) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    let linked = sqlx::query_scalar::<_, Uuid>(LINK_QUOTE_SQL)
        .bind(quoting_id)
        .bind(quoted.id)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
    if linked {
        step_counter(&mut tx, quoted.id, Counter::Quotes, 1).await?;
    }
    tx.commit().await?;
    Ok(linked)
}

/// Links the waiting quotes of the just-stored original `quoted_id`.
///
/// Every live, unlinked status whose `quote_uri` is `uri` is linked, on the
/// caller's connection, and the original's `quotes_count` steps by the number
/// linked. Answers that number.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn link_quotes_of(
    conn: &mut PgConnection,
    quoted_id: Uuid,
    uri: &str,
) -> AppResult<usize> {
    Ok(link_waiting_quotes(conn, quoted_id, uri).await?.len())
}

/// [`link_quotes_of`], answering the ids of the quoting statuses it linked
/// instead of their number.
///
/// The ids let the caller tell the original's author about each quote once
/// the transaction has committed.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn link_waiting_quotes(
    conn: &mut PgConnection,
    quoted_id: Uuid,
    uri: &str,
) -> AppResult<Vec<Uuid>> {
    let linked = sqlx::query_scalar::<_, Uuid>(LINK_WAITING_SQL)
        .bind(quoted_id)
        .bind(uri)
        .fetch_all(&mut *conn)
        .await?;
    if !linked.is_empty() {
        let delta = i32::try_from(linked.len()).unwrap_or(i32::MAX);
        step_counter(conn, quoted_id, Counter::Quotes, delta).await?;
    }
    Ok(linked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::status::{
        NewStatus, insert, soft_delete,
        test_support::{BASE, counter, mirrored, remote_user, stored},
    };

    /// A remote quote of `quoted_uri` by a fresh mirror, stored unlinked.
    async fn waiting_quote(pool: &PgPool, name: &str, quoted_uri: &str) -> Status {
        let author = remote_user(pool, name).await;
        insert(
            pool,
            BASE,
            &NewStatus {
                quote_uri: Some(quoted_uri.to_owned()),
                ..mirrored(&author, &format!("https://remote.example/objects/{name}-q"))
            },
        )
        .await
        .expect("the unlinked quote inserts")
    }

    /// The stored original `uri`, by a fresh mirror.
    async fn original(pool: &PgPool, name: &str, uri: &str) -> Status {
        let author = remote_user(pool, name).await;
        insert(pool, BASE, &mirrored(&author, uri))
            .await
            .expect("the original inserts")
    }

    #[tokio::test]
    async fn an_unlinked_quote_keeps_its_uri_until_it_is_linked() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let uri = "https://remote.example/objects/orig";
        let quoting = waiting_quote(&pool, "carol", uri).await;
        assert_eq!(quoting.quote_of_id, None, "nothing links at insert");
        assert_eq!(
            pending_quote_uri(&pool, quoting.id)
                .await
                .unwrap()
                .as_deref(),
            Some(uri),
            "the reference is kept"
        );

        let quoted = original(&pool, "dave", uri).await;
        assert!(
            link_backfilled_quote(&pool, quoting.id, &quoted)
                .await
                .unwrap(),
            "the first link writes"
        );
        assert!(
            !link_backfilled_quote(&pool, quoting.id, &quoted)
                .await
                .unwrap(),
            "a second pass writes nothing"
        );
        assert_eq!(stored(&pool, quoting.id).await.quote_of_id, Some(quoted.id));
        assert_eq!(
            counter(&pool, quoted.id, Counter::Quotes).await,
            1,
            "the count stepped once"
        );
        assert_eq!(
            pending_quote_uri(&pool, quoting.id).await.unwrap(),
            None,
            "a linked quote waits for nothing"
        );
    }

    #[tokio::test]
    async fn a_tombstoned_quote_is_not_linked() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let uri = "https://remote.example/objects/orig2";
        let quoting = waiting_quote(&pool, "carol", uri).await;
        let quoted = original(&pool, "dave", uri).await;
        let _gone = soft_delete(&pool, quoting.id).await.unwrap();
        assert!(
            !link_backfilled_quote(&pool, quoting.id, &quoted)
                .await
                .unwrap(),
            "a tombstone is not re-linked"
        );
        assert_eq!(pending_quote_uri(&pool, quoting.id).await.unwrap(), None);
        assert_eq!(counter(&pool, quoted.id, Counter::Quotes).await, 0);
    }

    #[tokio::test]
    async fn the_arrival_of_the_original_links_every_waiting_quote() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let uri = "https://remote.example/objects/orig3";
        let first = waiting_quote(&pool, "carol", uri).await;
        let second = waiting_quote(&pool, "erin", uri).await;
        let other = waiting_quote(&pool, "frank", "https://remote.example/objects/else").await;
        let quoted = original(&pool, "dave", uri).await;

        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(link_quotes_of(&mut conn, quoted.id, uri).await.unwrap(), 2);
        assert_eq!(
            link_quotes_of(&mut conn, quoted.id, uri).await.unwrap(),
            0,
            "a repeat links nothing"
        );
        assert_eq!(stored(&pool, first.id).await.quote_of_id, Some(quoted.id));
        assert_eq!(stored(&pool, second.id).await.quote_of_id, Some(quoted.id));
        assert_eq!(
            stored(&pool, other.id).await.quote_of_id,
            None,
            "a quote of another uri stays unlinked"
        );
        assert_eq!(counter(&pool, quoted.id, Counter::Quotes).await, 2);
    }

    #[tokio::test]
    async fn a_status_naming_its_own_uri_quotes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let uri = "https://remote.example/objects/self";
        let author = remote_user(&pool, "carol").await;
        let row = insert(
            &pool,
            BASE,
            &NewStatus {
                quote_uri: Some(uri.to_owned()),
                ..mirrored(&author, uri)
            },
        )
        .await
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(link_quotes_of(&mut conn, row.id, uri).await.unwrap(), 0);
        assert_eq!(stored(&pool, row.id).await.quote_of_id, None);
    }
}
