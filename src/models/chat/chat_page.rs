//! The list of one participant's chats: [`page_for_user`], newest activity
//! first, under [`ChatFilter`]'s restrictions.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, pagination::PageParams};

use super::{Chat, ChatFilter};

/// [`page_for_user`]: one participant's chats. The text stops where its one
/// placeholder belongs, the way [`REFERENCE_SELECT_SQL`] does; the pinned
/// restriction and the keyset [`PageParams::apply_keyset`] appends follow, the
/// latter running on the chat id `c.id`.
///
/// [`REFERENCE_SELECT_SQL`]: super::references::REFERENCE_SELECT_SQL
const CHATS_PAGE_SQL: &str = "SELECT c.* FROM chats c WHERE c.user_id = ";

/// The chats of `user_id`, newest activity first, `page`'s `limit` at a time,
/// `filter`'s restrictions applied.
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
        models::chat::test_support::{ALL, chat_ids, local, page, send},
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
}
