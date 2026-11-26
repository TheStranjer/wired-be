//! Fetching stored statuses by key: by id ([`get_by_id`] and its
//! tombstone-keeping twin), by `ActivityPub` id ([`get_by_uri`]), in batches
//! ([`get_many`]), the ingest context lookup
//! ([`conversation_id_for_context`]) and the derived URLs a fresh row
//! writes ([`object_uri`], [`notice_url`]).

use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::object_ap_id};

use super::{NewStatus, Status};

/// [`get_by_id`]: the live row behind a primary key, tombstones excluded.
const LIVE_BY_ID_SQL: &str = "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL";

/// [`get_many`]: the live rows of `ids`, oldest id first (decision 4).
const MANY_SQL: &str =
    "SELECT * FROM statuses WHERE id = ANY($1) AND deleted_at IS NULL ORDER BY id";

/// [`get_by_id_including_deleted`]: one row, tombstone or not.
const ANY_BY_ID_SQL: &str = "SELECT * FROM statuses WHERE id = $1";

/// [`get_by_uri`]: one row by its `ActivityPub` id, tombstone or not —
/// federation dedupes on this, and a deleted post's `uri` stays reserved.
const BY_URI_SQL: &str = "SELECT * FROM statuses WHERE uri = $1";

/// [`conversation_id_for_context`]: the conversation of the oldest live
/// status stored under an `ap_context` — the `ORDER BY id LIMIT 1` pick the
/// ingest reply arm inherits when the parent is not here yet. Tombstones
/// keep their `ap_context` but answer no conversation, and the walk is
/// served by `statuses_ap_context_idx` (see
/// `migrations/1795800000_statuses_ap_context.sql`).
const CONTEXT_CONVERSATION_SQL: &str = "SELECT conversation_id FROM statuses
       WHERE ap_context = $1 AND deleted_at IS NULL
    ORDER BY id
        LIMIT 1";

/// [`remote_thread_context`]: the oldest context stored under a
/// conversation whose root row (the status with the conversation's id) is
/// a mirrored one.
const REMOTE_THREAD_CONTEXT_SQL: &str = "SELECT s.ap_context FROM statuses s
       WHERE s.conversation_id = $1 AND s.ap_context IS NOT NULL
         AND EXISTS (SELECT 1 FROM statuses r WHERE r.id = $1 AND NOT r.local)
    ORDER BY s.id
       LIMIT 1";

/// [`conversation_root_is_remote`]: the `local` flag of the conversation's
/// root row.
const ROOT_LOCAL_SQL: &str = "SELECT local FROM statuses WHERE id = $1";

/// Fetches the live status whose primary key is `id`, `None` when no row
/// matches or when the row is a tombstone.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<Status>> {
    let status = sqlx::query_as::<_, Status>(LIVE_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(status)
}

/// Fetches the status whose primary key is `id`, tombstone included: the
/// moderation and federation view of a deleted post.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id_including_deleted(pool: &PgPool, id: Uuid) -> AppResult<Option<Status>> {
    let status = sqlx::query_as::<_, Status>(ANY_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(status)
}

/// Fetches the status whose `ActivityPub` object id is `uri`, tombstone
/// included: `uri` is unique and stays reserved when a status is deleted, so
/// this is how federation recognises a post it has already seen.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_uri(pool: &PgPool, uri: &str) -> AppResult<Option<Status>> {
    let status = sqlx::query_as::<_, Status>(BY_URI_SQL)
        .bind(uri)
        .fetch_optional(pool)
        .await?;
    Ok(status)
}

/// The conversation of the oldest live status stored under `ap_context`.
///
/// This is the ingest reply arm's conversation inheritance when the parent
/// itself is not here: `ORDER BY id LIMIT 1` makes the pick deterministic —
/// the oldest status carrying the context, the one that minted the
/// `conversation_id` every later status of the thread joined.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn conversation_id_for_context(
    pool: &PgPool,
    ap_context: &str,
) -> AppResult<Option<Uuid>> {
    let conversation = sqlx::query_as::<_, (Uuid,)>(CONTEXT_CONVERSATION_SQL)
        .bind(ap_context)
        .fetch_optional(pool)
        .await?;
    Ok(conversation.map(|row| row.0))
}

/// The `ActivityPub` context a remote-rooted conversation advertises.
///
/// The oldest stored `ap_context` among the conversation's statuses, `None`
/// when the thread is rooted here (this instance owns its context), when
/// the remote root is unknown, and when no remote status of the thread
/// carried a context. A local reply names this instead of a context of
/// its own.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn remote_thread_context(
    pool: &PgPool,
    conversation_id: Uuid,
) -> AppResult<Option<String>> {
    let context = sqlx::query_scalar::<_, String>(REMOTE_THREAD_CONTEXT_SQL)
        .bind(conversation_id)
        .fetch_optional(pool)
        .await?;
    Ok(context)
}

/// Whether the root of conversation `conversation_id` is a mirrored status.
///
/// `false` for a locally rooted conversation and for an id no status
/// carries; the context collection only answers for the former's owner.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn conversation_root_is_remote(pool: &PgPool, conversation_id: Uuid) -> AppResult<bool> {
    let local = sqlx::query_scalar::<_, bool>(ROOT_LOCAL_SQL)
        .bind(conversation_id)
        .fetch_optional(pool)
        .await?;
    Ok(local == Some(false))
}

/// Fetches the live statuses whose ids are in `ids`, in `id` order (`UUIDv7`
/// text sorts by creation, decision 4). Ids with no live row — absent or
/// deleted — are skipped; an empty `ids` yields an empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_many(pool: &PgPool, ids: &[Uuid]) -> AppResult<Vec<Status>> {
    let ids = ids.to_vec();
    let statuses = sqlx::query_as::<_, Status>(MANY_SQL)
        .bind(&ids)
        .fetch_all(pool)
        .await?;
    Ok(statuses)
}

/// The `ActivityPub` object id to store: this instance's
/// `{base_url}/objects/{id}` for a local status (decision 6), else the remote
/// id the caller supplied — `None` for a remote status with no id, which the
/// `NOT NULL` column then rejects.
pub(super) fn object_uri(base_url: &str, new_status: &NewStatus, id: Uuid) -> Option<String> {
    new_status
        .local
        .then(|| object_ap_id(base_url, id))
        .or_else(|| new_status.uri.clone())
}

/// The human-facing URL to store: this instance's `{base_url}/notice/{id}`
/// for a local status, else the remote's own `url`.
pub(super) fn notice_url(base_url: &str, new_status: &NewStatus, id: Uuid) -> Option<String> {
    new_status
        .local
        .then(|| format!("{base_url}/notice/{id}"))
        .or_else(|| new_status.url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::status::{insert, soft_delete, test_support::*};
    use crate::util::id::new_id;

    #[tokio::test]
    async fn get_by_uri_still_finds_a_tombstone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let mallory = remote_user(&pool, "mallory").await;
        let theirs = insert(
            &pool,
            BASE,
            &mirrored(&mallory, "https://remote.example/posts/7"),
        )
        .await
        .unwrap();
        let mine = post(&pool, &alice, "mine").await;

        assert_eq!(
            get_by_uri(&pool, &theirs.uri).await.unwrap(),
            Some(theirs.clone()),
            "federation finds a mirrored post by its remote id"
        );
        assert!(
            soft_delete(&pool, theirs.id).await.unwrap().is_some(),
            "a Delete turns it into a tombstone"
        );
        let found = get_by_uri(&pool, &theirs.uri)
            .await
            .unwrap()
            .expect("the uri stays reserved");
        assert_eq!(found.id, theirs.id, "the tombstone still answers by uri");
        assert!(found.deleted_at.is_some(), "and it is still marked deleted");
        assert_eq!(
            get_by_uri(&pool, &mine.uri).await.unwrap(),
            Some(mine.clone()),
            "a local status is found by its own object id too"
        );
        assert!(
            get_by_uri(&pool, "https://remote.example/posts/none")
                .await
                .unwrap()
                .is_none(),
            "an unseen uri is nothing"
        );

        let err = insert(&pool, BASE, &mirrored(&mallory, &theirs.uri))
            .await
            .expect_err("a deleted remote post cannot be imported again");
        assert_eq!(
            sqlstate_of(err),
            "23505",
            "the unique uri is what rejects the re-import"
        );
    }

    #[tokio::test]
    async fn get_many_reads_live_rows_in_id_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let one = post(&pool, &alice, "one").await;
        let two = post(&pool, &alice, "two").await;
        let mut wanted = vec![one.id, two.id];
        wanted.sort_unstable();

        assert_eq!(
            ids(&get_many(&pool, &[two.id, new_id(), one.id]).await.unwrap()),
            wanted,
            "the two live rows, oldest first, an unknown id skipped"
        );

        assert!(
            soft_delete(&pool, two.id).await.unwrap().is_some(),
            "one of them is deleted"
        );
        assert_eq!(
            ids(&get_many(&pool, &[one.id, two.id]).await.unwrap()),
            vec![one.id],
            "and it leaves the list"
        );
        assert!(
            get_many(&pool, &[]).await.unwrap().is_empty(),
            "no ids, no rows"
        );
    }
}
