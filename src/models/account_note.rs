//! The `account_notes` table: one account's private note on another.
//!
//! Reference: Pleroma's `Pleroma.UserNote` (`lib/pleroma/user_note.ex`).
//! Its `user_notes` table keys the note on `(source_id, target_id)` and
//! `show/2` reads the comment back as the empty string when the pair has no
//! row; this table is the same fact under the ticket's `user_id`/`target_id`
//! names, and [`get_many`] is that reading batched over many targets.
//!
//! Pleroma's `create/3` replaces the comment on conflict; [`set`] does the
//! same and additionally deletes the row when the comment is empty or all
//! whitespace, so a blank note and no note are one and the same stored
//! state — which is exactly what [`get_many`]'s absence-means-empty answer
//! renders either way.
//!
//! Notes are strictly per viewer: two accounts noting the same target are
//! two independent rows and one never reads another's. Deleting a user
//! takes their notes with it through the foreign key's cascade, in both
//! directions — the notes they wrote and the notes written about them.

use std::collections::HashMap;

use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

/// [`set`]'s write when the comment is not blank: Pleroma's
/// `on_conflict: {:replace, [:comment]}` upsert, restamping `updated_at`.
/// A write that conflicts keeps the row's stored id, so a note is one row
/// per pair, rewritten in place (decision 4).
const UPSERT_SQL: &str = "INSERT INTO account_notes (id, user_id, target_id, comment)
     VALUES ($1, $2, $3, $4)
ON CONFLICT (user_id, target_id) DO UPDATE SET
      comment = EXCLUDED.comment,
      updated_at = now()";

/// [`set`]'s other write: a blank comment stores nothing, so the pair's
/// row (if it has one) goes away and the note is back to the empty default.
const DELETE_SQL: &str = "DELETE FROM account_notes WHERE user_id = $1 AND target_id = $2";

/// [`get_many`]: the comments of one viewer's notes over a batch of
/// targets. The `$2` cast lets an empty request bind as the empty array
/// rather than as an untypeable `NULL` (the markers' rule); pairs without a
/// row are simply absent from the result.
const GET_MANY_SQL: &str = "SELECT target_id, comment FROM account_notes
      WHERE user_id = $1 AND target_id = ANY($2::uuid[])";

/// Stores `comment` as `user_id`'s note on `target_id`, or removes the note
/// when the comment is empty or all whitespace.
///
/// A pair that already has a note keeps its row and has its comment
/// replaced in place (Pleroma's `create/3` replace-on-conflict); a blank
/// comment deletes the row whether or not one was there, so clearing is
/// idempotent. The comment is stored verbatim: trimming only decides which
/// of the two writes runs, never what lands in `comment`. Length limits are
/// the endpoint's, not the model's.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set(pool: &PgPool, user_id: Uuid, target_id: Uuid, comment: &str) -> AppResult<()> {
    if comment.trim().is_empty() {
        sqlx::query(DELETE_SQL)
            .bind(user_id)
            .bind(target_id)
            .execute(pool)
            .await
            .map(drop)?;
        return Ok(());
    }
    sqlx::query(UPSERT_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(target_id)
        .bind(comment)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The notes `user_id` has on the accounts in `target_ids`, keyed by target.
///
/// Targets without a note are absent from the map — the caller's blank
/// default (the empty string) is what [`Relationship::note`] renders for
/// them, and the batch loader folds the two together. An empty
/// `target_ids` answers an empty `HashMap` without reading anything.
///
/// [`Relationship::note`]: crate::entities::relationship::Relationship::note
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_many(
    pool: &PgPool,
    user_id: Uuid,
    target_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, String>> {
    let rows = sqlx::query_as::<_, (Uuid, String)>(GET_MANY_SQL)
        .bind(user_id)
        .bind(target_ids.to_vec())
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{NewUser, User, insert, local_ap_id},
    };

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// The single note of `owner` on `target`, or `None` — the map a
    /// one-target [`get_many`] answers, unwrapped for the assertions.
    async fn note_of(pool: &PgPool, owner: &User, target: &User) -> Option<String> {
        get_many(pool, owner.id, &[target.id])
            .await
            .expect("the note read succeeds")
            .get(&target.id)
            .cloned()
    }

    /// Counts the rows of the table, for the "the row is really gone /
    /// still one row" assertions.
    async fn row_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM account_notes")
            .fetch_one(pool)
            .await
            .expect("the count reads")
    }

    /// The stored `(id, inserted_at, updated_at)` of the pair's row, for
    /// the rewrite-in-place assertions.
    async fn row_times(pool: &PgPool, owner: &User, target: &User) -> (Uuid, bool) {
        let (id, same_stamp) = sqlx::query_as::<_, (Uuid, bool)>(
            "SELECT id, inserted_at = updated_at FROM account_notes
              WHERE user_id = $1 AND target_id = $2",
        )
        .bind(owner.id)
        .bind(target.id)
        .fetch_one(pool)
        .await
        .expect("the row is there to read");
        (id, same_stamp)
    }

    #[tokio::test]
    async fn a_first_note_is_written_and_read_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        set(&pool, alice.id, bob.id, "met at a conference")
            .await
            .expect("the first note stores");

        assert_eq!(
            note_of(&pool, &alice, &bob).await.as_deref(),
            Some("met at a conference"),
            "the comment comes back verbatim"
        );
        assert_eq!(row_count(&pool).await, 1, "and it is one row");
    }

    #[tokio::test]
    async fn a_second_note_replaces_the_first_in_the_same_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        set(&pool, alice.id, bob.id, "first").await.unwrap();
        let (before_id, fresh) = row_times(&pool, &alice, &bob).await;
        assert!(fresh, "the insert leaves inserted_at and updated_at equal");

        set(&pool, alice.id, bob.id, "second")
            .await
            .expect("the overwrite stores");
        assert_eq!(
            note_of(&pool, &alice, &bob).await.as_deref(),
            Some("second"),
            "the comment is replaced, not appended"
        );
        let (after_id, _) = row_times(&pool, &alice, &bob).await;
        assert_eq!(after_id, before_id, "and it is the same row");
        assert_eq!(row_count(&pool).await, 1, "still exactly one note");
    }

    #[tokio::test]
    async fn an_empty_comment_deletes_the_note() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        set(&pool, alice.id, bob.id, "keep this").await.unwrap();

        set(&pool, alice.id, bob.id, "")
            .await
            .expect("clearing stores");
        assert_eq!(note_of(&pool, &alice, &bob).await, None, "the note is gone");
        assert_eq!(row_count(&pool).await, 0, "and so is the row");

        set(&pool, alice.id, bob.id, "")
            .await
            .expect("clearing again is idempotent");
        assert_eq!(row_count(&pool).await, 0, "nothing came back");
    }

    #[tokio::test]
    async fn a_whitespace_comment_is_an_empty_comment() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        set(&pool, alice.id, bob.id, "keep this").await.unwrap();

        set(&pool, alice.id, bob.id, " \n\t ")
            .await
            .expect("a whitespace clear stores");
        assert_eq!(
            note_of(&pool, &alice, &bob).await,
            None,
            "all-whitespace deletes like the empty string"
        );

        set(&pool, alice.id, bob.id, "   ")
            .await
            .expect("it deletes even with no row to delete");
        assert_eq!(row_count(&pool).await, 0, "and nothing was stored");
    }

    #[tokio::test]
    async fn notes_are_each_viewer_s_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        set(&pool, alice.id, bob.id, "alice's view").await.unwrap();

        assert_eq!(
            note_of(&pool, &carol, &bob).await,
            None,
            "carol sees nothing of alice's note"
        );
        set(&pool, carol.id, bob.id, "carol's view")
            .await
            .expect("carol's own note stores");
        assert_eq!(
            note_of(&pool, &alice, &bob).await.as_deref(),
            Some("alice's view"),
            "and bob's write left alice's alone"
        );
        assert_eq!(row_count(&pool).await, 2, "one row per viewer");
    }

    #[tokio::test]
    async fn an_account_can_note_itself() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        set(&pool, alice.id, alice.id, "self-reminder")
            .await
            .expect("the self note stores");
        assert_eq!(
            note_of(&pool, &alice, &alice).await.as_deref(),
            Some("self-reminder"),
            "and reads back like any other"
        );
    }

    #[tokio::test]
    async fn get_many_batches_targets_and_skips_the_unnoted() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let dave = user(&pool, "dave").await;
        set(&pool, alice.id, bob.id, "b").await.unwrap();
        set(&pool, alice.id, dave.id, "d").await.unwrap();
        set(&pool, carol.id, bob.id, "not alice's").await.unwrap();

        let targets = [bob.id, carol.id, dave.id];
        let notes = get_many(&pool, alice.id, &targets)
            .await
            .expect("the batch reads");
        assert_eq!(notes.len(), 2, "only alice's own two notes");
        assert_eq!(notes.get(&bob.id).expect("bob").as_str(), "b");
        assert_eq!(notes.get(&dave.id).expect("dave").as_str(), "d");
        assert!(
            !notes.contains_key(&carol.id),
            "an unnoted target is absent, not empty"
        );

        let nothing = get_many(&pool, alice.id, &[])
            .await
            .expect("an empty batch is not a failure");
        assert!(nothing.is_empty(), "and answers nothing at all");
    }

    #[tokio::test]
    async fn deleting_either_side_takes_the_note() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        set(&pool, alice.id, bob.id, "doomed").await.unwrap();

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the fixture target deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");
        assert_eq!(
            row_count(&pool).await,
            0,
            "a note about the deleted account went with it"
        );

        let carol = user(&pool, "carol").await;
        let dave = user(&pool, "dave").await;
        set(&pool, carol.id, dave.id, "hers").await.unwrap();
        let gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(carol.id)
            .execute(&pool)
            .await
            .expect("the fixture author deletes");
        assert_eq!(gone.rows_affected(), 1, "and so did its author");
        assert_eq!(row_count(&pool).await, 0, "the author's notes cascaded too");
    }

    #[tokio::test]
    async fn a_long_comment_is_stored_verbatim() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        // The endpoint caps at 2000 characters; the table is text and the
        // model stores whatever it is handed, around that cap.
        let long = "é".repeat(2500);
        set(&pool, alice.id, bob.id, &long)
            .await
            .expect("text is text");
        assert_eq!(
            note_of(&pool, &alice, &bob).await.as_deref(),
            Some(long.as_str()),
            "not cut, not refused, not re-encoded"
        );
    }
}
