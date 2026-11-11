//! The stored remote activities, read back by `ActivityPub` id.
//!
//! This module owns no table: it is the one question the incoming
//! dispatcher's `Undo` routing asks across the tables that keep an
//! activity behind their row (decision 6 — follows, favourites, blocks and
//! emoji reactions carry the `ap_id` of the activity they came from, and a
//! reblog *is* a status row whose `uri` is the `Announce` id). An `Undo`
//! that names such an activity by id alone cannot say what it withdraws;
//! [`find_by_ap_id`] looks the id up in each of those tables and answers
//! the activity's type and its actor's `ActivityPub` id, which is what the
//! dispatcher needs to pick the `Undo` handler and to check that the
//! undoing actor is the actor of the activity being undone.

use sqlx::PgPool;

use crate::error::{AppError, AppResult};

/// [`find_by_ap_id`]: one `UNION ALL` over the five activity-bearing
/// tables, each arm naming its own type and joining `users` for the actor
/// id. The id binds five times; each arm's own unique index
/// (`ap_id`/`uri`) answers its lookup. A withdrawn reblog's tombstone
/// (`deleted_at` set) proves nothing any more, so that arm reads live rows
/// only; the relation tables have no tombstones — withdrawal deletes the
/// row, which is exactly why its `Undo` stops resolving.
const FIND_SQL: &str = "
    SELECT 'Follow' AS kind, u.ap_id AS actor
      FROM follows f
      JOIN users u ON u.id = f.follower_id
     WHERE f.ap_id = $1
    UNION ALL
    SELECT 'Like', u.ap_id
      FROM favourites fa
      JOIN users u ON u.id = fa.user_id
     WHERE fa.ap_id = $1
    UNION ALL
    SELECT 'Block', u.ap_id
      FROM blocks b
      JOIN users u ON u.id = b.blocker_id
     WHERE b.ap_id = $1
    UNION ALL
    SELECT 'EmojiReact', u.ap_id
      FROM emoji_reactions er
      JOIN users u ON u.id = er.user_id
     WHERE er.ap_id = $1
    UNION ALL
    SELECT 'Announce', u.ap_id
      FROM statuses s
      JOIN users u ON u.id = s.user_id
     WHERE s.uri = $1 AND s.reblog_of_id IS NOT NULL AND s.deleted_at IS NULL
     LIMIT 1";

/// The activity stored behind an `ActivityPub` id: its type as this
/// instance's dispatcher names it, and the `ActivityPub` id of the actor
/// the activity came from.
pub type StoredActivity = (&'static str, String);

/// Looks `ap_id` up among the stored remote activities ([`StoredActivity`]'s
/// tables), `None` when no row carries it.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn find_by_ap_id(pool: &PgPool, ap_id: &str) -> AppResult<Option<StoredActivity>> {
    let row: Option<(String, String)> = sqlx::query_as(FIND_SQL)
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    let Some((kind, actor)) = row else {
        return Ok(None);
    };
    let kind: &'static str = match kind.as_str() {
        "Follow" => "Follow",
        "Like" => "Like",
        "Block" => "Block",
        "EmojiReact" => "EmojiReact",
        "Announce" => "Announce",
        other => {
            return Err(AppError::Internal(format!(
                "the activity lookup answered an unlisted type: {other}"
            )));
        }
    };
    Ok(Some((kind, actor)))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::{
        models::user::{NewUser, User, insert, local_ap_id},
        util::id::new_id,
    };

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id("https://test.example", nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Inserts `id` into `statuses` with `uri`, `local`, and `reblog_of_id`
    /// as given; `actor` authors it.
    async fn status(pool: &PgPool, id: Uuid, uri: &str, actor: &User, reblog_of: Option<Uuid>) {
        let inserted = sqlx::query(
            "INSERT INTO statuses (id, uri, user_id, conversation_id, local, visibility, reblog_of_id)
             VALUES ($1, $2, $3, $1, true, 'public', $4)",
        )
        .bind(id)
        .bind(uri)
        .bind(actor.id)
        .bind(reblog_of)
        .execute(pool)
        .await
        .expect("the status fixture inserts");
        assert_eq!(inserted.rows_affected(), 1, "the status row is there");
    }

    /// The lookup's answer for `ap_id`, unwrapped for the passing arms.
    async fn found(pool: &PgPool, ap_id: &str) -> StoredActivity {
        find_by_ap_id(pool, ap_id)
            .await
            .expect("the lookup runs")
            .unwrap_or_else(|| panic!("{ap_id} resolves"))
    }

    /// Each of the five activity-bearing tables answers its own type and
    /// its actor's id, by the `ap_id` (or reblog `uri`) it stores.
    #[tokio::test]
    async fn every_activity_table_answers_its_type_and_actor() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original_id = new_id();
        status(
            &pool,
            original_id,
            "https://test.example/objects/original",
            &bob,
            None,
        )
        .await;

        let _followed = sqlx::query(
            "INSERT INTO follows (id, follower_id, followee_id, state, ap_id)
             VALUES ($1, $2, $3, 'accepted', 'https://peer.example/activities/follow-1')",
        )
        .bind(new_id())
        .bind(alice.id)
        .bind(bob.id)
        .execute(&pool)
        .await
        .expect("the follow fixture inserts");
        let _liked = sqlx::query(
            "INSERT INTO favourites (id, user_id, status_id, ap_id)
             VALUES ($1, $2, $3, 'https://peer.example/activities/like-1')",
        )
        .bind(new_id())
        .bind(alice.id)
        .bind(original_id)
        .execute(&pool)
        .await
        .expect("the favourite fixture inserts");
        let _blocked = sqlx::query(
            "INSERT INTO blocks (id, blocker_id, blocked_id, ap_id)
             VALUES ($1, $2, $3, 'https://peer.example/activities/block-1')",
        )
        .bind(new_id())
        .bind(alice.id)
        .bind(bob.id)
        .execute(&pool)
        .await
        .expect("the block fixture inserts");
        let _reacted = sqlx::query(
            "INSERT INTO emoji_reactions (id, user_id, status_id, emoji, ap_id)
             VALUES ($1, $2, $3, '👍', 'https://peer.example/activities/react-1')",
        )
        .bind(new_id())
        .bind(alice.id)
        .bind(original_id)
        .execute(&pool)
        .await
        .expect("the reaction fixture inserts");
        status(
            &pool,
            new_id(),
            "https://test.example/activities/announce-1",
            &alice,
            Some(original_id),
        )
        .await;

        for (ap_id, want_kind) in [
            ("https://peer.example/activities/follow-1", "Follow"),
            ("https://peer.example/activities/like-1", "Like"),
            ("https://peer.example/activities/block-1", "Block"),
            ("https://peer.example/activities/react-1", "EmojiReact"),
            ("https://test.example/activities/announce-1", "Announce"),
        ] {
            let (kind, actor) = found(&pool, ap_id).await;
            assert_eq!(kind, want_kind, "{ap_id} reads as its own table's type");
            assert_eq!(
                actor, alice.ap_id,
                "and every fixture activity was alice's: {ap_id}"
            );
        }
    }

    /// An id no activity row carries answers `None` — and a plain (not
    /// reblog) status's `uri` is no `Announce` id.
    #[tokio::test]
    async fn unknown_ids_and_plain_status_uris_resolve_to_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        status(
            &pool,
            new_id(),
            "https://test.example/objects/plain-post",
            &alice,
            None,
        )
        .await;

        for ap_id in [
            "https://peer.example/activities/never-seen",
            "https://test.example/objects/plain-post",
        ] {
            assert!(
                find_by_ap_id(&pool, ap_id)
                    .await
                    .expect("the lookup runs")
                    .is_none(),
                "{ap_id} names no stored activity"
            );
        }
    }

    /// A tombstoned reblog answers nothing: its withdrawal already took the
    /// fact down.
    #[tokio::test]
    async fn a_withdrawn_reblog_resolves_to_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let original_id = new_id();
        status(
            &pool,
            original_id,
            "https://test.example/objects/original",
            &alice,
            None,
        )
        .await;
        let reblog_id = new_id();
        status(
            &pool,
            reblog_id,
            "https://test.example/activities/announce-1",
            &alice,
            Some(original_id),
        )
        .await;
        let _tombstoned = sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
            .bind(reblog_id)
            .execute(&pool)
            .await
            .expect("the reblog tombstones");

        assert!(
            find_by_ap_id(&pool, "https://test.example/activities/announce-1")
                .await
                .expect("the lookup runs")
                .is_none(),
            "the withdrawn announce is gone"
        );
    }
}
