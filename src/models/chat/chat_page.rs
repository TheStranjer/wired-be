//! The list of one participant's chats: [`page_for_user`], newest activity
//! first, under [`ChatFilter`]'s restrictions.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, pagination::PageParams};

use super::{Chat, ChatFilter};

/// [`page_for_user`]: one participant's chats. The text stops where its one
/// placeholder belongs, the way [`REFERENCE_SELECT_SQL`] does; the block and
/// mute exclusions, the pinned restriction and the keyset
/// [`PageParams::apply_keyset`] appends follow, the latter running on the chat
/// id `c.id`.
///
/// [`REFERENCE_SELECT_SQL`]: super::references::REFERENCE_SELECT_SQL
const CHATS_PAGE_SQL: &str = "SELECT c.* FROM chats c WHERE c.user_id = ";

/// What the list never shows: a chat whose counterpart the owner has blocked
/// (Pleroma's `cached_blocked_users_ap_ids`).
///
/// Only the owner's own blocks count; a counterpart who blocked the owner stays
/// listed.
const EXCLUDE_BLOCKED_SQL: &str = " AND NOT EXISTS (SELECT 1 FROM blocks b \
     WHERE b.blocker_id = c.user_id AND b.blocked_id = c.recipient_id)";

/// What the list hides unless `with_muted`: a chat whose counterpart the owner
/// has a live mute on, the same live-mute test as `MUTE_LIVE_ARMS` in
/// `status_search.rs` (no expiry, or one still ahead).
const EXCLUDE_MUTED_SQL: &str = " AND NOT EXISTS (SELECT 1 FROM mutes m \
     WHERE m.muter_id = c.user_id AND m.muted_id = c.recipient_id \
     AND (m.expires_at IS NULL OR m.expires_at > now()))";

/// The chats of `user_id`, newest activity first, `page`'s `limit` at a time,
/// `filter`'s restrictions applied.
///
/// Chats with accounts `user_id` has blocked are never listed, and those with
/// accounts it has muted only when [`ChatFilter::with_muted`] is set.
///
/// The list order is [`updated_at`](Chat::updated_at), which is what Pleroma's
/// `Chat.for_user_query` orders by, while the cursors are ids, which is what
/// [`PageParams::apply_keyset`] makes of them. Both are honoured: the SQL
/// selects and bounds the page by id, and the fetched rows are then ordered by
/// activity. Since a chat's id never changes while its `updated_at` does, no
/// page can hold a longer activity order than itself, and
/// [`PageParams::finish`]'s `min_id` flip has nothing left to undo here.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn page_for_user(
    pool: &PgPool,
    user_id: Uuid,
    page: PageParams,
    filter: ChatFilter,
) -> AppResult<Vec<Chat>> {
    let mut builder = QueryBuilder::<Postgres>::new(CHATS_PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // the result keeps these statements from being unused (decision 14).
    let _bound: &mut _ = builder.push_bind(user_id);
    let _blocked: &mut _ = builder.push(EXCLUDE_BLOCKED_SQL);
    if !filter.with_muted {
        let _muted: &mut _ = builder.push(EXCLUDE_MUTED_SQL);
    }
    if let Some(pinned) = filter.pinned {
        let _pinned: &mut _ = builder.push(" AND c.pinned = ").push_bind(pinned);
    }
    page.apply_keyset(&mut builder, "c.id");
    let mut chats = builder.build_query_as::<Chat>().fetch_all(pool).await?;
    sort_by_activity(&mut chats);
    Ok(chats)
}

/// Orders a page of chats by activity, newest first, the id breaking a tie
/// between two chats bumped in the same transaction.
fn sort_by_activity(chats: &mut [Chat]) {
    chats.sort_unstable_by(|newer, older| {
        older
            .updated_at
            .cmp(&newer.updated_at)
            .then(older.id.cmp(&newer.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{
            chat::test_support::{ALL, chat_ids, local, page, send},
            mute::MuteOptions,
            user::User,
        },
        util::id::new_id,
    };

    #[tokio::test]
    async fn chat_pages_walk_by_activity_and_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let mut opened = Vec::new();
        for nickname in ["bob", "carol", "dave"] {
            let partner = local(&pool, nickname).await;
            let (_, reference, _) = send(&pool, &alice, &partner, "hello").await;
            opened.push(reference.chat_id);
        }

        let all = page_for_user(&pool, alice.id, page(ALL), ChatFilter::default())
            .await
            .unwrap();
        assert_eq!(
            chat_ids(&all),
            vec![opened[2], opened[1], opened[0]],
            "the list is newest activity first"
        );

        let first = page_for_user(&pool, alice.id, page("limit=2"), ChatFilter::default())
            .await
            .unwrap();
        assert_eq!(first.len(), 2, "the limit holds the page down");
        assert_eq!(
            chat_ids(&first),
            vec![opened[2], opened[1]],
            "the two newest ids, ordered by activity"
        );

        let second = page_for_user(
            &pool,
            alice.id,
            page(&format!("limit=2&max_id={}", opened[1])),
            ChatFilter::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            chat_ids(&second),
            vec![opened[0]],
            "max_id walks back to the oldest chat"
        );

        let since = page_for_user(
            &pool,
            alice.id,
            page(&format!("since_id={}", opened[1])),
            ChatFilter::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            chat_ids(&since),
            vec![opened[2]],
            "since_id keeps only the chats with a newer id"
        );

        assert!(
            page_for_user(
                &pool,
                alice.id,
                page(&format!("max_id={}", opened[0])),
                ChatFilter::default(),
            )
            .await
            .unwrap()
            .is_empty(),
            "nothing is older than the oldest chat"
        );
        assert!(
            page_for_user(&pool, new_id(), page(ALL), ChatFilter::default())
                .await
                .unwrap()
                .is_empty(),
            "and a user with no chats gets no rows"
        );
    }

    /// The ids of `owner`'s chats under `with_muted`.
    async fn listed(pool: &PgPool, owner: &User, with_muted: bool) -> Vec<Uuid> {
        let filter = ChatFilter {
            pinned: None,
            with_muted,
        };
        chat_ids(
            &page_for_user(pool, owner.id, page(ALL), filter)
                .await
                .unwrap(),
        )
    }

    /// Opens a chat of `owner` with `partner` and returns its id.
    async fn open(pool: &PgPool, owner: &User, partner: &User) -> Uuid {
        send(pool, owner, partner, "hello").await.1.chat_id
    }

    #[tokio::test]
    async fn a_blocked_counterpart_is_never_listed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let _bobs = open(&pool, &alice, &bob).await;
        let with_carol = open(&pool, &alice, &carol).await;
        let _row = crate::models::block::block(&pool, crate::config::TEST_BASE_URL, &alice, &bob)
            .await
            .unwrap();

        for with_muted in [false, true] {
            assert_eq!(
                listed(&pool, &alice, with_muted).await,
                vec![with_carol],
                "the blocked chat is absent with with_muted={with_muted}"
            );
        }
    }

    #[tokio::test]
    async fn a_counterpart_who_blocked_the_owner_stays_listed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let with_bob = open(&pool, &alice, &bob).await;
        let _row = crate::models::block::block(&pool, crate::config::TEST_BASE_URL, &bob, &alice)
            .await
            .unwrap();

        assert_eq!(
            listed(&pool, &alice, false).await,
            vec![with_bob],
            "only the owner's own blocks hide a chat"
        );
    }

    #[tokio::test]
    async fn a_muted_counterpart_is_listed_only_with_muted() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let with_bob = open(&pool, &alice, &bob).await;
        let with_carol = open(&pool, &alice, &carol).await;
        let _row = crate::models::mute::mute(&pool, &alice, &bob, MuteOptions::default())
            .await
            .unwrap();

        assert_eq!(
            listed(&pool, &alice, false).await,
            vec![with_carol],
            "the muted chat is hidden by default"
        );
        let mut both = listed(&pool, &alice, true).await;
        both.sort_unstable();
        let mut want = vec![with_bob, with_carol];
        want.sort_unstable();
        assert_eq!(both, want, "with_muted brings it back");
    }

    #[tokio::test]
    async fn an_expired_mute_hides_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let with_bob = open(&pool, &alice, &bob).await;
        let _written: sqlx::postgres::PgQueryResult = sqlx::query(
            "INSERT INTO mutes (id, muter_id, muted_id, expires_at)
             VALUES ($1, $2, $3, now() - interval '1 hour')",
        )
        .bind(new_id())
        .bind(alice.id)
        .bind(bob.id)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(
            listed(&pool, &alice, false).await,
            vec![with_bob],
            "a lapsed mute is absent to the list"
        );
    }
}
