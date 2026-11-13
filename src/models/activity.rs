//! Activities read back by id: the stored remote ones by their `ap_id`
//! text, and this instance's own by the `UUIDv7` of the row behind them.
//!
//! This module owns no table: both readers walk the tables that keep an
//! activity behind their row (decision 6 — follows, favourites, blocks and
//! emoji reactions carry the `ap_id` of the activity they came from, and a
//! reblog *is* a status row whose `uri` is the `Announce` id).
//! [`find_by_ap_id`] is the question the incoming dispatcher's `Undo`
//! routing asks: an `Undo` that names a stored remote activity by id alone
//! cannot say what it withdraws, so the lookup answers the activity's type
//! and its actor's `ActivityPub` id, which is what the dispatcher needs to
//! pick the `Undo` handler and to check that the undoing actor is the
//! actor of the activity being undone. [`find_local_by_id`] is its local
//! counterpart, keyed on each row's own `UUIDv7` — the id
//! [`activity_id`](crate::federation::activities::activity_id) mints into
//! `{BASE_URL}/activities/{uuid}` — and answers every column the
//! [`activities`](crate::federation::activities) and
//! [`note`](crate::federation::note) builders need, so the
//! `GET /activities/{uuid}` route renders its document without another
//! read of the relation rows.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

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

/// [`find_local_by_id`]: one `UNION ALL` over the six activity-bearing
/// arms, each keyed on its own row `UUIDv7` — in order, the `Create` of a
/// live non-reblog status (`reblog_of_id IS NULL AND deleted_at IS NULL`),
/// the `Announce` of a live reblog row (`reblog_of_id IS NOT NULL`, live,
/// exactly [`FIND_SQL`]'s announce arm, with the original and its author
/// joined in as the object and the boosted actor), the `Like` of a
/// favourite, the `EmojiReact` of a reaction (with its stored emoji and
/// image URL), the `Follow` of a relationship and the `Block` of a
/// blocking edge; the relation arms join `statuses`/`users` for the object
/// and the other party of the pair the read is gated to. The id binds once
/// and every arm keys on it. A row id is per-table, so at most one arm can
/// answer it in practice; `LIMIT 1` keeps the arm order the tie-break if
/// two tables ever share one id.
const LOCAL_FIND_SQL: &str = "
    SELECT 'Create'::text AS kind, s.id, s.user_id AS actor_user_id,
           u.ap_id AS actor_ap_id, s.uri AS object_ap_id,
           NULL::uuid AS target_user_id, NULL::text AS target_ap_id,
           s.inserted_at, NULL::text AS emoji, NULL::text AS emoji_url
      FROM statuses s
      JOIN users u ON u.id = s.user_id
     WHERE s.id = $1 AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL
    UNION ALL
    SELECT 'Announce', s.id, s.user_id, u.ap_id,
           o.uri, o.user_id, ou.ap_id,
           s.inserted_at, NULL, NULL
      FROM statuses s
      JOIN users u ON u.id = s.user_id
      JOIN statuses o ON o.id = s.reblog_of_id
      JOIN users ou ON ou.id = o.user_id
     WHERE s.id = $1 AND s.reblog_of_id IS NOT NULL AND s.deleted_at IS NULL
    UNION ALL
    SELECT 'Like', fa.id, fa.user_id, u.ap_id,
           s.uri, s.user_id, NULL, fa.inserted_at, NULL, NULL
      FROM favourites fa
      JOIN users u ON u.id = fa.user_id
      JOIN statuses s ON s.id = fa.status_id
     WHERE fa.id = $1
    UNION ALL
    SELECT 'EmojiReact', er.id, er.user_id, u.ap_id,
           s.uri, s.user_id, NULL, er.inserted_at, er.emoji, er.url
      FROM emoji_reactions er
      JOIN users u ON u.id = er.user_id
      JOIN statuses s ON s.id = er.status_id
     WHERE er.id = $1
    UNION ALL
    SELECT 'Follow', f.id, f.follower_id, u.ap_id,
           fu.ap_id, f.followee_id, NULL, f.inserted_at, NULL, NULL
      FROM follows f
      JOIN users u ON u.id = f.follower_id
      JOIN users fu ON fu.id = f.followee_id
     WHERE f.id = $1
    UNION ALL
    SELECT 'Block', b.id, b.blocker_id, u.ap_id,
           bl.ap_id, b.blocked_id, NULL, b.inserted_at, NULL, NULL
      FROM blocks b
      JOIN users u ON u.id = b.blocker_id
      JOIN users bl ON bl.id = b.blocked_id
     WHERE b.id = $1
     LIMIT 1";

/// Which activity a [`LocalActivity`] row is, as the `GET
/// /activities/{uuid}` route names it; the `kind` literals of the
/// resolver's SQL arms are these variant names verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text")]
pub enum LocalActivityKind {
    /// A live, non-reblog status: its `Create`.
    Create,
    /// A live reblog row: its `Announce`.
    Announce,
    /// A favourite row: its `Like`.
    Like,
    /// An emoji-reaction row: its `EmojiReact`.
    EmojiReact,
    /// A follow row: its `Follow`.
    Follow,
    /// A block row: its `Block`.
    Block,
}

/// Everything the row behind one local activity carries, so the
/// `GET /activities/{uuid}` route renders the document and judges the
/// reader without a further read of the activity tables.
///
/// The fields the arm that resolved fills depend on the kind: a `Create`
/// names its status' `uri` and has no other party; an `Announce` names the
/// boosted original's `uri` as `object_ap_id` and its author as the target;
/// a `Like` or `EmojiReact` names the liked/reacted-to status' `uri` and
/// its author as the target (the reaction also carries its emoji columns);
/// a `Follow` or `Block` names the other actor's `ap_id` as both object and
/// target.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct LocalActivity {
    /// Which of the six kinds the resolved row is.
    pub kind: LocalActivityKind,
    /// The row's own `UUIDv7`, which names the activity (decision 6).
    pub id: Uuid,
    /// `users.id` of the acting account — the favourite's, reaction's,
    /// follow's or block's owner, or the status' author.
    pub actor_user_id: Uuid,
    /// The acting account's `ActivityPub` id, the document's `actor`.
    pub actor_ap_id: String,
    /// The object the activity names: a status `uri` (the status itself
    /// for a `Create`, the boosted original for an `Announce`, the
    /// liked/reacted-to post for a `Like`/`EmojiReact`) or the other
    /// actor's `ap_id` for a `Follow`/`Block`.
    pub object_ap_id: String,
    /// `users.id` of the activity's other party — the status' author, the
    /// followee, the blocked account — the reader the gate admits besides
    /// the actor; `None` for a `Create`, which has none.
    pub target_user_id: Option<Uuid>,
    /// The `Announce`'s boosted author's `ap_id`, the builder's
    /// addressing input; `None` on every other arm.
    pub target_ap_id: Option<String>,
    /// The row's creation moment, the activity's `published`.
    pub inserted_at: DateTime<Utc>,
    /// The reaction's emoji as stored; `Some` only for an `EmojiReact`.
    pub emoji: Option<String>,
    /// The custom emoji's image URL; `Some` only for a custom-emoji
    /// `EmojiReact`.
    pub emoji_url: Option<String>,
}

/// Looks `id` up among this instance's own activities, keyed on each row's
/// own `UUIDv7` — the shape [`activity_id`] mints into
/// `{BASE_URL}/activities/{uuid}`.
///
/// Answers the [`LocalActivity`] the route renders from, `None` when no
/// table carries the id and for a soft-deleted status, whose arms require
/// the live row.
///
/// [`activity_id`]: crate::federation::activities::activity_id
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn find_local_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<LocalActivity>> {
    let row = sqlx::query_as::<_, LocalActivity>(LOCAL_FIND_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
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

    /// The local resolver's answer for `id`, unwrapped for the passing arms.
    async fn found_local(pool: &PgPool, id: Uuid) -> LocalActivity {
        find_local_by_id(pool, id)
            .await
            .expect("the local lookup runs")
            .unwrap_or_else(|| panic!("{id} resolves to a local activity"))
    }

    /// Inserts a favourite of `user_id` on `status_id` and answers its row
    /// id — the id of the `Like` behind it.
    async fn favourite_row(pool: &PgPool, user_id: Uuid, status_id: Uuid) -> Uuid {
        let id = new_id();
        let _liked = sqlx::query(
            "INSERT INTO favourites (id, user_id, status_id, ap_id)
             VALUES ($1, $2, $3, 'https://test.example/activities/local-like')",
        )
        .bind(id)
        .bind(user_id)
        .bind(status_id)
        .execute(pool)
        .await
        .expect("the favourite fixture inserts");
        id
    }

    /// Inserts a reaction of `emoji` (with custom-emoji `url`) by
    /// `user_id` on `status_id` and answers its row id.
    async fn reaction_row(
        pool: &PgPool,
        user_id: Uuid,
        status_id: Uuid,
        emoji: &str,
        url: Option<&str>,
    ) -> Uuid {
        let id = new_id();
        let _reacted = sqlx::query(
            "INSERT INTO emoji_reactions (id, user_id, status_id, emoji, url, ap_id)
             VALUES ($1, $2, $3, $4, $5, 'https://test.example/activities/local-react-' || $1)",
        )
        .bind(id)
        .bind(user_id)
        .bind(status_id)
        .bind(emoji)
        .bind(url)
        .execute(pool)
        .await
        .expect("the reaction fixture inserts");
        id
    }

    /// A live, non-reblog status resolves as its own `Create`: the row id,
    /// the author, and the status' `uri` as the object, with no second
    /// party and no emoji.
    #[tokio::test]
    async fn a_live_original_resolves_as_its_create() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let id = new_id();
        status(
            &pool,
            id,
            "https://test.example/objects/create-1",
            &alice,
            None,
        )
        .await;

        let found = found_local(&pool, id).await;
        assert_eq!(found.kind, LocalActivityKind::Create, "the create arm");
        assert_eq!(found.id, id, "keyed on the status row's own id");
        assert_eq!(found.actor_user_id, alice.id, "the author acts");
        assert_eq!(found.actor_ap_id, alice.ap_id, "and is named by ap_id");
        assert_eq!(
            found.object_ap_id, "https://test.example/objects/create-1",
            "the object is the status' own uri"
        );
        assert_eq!(found.target_user_id, None, "a Create has no other party");
        assert_eq!(found.target_ap_id, None, "and no second actor");
        assert_eq!(found.emoji, None, "and no emoji");
    }

    /// A live reblog row resolves as its `Announce`: the original's `uri`
    /// as the object and the original's author as the target, so the route
    /// renders without another read.
    #[tokio::test]
    async fn a_live_reblog_resolves_as_its_announce() {
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
        let reblog_id = new_id();
        status(
            &pool,
            reblog_id,
            "https://test.example/activities/announce-1",
            &alice,
            Some(original_id),
        )
        .await;

        let found = found_local(&pool, reblog_id).await;
        assert_eq!(found.kind, LocalActivityKind::Announce, "the announce arm");
        assert_eq!(found.id, reblog_id, "keyed on the reblog row's own id");
        assert_eq!(found.actor_user_id, alice.id, "the reblogger acts");
        assert_eq!(
            found.object_ap_id, "https://test.example/objects/original",
            "the object is the boosted original's uri"
        );
        assert_eq!(
            found.target_user_id,
            Some(bob.id),
            "the target is its author"
        );
        assert_eq!(
            found.target_ap_id.as_deref(),
            Some(bob.ap_id.as_str()),
            "named by ap_id for the builder's addressing"
        );
    }

    /// A favourite row resolves as its `Like`, naming the liked status'
    /// `uri` and its author as the other party.
    #[tokio::test]
    async fn a_favourite_resolves_as_its_like() {
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
        let like_id = favourite_row(&pool, alice.id, original_id).await;

        let found = found_local(&pool, like_id).await;
        assert_eq!(found.kind, LocalActivityKind::Like, "the like arm");
        assert_eq!(found.id, like_id, "the favourite row's id is the Like's id");
        assert_eq!(found.actor_user_id, alice.id, "the favouriter acts");
        assert_eq!(
            found.object_ap_id, "https://test.example/objects/original",
            "the object is the liked status' uri"
        );
        assert_eq!(
            found.target_user_id,
            Some(bob.id),
            "the target is its author"
        );
        assert_eq!(found.emoji, None, "a Like carries no emoji");
    }

    /// A reaction row resolves as its `EmojiReact`, unicode and custom
    /// alike: the stored emoji is `content`, and only a custom one brings
    /// the image URL that becomes the `tag`.
    #[tokio::test]
    async fn a_reaction_resolves_as_its_emoji_react() {
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
        let unicode_id = reaction_row(&pool, alice.id, original_id, "🎉", None).await;
        let custom_id = reaction_row(
            &pool,
            alice.id,
            original_id,
            ":railroad_track:",
            Some("https://test.example/emoji/railroad_track.png"),
        )
        .await;

        let unicode = found_local(&pool, unicode_id).await;
        assert_eq!(unicode.kind, LocalActivityKind::EmojiReact, "the react arm");
        assert_eq!(unicode.actor_user_id, alice.id, "the reactor acts");
        assert_eq!(unicode.emoji.as_deref(), Some("🎉"), "the stored emoji");
        assert_eq!(unicode.emoji_url, None, "a unicode reaction has no image");
        assert_eq!(
            unicode.object_ap_id, "https://test.example/objects/original",
            "the object is the reacted-to status' uri"
        );
        assert_eq!(
            unicode.target_user_id,
            Some(bob.id),
            "the target is its author"
        );

        let custom = found_local(&pool, custom_id).await;
        assert_eq!(custom.kind, LocalActivityKind::EmojiReact, "the react arm");
        assert_eq!(custom.emoji.as_deref(), Some(":railroad_track:"));
        assert_eq!(
            custom.emoji_url.as_deref(),
            Some("https://test.example/emoji/railroad_track.png"),
            "the custom emoji's image URL rides along"
        );
    }

    /// A follow row resolves as its `Follow`: the followee is both the
    /// activity's object and its other party.
    #[tokio::test]
    async fn a_follow_resolves_as_its_follow() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let follow_id = new_id();
        let _followed = sqlx::query(
            "INSERT INTO follows (id, follower_id, followee_id, state, ap_id)
             VALUES ($1, $2, $3, 'accepted', 'https://test.example/activities/local-follow')",
        )
        .bind(follow_id)
        .bind(alice.id)
        .bind(bob.id)
        .execute(&pool)
        .await
        .expect("the follow fixture inserts");

        let found = found_local(&pool, follow_id).await;
        assert_eq!(found.kind, LocalActivityKind::Follow, "the follow arm");
        assert_eq!(found.id, follow_id, "the follow row's id is the activity's");
        assert_eq!(found.actor_user_id, alice.id, "the follower acts");
        assert_eq!(found.object_ap_id, bob.ap_id, "the followee is the object");
        assert_eq!(found.target_user_id, Some(bob.id), "and the other party");
    }

    /// A block row resolves as its `Block`: the blocked account is both
    /// the object and the other party.
    #[tokio::test]
    async fn a_block_resolves_as_its_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let block_id = new_id();
        let _blocked = sqlx::query(
            "INSERT INTO blocks (id, blocker_id, blocked_id, ap_id)
             VALUES ($1, $2, $3, 'https://test.example/activities/local-block')",
        )
        .bind(block_id)
        .bind(alice.id)
        .bind(bob.id)
        .execute(&pool)
        .await
        .expect("the block fixture inserts");

        let found = found_local(&pool, block_id).await;
        assert_eq!(found.kind, LocalActivityKind::Block, "the block arm");
        assert_eq!(found.id, block_id, "the block row's id is the activity's");
        assert_eq!(found.actor_user_id, alice.id, "the blocker acts");
        assert_eq!(found.object_ap_id, bob.ap_id, "the blocked is the object");
        assert_eq!(found.target_user_id, Some(bob.id), "and the other party");
    }

    /// An id no activity row carries answers `None` — and a soft-deleted
    /// status resolves to no arm at all: the row exists, but its `Create`
    /// is gone with the post.
    #[tokio::test]
    async fn unknown_and_tombstoned_ids_resolve_to_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let id = new_id();
        status(&pool, id, "https://test.example/objects/gone", &alice, None).await;
        let _tombstoned = sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("the status tombstones");

        for unrouted in [Uuid::now_v7(), id] {
            assert!(
                find_local_by_id(&pool, unrouted)
                    .await
                    .expect("the local lookup runs")
                    .is_none(),
                "{unrouted} names no local activity"
            );
        }
    }
}
