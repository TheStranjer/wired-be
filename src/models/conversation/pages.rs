//! The paged listings of this module's two tables: [`page_for_user`]'s
//! conversation page (with its recipients filter and newest-activity
//! order) and [`page_direct_statuses`]'s page of one thread's live direct
//! statuses — each one [`PageParams`] keyset over a SQL stem that stops at
//! its placeholder.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::status::Status,
    pagination::{PageParams, keyset_page},
};

use super::Participation;

/// [`page_for_user`]: the account's threads. The text stops where its one
/// placeholder belongs; the recipients filter [`page_for_user`] pushes
/// between the binds and the keyset [`PageParams::apply_keyset`] appends
/// runs on the participation id `p.id` — the ticket's simplification of
/// Pleroma's `updated_at DESC` listing, walked newest-participation first
/// and then ordered by activity in the fetched page.
const PAGE_SQL: &str = "SELECT p.* FROM conversation_participations p WHERE p.user_id = ";

/// [`page_for_user`]'s recipients filter, its tail: the head ` AND ` and the
/// bound array go before this text, so the placeholder lands in its slot
/// with the statement's own numbering (the array arrives typed `uuid[]`,
/// and the containment is `uuid[] <@ uuid[]`). Pleroma's
/// `Participation.restrict_recipients`, restated over this schema's
/// recipient rows: here the ids a participation addresses, not the
/// accounts holding one. A participation with no addressees aggregates to
/// `NULL` and the containment is null — every named account fails to be
/// among them, which is the right exclusion.
const RECIPIENTS_FILTER_TAIL_SQL: &str = " <@ (SELECT array_agg(r.user_id) \
     FROM conversation_participation_recipients r WHERE r.participation_id = p.id)";

/// [`page_direct_statuses`]: the live `direct` statuses of one thread. The
/// text stops where its one placeholder belongs and the keyset
/// [`PageParams::apply_keyset`] appends run on the status id `s.id` —
/// id descending is creation order (decision 4), so the page is newest
/// activity first, the order the endpoint answers in.
const THREAD_STATUSES_SQL: &str = "SELECT s.* FROM statuses s \
     WHERE s.direct_conversation_id = ";

/// `user_id`'s conversation list, newest activity first, `page`'s cursors
/// and cap honoured, `recipients`' accounts all addressed by each kept
/// participation.
///
/// The id keyset works the way the chats listing's does: [`PageParams`]
/// selects and bounds the page by participation id (decision 4's creation
/// order), and the fetched page is then ordered by [`updated_at`] — what
/// every new direct status bumps, Pleroma's `for_user` order — newest
/// first, the participation id breaking ties. The cursors a client follows
/// are still the ids the `Link` header names. `recipients` is Pleroma's
/// `restrict_recipients`: a participation whose addressee list misses any
/// named account leaves the listing; an empty slice asks for everything.
/// An account nobody has DM'd answers the empty `Vec`.
///
/// [`updated_at`]: Participation::updated_at
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn page_for_user(
    pool: &PgPool,
    user_id: Uuid,
    page: PageParams,
    recipients: &[Uuid],
) -> AppResult<Vec<Participation>> {
    let mut builder = QueryBuilder::<Postgres>::new(PAGE_SQL);
    let _user: &mut _ = builder.push_bind(user_id);
    if !recipients.is_empty() {
        let _filter: &mut _ = builder
            .push(" AND ")
            .push_bind(recipients.to_vec())
            .push(RECIPIENTS_FILTER_TAIL_SQL);
    }
    let mut rows: Vec<Participation> = keyset_page(pool, builder, "p.id", &page).await?;
    sort_by_activity(&mut rows);
    Ok(rows)
}

/// [`page_for_user`]'s list order: the participation whose thread saw
/// activity last first, its id breaking a tie (the chats listing's rule,
/// restated over [`updated_at`], which every bump moves).
///
/// [`updated_at`]: Participation::updated_at
fn sort_by_activity(rows: &mut [Participation]) {
    rows.sort_unstable_by(|newer, older| {
        older
            .updated_at
            .cmp(&newer.updated_at)
            .then(older.id.cmp(&newer.id))
    });
}

/// The live `direct` statuses of thread `conversation_id`, newest first,
/// one [`PageParams`] page (cursors and cap on the status id).
///
/// What Pleroma's `PleromaAPI.ConversationController#statuses` fetches for
/// a participation's context: the thread's posts, tombstones out; whether
/// the reader may see each one is [`crate::models::status_visibility`]'s
/// question, asked by the endpoint that reads this. A thread with no live
/// status answers the empty `Vec`.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn page_direct_statuses(
    pool: &PgPool,
    conversation_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<Status>> {
    let mut builder = QueryBuilder::<Postgres>::new(THREAD_STATUSES_SQL);
    let _thread: &mut _ = builder
        .push_bind(conversation_id)
        .push(" AND s.visibility = 'direct' AND s.deleted_at IS NULL");
    keyset_page(pool, builder, "s.id", &page).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        models::status::{self, NewStatus, Visibility},
    };

    use crate::models::conversation::test_support::{
        bump, direct, direct_reply, linked_thread, local, page, slot,
    };

    #[tokio::test]
    async fn page_orders_newest_first_and_walks_the_id_keyset() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        for n in 0..3 {
            let post = direct(&pool, &alice, &[&bob], &format!("@bob {n}")).await;
            bump(&pool, &post).await;
        }

        let first_two = page_for_user(&pool, bob.id, page(2), &[])
            .await
            .expect("the first page reads");
        assert_eq!(first_two.len(), 2, "the cap held");
        assert!(
            first_two[0].id > first_two[1].id,
            "newest participation id first"
        );

        let older = page_for_user(
            &pool,
            bob.id,
            PageParams {
                max_id: first_two.last().map(|row| row.id),
                ..page(2)
            },
            &[],
        )
        .await
        .expect("the second page reads");
        assert_eq!(older.len(), 1, "one row walks out past the cursor");
        assert!(
            older.iter().all(|row| row.id < first_two[1].id),
            "and every one is older than the cursor"
        );

        let alice_page = page_for_user(&pool, alice.id, page(10), &[])
            .await
            .expect("alice's page reads");
        assert_eq!(
            alice_page.len(),
            3,
            "alice sees her own three, and nobody else's rows"
        );
        assert!(
            alice_page.iter().all(|row| row.user_id == alice.id),
            "every row is hers"
        );
    }

    #[tokio::test]
    async fn the_recipients_filter_keeps_only_threads_addressing_everyone() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let pair = direct(&pool, &alice, &[&bob, &carol], "@bob @carol psst").await;
        bump(&pool, &pair).await;
        let single = direct(&pool, &alice, &[&bob], "@bob alone").await;
        bump(&pool, &single).await;
        let pair_thread = linked_thread(&pool, &pair).await;

        let both = page_for_user(&pool, bob.id, page(10), &[carol.id])
            .await
            .expect("the filtered page reads");
        assert_eq!(
            both.iter()
                .map(|row| row.conversation_id)
                .collect::<Vec<_>>(),
            vec![pair_thread],
            "only the thread whose recipient list also names carol stands"
        );
        let everyone = page_for_user(&pool, bob.id, page(10), &[])
            .await
            .expect("the open page reads");
        assert_eq!(everyone.len(), 2, "nobody named keeps everything");
        let stranger = page_for_user(&pool, bob.id, page(10), &[dave.id])
            .await
            .expect("the stranger-naming page reads");
        assert!(
            stranger.is_empty(),
            "an account no thread addresses matches nothing"
        );
    }

    #[tokio::test]
    async fn page_direct_statuses_answers_the_thread_newest_first() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let first = direct(&pool, &alice, &[&bob], "@bob first").await;
        bump(&pool, &first).await;
        let second = direct_reply(&pool, &bob, &first, &[&alice], "@alice second").await;
        bump(&pool, &second).await;
        let thread = linked_thread(&pool, &first).await;
        let outside = direct(&pool, &alice, &[&bob], "@bob elsewhere").await;
        bump(&pool, &outside).await;
        let _public = status::insert(
            &pool,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Public,
                ..NewStatus::local(&alice, "not a dm")
            },
        )
        .await
        .expect("the public fixture inserts");

        let page_all = page_direct_statuses(&pool, thread, page(10))
            .await
            .expect("the thread reads");
        assert_eq!(
            page_all.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![second.id, first.id],
            "the thread's two direct statuses, newest first, other threads out"
        );

        let _gone = status::soft_delete(&pool, second.id)
            .await
            .expect("the newest fixture deletes");
        let after_delete = page_direct_statuses(&pool, thread, page(10))
            .await
            .expect("the page after the delete reads");
        assert_eq!(
            after_delete.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![first.id],
            "the tombstone leaves the listing"
        );
        let small = page_direct_statuses(
            &pool,
            thread,
            PageParams {
                limit: 1,
                ..page(1)
            },
        )
        .await
        .expect("the capped page reads");
        assert_eq!(
            small.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![first.id],
            "the cap held"
        );
        assert!(
            page_direct_statuses(&pool, Uuid::now_v7(), page(10))
                .await
                .expect("the unknown thread reads")
                .is_empty(),
            "a thread that never posted answers nothing — and the public \
             post is in no thread's page"
        );
    }
}
