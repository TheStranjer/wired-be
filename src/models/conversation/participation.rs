//! The `conversation_participations` rows: the reads and writes of one
//! account's own side of a thread after [`participate`] has opened it —
//! one row by id, the batched thread-to-row map the status renderer reads,
//! the local roster of one status's thread, the read flags, the
//! withdrawal, and the badge count.
//!
//! [`participate`]: super::participate

use std::collections::HashMap;

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

use super::Participation;

/// [`participation_ids`]: which of a batch of threads one account
/// participates in, and with which row. `= ANY($2)` is the batch form the
/// page renderer needs — one read per rendered page, whatever its size
/// (decision 3's fixed query set).
const PARTICIPATION_IDS_SQL: &str = "SELECT conversation_id, id FROM conversation_participations \
     WHERE user_id = $1 AND conversation_id = ANY($2)";

/// [`participations_for_status`]: every local side of the thread one status
/// was recorded into. The join runs through `statuses.direct_conversation_id`,
/// so a status of any other visibility (threaded by nothing) and a thread
/// whose only members are remote (a participation is a local account's row)
/// both answer no rows; the roster is ordered by its owning account.
const PARTICIPATIONS_FOR_STATUS_SQL: &str = "SELECT p.* FROM conversation_participations p \
     JOIN statuses s ON s.direct_conversation_id = p.conversation_id \
     WHERE s.id = $1 ORDER BY p.user_id";

/// [`get_participation`]: the one row of `user_id` whose id is `id`. Both
/// halves of the key are required — a participation somebody else owns
/// simply is not there, which is what makes the id a capability.
const OWNED_SQL: &str = "SELECT * FROM conversation_participations WHERE id = $1 AND user_id = $2";

/// [`mark_read`]: the owner's own row, flagged read either way — repeating
/// the write returns the same row and changes nothing.
const MARK_READ_SQL: &str = "UPDATE conversation_participations
      SET read = true
    WHERE id = $1 AND user_id = $2
  RETURNING *";

/// [`mark_all_read`]: only the unread rows are written, and `RETURNING *`
/// answers exactly the set that flipped (Pleroma's `mark_all_as_read`
/// reports the same list).
const MARK_ALL_READ_SQL: &str = "UPDATE conversation_participations
      SET read = true
    WHERE user_id = $1 AND NOT read
  RETURNING *";

/// [`delete_participation`]: one account's one thread; a row never held (or
/// already withdrawn) deletes nothing and is not an error.
const DELETE_PARTICIPATION_SQL: &str =
    "DELETE FROM conversation_participations WHERE id = $1 AND user_id = $2";

/// [`unread_count`]: how many of the account's threads still name an unread
/// status (Pleroma's `Participation.unread_count`).
const UNREAD_COUNT_SQL: &str = "SELECT count(*) FROM conversation_participations \
     WHERE user_id = $1 AND NOT read";

/// Every local side of the direct thread `status_id` was recorded into, each
/// as its own [`Participation`] row, ordered by its owning account.
///
/// The row's `user_id` is the reader to tell and its `id` is the participation
/// their socket renders — the id [`crate::streaming::hub::RenderSpec::Conversation`]
/// names, which means nothing to any other side of the thread. The thread is
/// found through the status's stored
/// [`direct_conversation_id`](crate::models::status::Status::direct_conversation_id),
/// so the answer is exactly the [`participate`] roster the create hook just
/// wrote: a status of any other visibility (threaded by nothing) and a thread
/// whose only members are remote (a participation is a local account's row)
/// both answer the empty `Vec`. [`participate_in_conversation`] is its only
/// caller, publishing one conversation update per row it returns.
///
/// [`participate_in_conversation`]: crate::services::create_status
/// [`participate`]: super::participate
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn participations_for_status(
    pool: &PgPool,
    status_id: Uuid,
) -> AppResult<Vec<Participation>> {
    let rows = sqlx::query_as::<_, Participation>(PARTICIPATIONS_FOR_STATUS_SQL)
        .bind(status_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Which of `conversation_ids` `user_id` participates in, keyed by thread
/// with the participation id: the Status renderer's one read per page.
///
/// Pleroma's `direct_conversation_id` is the same fact, read per row
/// there. A thread the account was never bumped into is simply absent,
/// and an empty ask skips the database.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn participation_ids(
    pool: &PgPool,
    user_id: Uuid,
    conversation_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Uuid>> {
    if conversation_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (Uuid, Uuid)>(PARTICIPATION_IDS_SQL)
        .bind(user_id)
        .bind(conversation_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().collect())
}

/// `user_id`'s participation `id`, `None` when no such row is theirs.
///
/// Somebody else's id and no id at all are the same answer, which is what
/// lets `POST /api/v1/statuses` refuse a foreign or unknown
/// `in_reply_to_conversation_id` as a plain 404.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn get_participation(
    pool: &PgPool,
    user_id: Uuid,
    id: Uuid,
) -> AppResult<Option<Participation>> {
    let row = sqlx::query_as::<_, Participation>(OWNED_SQL)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Flags `user_id`'s participation `id` read and answers the row, `None`
/// when the id is not theirs. A repeat mark is the same row and a no-op.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn mark_read(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<Option<Participation>> {
    let row = sqlx::query_as::<_, Participation>(MARK_READ_SQL)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Flags every unread participation of `user_id` read at once and answers
/// exactly the rows that flipped.
///
/// Pleroma's `Participation.mark_all_as_read/1`. An account with nothing
/// unread answers the empty `Vec` and writes nothing.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn mark_all_read(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Participation>> {
    let rows = sqlx::query_as::<_, Participation>(MARK_ALL_READ_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Withdraws `user_id`'s participation `id`, answering whether a row went:
/// the recipient list cascades with it, and the account drops out of that
/// listing of the thread.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn delete_participation(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<bool> {
    let gone = sqlx::query(DELETE_PARTICIPATION_SQL)
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(gone > 0)
}

/// How many of `user_id`'s participations still name an unread status —
/// the badge count (Pleroma's `Participation.unread_count`).
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn unread_count(pool: &PgPool, user_id: Uuid) -> AppResult<i64> {
    let count = sqlx::query_scalar::<_, i64>(UNREAD_COUNT_SQL)
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        models::status::{self, NewStatus},
    };

    use crate::models::conversation::{
        recipients,
        test_support::{
            bump, direct, linked_thread, local, participation, remote, slot, stored_recipients,
        },
    };

    #[tokio::test]
    async fn participation_ids_answers_membership_of_the_ask_only() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let post = direct(&pool, &alice, &[&bob], "@bob psst").await;
        bump(&pool, &post).await;
        let thread = linked_thread(&pool, &post).await;
        let bob_row = participation(&pool, bob.id, thread)
            .await
            .expect("bob participates");

        let stranger = Uuid::now_v7();
        let asked = [thread, stranger];
        let found = participation_ids(&pool, bob.id, &asked)
            .await
            .expect("the batch reads");
        assert_eq!(
            found.get(&thread),
            Some(&bob_row.id),
            "his thread maps to his participation"
        );
        assert!(
            !found.contains_key(&stranger),
            "the unknown thread is absent"
        );
        assert!(
            participation_ids(&pool, alice.id, &asked)
                .await
                .expect("alice's batch reads")
                .contains_key(&thread),
            "alice participates in the same thread through her own row"
        );
        assert!(
            participation_ids(&pool, bob.id, &[])
                .await
                .expect("the empty ask runs")
                .is_empty(),
            "an empty ask is the empty map"
        );
    }

    #[tokio::test]
    async fn participations_for_status_reads_the_local_roster_of_the_status_thread() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        let post = direct(&pool, &alice, &[&bob, &carol], "@bob @carol psst").await;
        bump(&pool, &post).await;
        let thread = linked_thread(&pool, &post).await;
        let alice_row = participation(&pool, alice.id, thread)
            .await
            .expect("the author participates");
        let bob_row = participation(&pool, bob.id, thread)
            .await
            .expect("the local addressee participates");

        let found = participations_for_status(&pool, post.id)
            .await
            .expect("the local roster reads");
        let mut pairs: Vec<(Uuid, Uuid)> = found.iter().map(|row| (row.user_id, row.id)).collect();
        pairs.sort();
        let mut expected = vec![(alice.id, alice_row.id), (bob.id, bob_row.id)];
        expected.sort();
        assert_eq!(
            pairs, expected,
            "both local sides, each with their own participation id"
        );
        assert!(
            !pairs.iter().any(|(user_id, _)| *user_id == carol.id),
            "the remote addressee owns no participation and is absent"
        );

        let public = status::insert(
            &pool,
            TEST_BASE_URL,
            &NewStatus::local(&alice, "a public post"),
        )
        .await
        .expect("the public fixture inserts");
        assert!(
            participations_for_status(&pool, public.id)
                .await
                .expect("the read runs")
                .is_empty(),
            "a non-direct status is threaded by nothing, so it has no roster"
        );
    }

    #[tokio::test]
    async fn get_participation_reads_the_owners_own_row() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let post = direct(&pool, &alice, &[&bob], "@bob psst").await;
        bump(&pool, &post).await;
        let thread = linked_thread(&pool, &post).await;
        let expected = participation(&pool, bob.id, thread)
            .await
            .expect("bob participates");

        let found = get_participation(&pool, bob.id, expected.id)
            .await
            .expect("the read runs");
        assert_eq!(found.as_ref(), Some(&expected), "the row reads back whole");
        assert_eq!(
            recipients(&pool, expected.id)
                .await
                .expect("the list reads"),
            vec![alice.id],
            "and its recipient list reads back through the model"
        );
        assert!(
            get_participation(&pool, alice.id, expected.id)
                .await
                .expect("the foreign read runs")
                .is_none(),
            "somebody else's participation is not there for them"
        );
    }

    #[tokio::test]
    async fn deleting_the_account_takes_its_side_of_every_thread() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let post = direct(&pool, &alice, &[&bob], "@bob psst").await;
        bump(&pool, &post).await;
        let thread = linked_thread(&pool, &post).await;
        let bob_row = participation(&pool, bob.id, thread)
            .await
            .expect("bob participates");

        let _gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the account purges");

        assert!(
            participation(&pool, bob.id, thread).await.is_none(),
            "the cascade withdrew his participation"
        );
        assert!(
            stored_recipients(&pool, bob_row.id).await.is_empty(),
            "and his recipient rows"
        );
        let alice_row = participation(&pool, alice.id, thread)
            .await
            .expect("alice's side stands");
        assert!(
            !stored_recipients(&pool, alice_row.id)
                .await
                .contains(&bob.id),
            "and bob dropped out of hers — his row went with the cascade"
        );
    }
}
