//! The thread behind a direct status: the SQL that finds or opens its
//! `conversations` row, read from the status's own link and its parent's,
//! and the locality gate that decides which accounts of a direct status's
//! roster get a participation of their own — [`participate`]'s thread and
//! locality arms.
//!
//! [`participate`]: super::participate

use std::collections::HashSet;

use sqlx::PgConnection;
use uuid::Uuid;

use crate::{error::AppResult, models::status::Status, util::id::new_id};

/// [`participate`]'s inheritance arm: the thread `status`'s parent stores,
/// read before the reply is linked. The column is nullable — a parent that
/// is no direct post has none — so the row answers `Option<Uuid>`, and a
/// parent that is not stored at all answers no row.
///
/// [`participate`]: super::participate
const PARENT_THREAD_SQL: &str = "SELECT direct_conversation_id FROM statuses WHERE id = $1";

/// [`participate`]'s create arm: one row per direct-message thread, keyed
/// by the instance's context id for it — the thread group's
/// [`conversation_id`](Status::conversation_id), which every status of one
/// thread shares. `ON CONFLICT (ap_context) DO UPDATE` restates the winner
/// instead of failing, so a thread is found-or-created by one statement
/// (Pleroma's `Conversation.create_for_ap_id` does the same).
///
/// [`participate`]: super::participate
const CREATE_THREAD_SQL: &str = "INSERT INTO conversations (id, ap_context)
     VALUES ($1, $2)
ON CONFLICT (ap_context) DO UPDATE SET ap_context = EXCLUDED.ap_context
  RETURNING id";

/// [`participate`]'s locality gate: which of the accounts a direct status
/// involves are this instance's own. Only these get a participation; the
/// rest still ride the recipient lists.
///
/// [`participate`]: super::participate
const LOCAL_IDS_SQL: &str = "SELECT id FROM users WHERE id = ANY($1) AND local";

/// [`participate`]'s thread arm: the conversation `status` belongs to. The
/// row's own [`direct_conversation_id`](Status::direct_conversation_id)
/// wins (the `in_reply_to_conversation_id` create already placed it
/// there); else a reply inherits its parent's; else the thread is created
/// off the status's context id — and a reply whose parent stores no thread
/// (an answer to a non-`direct` post, or one to a parent this instance
/// never saw) starts its own, exactly as the ticket words it: a new root
/// creates one.
pub(super) async fn thread_of(conn: &mut PgConnection, status: &Status) -> AppResult<Uuid> {
    if let Some(existing) = status.direct_conversation_id {
        return Ok(existing);
    }
    if let Some(parent_id) = status.in_reply_to_id
        && let Some(inherited) = sqlx::query_as::<_, (Option<Uuid>,)>(PARENT_THREAD_SQL)
            .bind(parent_id)
            .fetch_optional(&mut *conn)
            .await?
        && let Some(conversation_id) = inherited.0
    {
        return Ok(conversation_id);
    }
    let row = sqlx::query_as::<_, (Uuid,)>(CREATE_THREAD_SQL)
        .bind(new_id())
        .bind(status.conversation_id.to_string())
        .fetch_one(&mut *conn)
        .await?;
    Ok(row.0)
}

/// [`participate`]'s locality gate: the roster's accounts this instance
/// owns, in roster order. A remote mentioned account is addressed (it
/// rides every recipient list) but never gets a participation of its own.
///
/// [`participate`]: super::participate
pub(super) async fn local_members(
    conn: &mut PgConnection,
    everyone: &[Uuid],
) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_scalar::<_, Uuid>(LOCAL_IDS_SQL)
        .bind(everyone)
        .fetch_all(&mut *conn)
        .await?;
    let local: HashSet<Uuid> = rows.into_iter().collect();
    Ok(everyone
        .iter()
        .copied()
        .filter(|id| local.contains(id))
        .collect())
}

#[cfg(test)]
mod tests {
    use crate::{
        config::TEST_BASE_URL,
        models::status::{self, NewStatus, Visibility},
    };

    use crate::models::conversation::{
        page_for_user,
        test_support::{
            bump, direct, direct_reply, linked_thread, local, page, participation, remote, slot,
            stored_recipients, thread_count,
        },
    };

    #[tokio::test]
    async fn a_new_direct_thread_lands_author_read_recipient_unread() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let post = direct(&pool, &alice, &[&bob], "@bob psst").await;

        bump(&pool, &post).await;
        let thread = linked_thread(&pool, &post).await;

        let mine = participation(&pool, alice.id, thread)
            .await
            .expect("the author participates");
        assert!(mine.read, "the author's own post is read");
        assert_eq!(mine.last_status_id, Some(post.id));
        let yours = participation(&pool, bob.id, thread)
            .await
            .expect("the mentioned local account participates");
        assert!(!yours.read, "the recipient lands unread");
        assert_eq!(yours.last_status_id, Some(post.id));
        assert_eq!(
            stored_recipients(&pool, mine.id).await,
            vec![bob.id],
            "the author's participation addresses bob"
        );
        assert_eq!(
            stored_recipients(&pool, yours.id).await,
            vec![alice.id],
            "and bob's addresses the author back"
        );
        assert_eq!(thread_count(&pool).await, 1, "one row for the thread");
    }

    #[tokio::test]
    async fn remote_mentions_are_recipients_without_participations() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        let post = direct(&pool, &alice, &[&bob, &carol], "@bob @carol psst").await;

        bump(&pool, &post).await;
        let thread = linked_thread(&pool, &post).await;

        assert!(
            participation(&pool, carol.id, thread).await.is_none(),
            "the remote account gets no participation of its own"
        );
        let alice_row = participation(&pool, alice.id, thread)
            .await
            .expect("alice participates");
        let mut expected = vec![bob.id, carol.id];
        expected.sort_unstable();
        assert_eq!(
            stored_recipients(&pool, alice_row.id).await,
            expected,
            "both addressees ride the author's recipient list, remote included"
        );
        let bob_row = participation(&pool, bob.id, thread)
            .await
            .expect("bob participates");
        let mut bobs_expected = vec![alice.id, carol.id];
        bobs_expected.sort_unstable();
        assert_eq!(
            stored_recipients(&pool, bob_row.id).await,
            bobs_expected,
            "and bob's list carries the author beside the remote account"
        );
    }

    #[tokio::test]
    async fn a_status_placed_into_a_named_thread_joins_it() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let first = direct(&pool, &alice, &[&bob], "@bob psst").await;
        bump(&pool, &first).await;
        let thread = linked_thread(&pool, &first).await;

        // The `in_reply_to_conversation_id` shape: a fresh row whose
        // `direct_conversation_id` already names the thread it answers into.
        let joined = status::insert(
            &pool,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Direct,
                direct_conversation_id: Some(thread),
                mentioned_user_ids: vec![bob.id],
                ..NewStatus::local(&alice, "another one")
            },
        )
        .await
        .expect("the linked fixture inserts");
        bump(&pool, &joined).await;

        assert_eq!(
            linked_thread(&pool, &joined).await,
            thread,
            "the named thread stands, no new row is opened"
        );
        assert_eq!(thread_count(&pool).await, 1);
        let bob_row = participation(&pool, bob.id, thread)
            .await
            .expect("bob participates");
        assert_eq!(
            bob_row.last_status_id,
            Some(joined.id),
            "and his pointer moved to the status posted into his thread"
        );
    }

    #[tokio::test]
    async fn the_page_moves_the_thread_that_saw_activity_last_to_the_front() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let first = direct(&pool, &alice, &[&bob], "@bob first").await;
        bump(&pool, &first).await;
        let second = direct(&pool, &alice, &[&bob], "@bob second").await;
        bump(&pool, &second).await;
        let first_thread = linked_thread(&pool, &first).await;

        // A third thread, then a reply into the first: its participation is
        // the freshest activity of the three, though the oldest row.
        let third = direct(&pool, &alice, &[&bob], "@bob third").await;
        bump(&pool, &third).await;
        let echoed = direct_reply(&pool, &bob, &first, &[&alice], "@alice back").await;
        bump(&pool, &echoed).await;

        let listed = page_for_user(&pool, alice.id, page(10), &[])
            .await
            .expect("the listing reads");
        assert_eq!(
            listed.first().expect("three threads").conversation_id,
            first_thread,
            "the thread the reply bumped leads the newest-activity-first list"
        );
    }
}
