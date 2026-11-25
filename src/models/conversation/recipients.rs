//! The `conversation_participation_recipients` rows: one participation's
//! addressee list — the SQL that rewrites it wholesale, and the reads that
//! answer it one row at a time or batched across a page.

use std::collections::HashMap;

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{error::AppResult, util::collections::group_pairs};

/// [`participate`]'s recipient arm, first half: the list is refreshed, not
/// accumulated, so the old set goes before the new one lands.
///
/// [`participate`]: super::participate
const CLEAR_RECIPIENTS_SQL: &str = "DELETE FROM conversation_participation_recipients \
     WHERE participation_id = $1";

/// [`participate`]'s recipient arm, second half: every addressee of one
/// participation in one statement, one row per element of the bound list.
///
/// [`participate`]: super::participate
const INSERT_RECIPIENTS_SQL: &str = "INSERT INTO conversation_participation_recipients (
     participation_id, user_id
 ) SELECT $1, u.value FROM unnest($2::uuid[]) AS u(value)";

/// [`recipients`]: every account one participation addresses besides its
/// owner, in the pair key's order: the audience the status that last
/// bumped the participation was addressed to.
const RECIPIENTS_SQL: &str = "SELECT user_id FROM conversation_participation_recipients \
     WHERE participation_id = $1 ORDER BY user_id";

/// [`set_recipients`]' and [`recipients_map`]'s batched read: the whole
/// addressee roster of a page of participations in one statement, each row
/// one (participation, addressee) pair, ordered so one list per
/// participation comes back in the pair key's order.
const RECIPIENTS_BATCH_SQL: &str = "SELECT participation_id, user_id \
     FROM conversation_participation_recipients \
     WHERE participation_id = ANY($1) ORDER BY participation_id, user_id";

/// [`thread_members`]: every account that authored or is @-mentioned by
/// a status of one thread, each named once.
const THREAD_MEMBERS_SQL: &str = "SELECT s.user_id FROM statuses s \
     WHERE s.direct_conversation_id = $1 \
     UNION \
     SELECT sm.user_id FROM status_mentions sm \
       JOIN statuses s ON s.id = sm.status_id \
     WHERE s.direct_conversation_id = $1";

/// [`participate`]'s recipient arm: `participation_id`'s audience becomes
/// every member of the roster except its owner, remote included — deleted
/// and reinserted wholesale, so the set always matches the status that
/// bumped the participation last.
///
/// [`participate`]: super::participate
pub(super) async fn store_recipients(
    conn: &mut PgConnection,
    participation_id: Uuid,
    owner: Uuid,
    everyone: &[Uuid],
) -> AppResult<()> {
    sqlx::query(CLEAR_RECIPIENTS_SQL)
        .bind(participation_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    let others: Vec<Uuid> = everyone.iter().copied().filter(|id| *id != owner).collect();
    if others.is_empty() {
        return Ok(());
    }
    sqlx::query(INSERT_RECIPIENTS_SQL)
        .bind(participation_id)
        .bind(&others)
        .execute(conn)
        .await
        .map(drop)?;
    Ok(())
}

/// The accounts `participation_id` addresses besides its owner, in the
/// pair key's order.
///
/// These are the mentions an `in_reply_to_conversation_id` post carries,
/// and the audience [`participate`] last stored. An unknown participation
/// and one with nobody on its list both answer the empty `Vec`.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
/// [`participate`]: super::participate
pub async fn recipients(pool: &PgPool, participation_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_scalar::<_, Uuid>(RECIPIENTS_SQL)
        .bind(participation_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The addressee lists of `participation_ids`, one per participation.
///
/// Each list is in the pair key's order — [`recipients`] for a whole page
/// of participations in one statement (decision 3's batched reads). A
/// participation with an empty list — and one the page never names — is
/// absent from the map.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn recipients_map(
    pool: &PgPool,
    participation_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<Uuid>>> {
    if participation_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (Uuid, Uuid)>(RECIPIENTS_BATCH_SQL)
        .bind(participation_ids)
        .fetch_all(pool)
        .await?;
    Ok(group_pairs(rows))
}

/// The accounts that took part in thread `conversation_id`.
///
/// Every author and every @-mentioned account of the thread's statuses,
/// each once, in id order. The `PATCH` rewrite may only name these.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn thread_members(pool: &PgPool, conversation_id: Uuid) -> AppResult<Vec<Uuid>> {
    let mut members = sqlx::query_scalar::<_, Uuid>(THREAD_MEMBERS_SQL)
        .bind(conversation_id)
        .fetch_all(pool)
        .await?;
    members.sort_unstable();
    Ok(members)
}

/// Makes `user_ids` the whole addressee list of `participation_id`.
///
/// One transaction: the old set goes before the new one lands, exactly the
/// wholesale rewrite [`participate`] runs — with the caller's list instead
/// of the status's roster (Pleroma's `Participation.set_recipients`, which
/// the `PATCH /api/v1/pleroma/conversations/{id}` endpoint drives; whether
/// every named account exists is that endpoint's check, since Pleroma
/// refuses an unknown recipient and the ticket owes it a 404). An empty
/// slice clears the list.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`] and the rollback
/// leaves the old list in place.
///
/// [`AppError::Database`]: crate::error::AppError
/// [`participate`]: super::participate
pub async fn set_recipients(
    pool: &PgPool,
    participation_id: Uuid,
    user_ids: &[Uuid],
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(CLEAR_RECIPIENTS_SQL)
        .bind(participation_id)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    if !user_ids.is_empty() {
        let _stored = sqlx::query(INSERT_RECIPIENTS_SQL)
            .bind(participation_id)
            .bind(user_ids)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::conversation::{
        Participation, delete_participation,
        test_support::{
            bump, direct, direct_reply, linked_thread, local, participation, remote, slot,
            stored_recipients,
        },
    };

    #[tokio::test]
    async fn recipients_map_answers_the_page_in_one_read() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        let post = direct(&pool, &alice, &[&bob, &carol], "@bob @carol psst").await;
        bump(&pool, &post).await;
        let thread = linked_thread(&pool, &post).await;
        let alice_row = participation(&pool, alice.id, thread)
            .await
            .expect("alice participates");
        let bob_row = participation(&pool, bob.id, thread)
            .await
            .expect("bob participates");
        let lonely = Participation {
            id: Uuid::now_v7(),
            ..alice_row.clone()
        };

        let asked = [alice_row.id, bob_row.id, lonely.id];
        let found = recipients_map(&pool, &asked)
            .await
            .expect("the batch reads");
        assert_eq!(
            found.get(&alice_row.id),
            Some(&vec![bob.id, carol.id]),
            "alice's list, in the pair key's order"
        );
        assert_eq!(
            found.get(&bob_row.id),
            Some(&vec![alice.id, carol.id]),
            "and bob's, the batch answering both from one read"
        );
        assert!(
            !found.contains_key(&lonely.id),
            "a participation with no rows is absent"
        );
        assert!(
            recipients_map(&pool, &[])
                .await
                .expect("the empty ask runs")
                .is_empty(),
            "an empty ask is the empty map"
        );
    }

    #[tokio::test]
    async fn thread_members_are_the_authors_and_mentioned_accounts() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let post = direct(&pool, &alice, &[&bob], "@bob psst").await;
        bump(&pool, &post).await;
        let thread = linked_thread(&pool, &post).await;
        let other = direct(&pool, &dave, &[&carol], "@carol elsewhere").await;
        bump(&pool, &other).await;
        let other_thread = linked_thread(&pool, &other).await;

        let mut expected = vec![alice.id, bob.id];
        expected.sort_unstable();
        assert_eq!(
            thread_members(&pool, thread).await.expect("the read runs"),
            expected,
            "author and mention, each once, and nobody from another thread"
        );
        let reply = direct_reply(&pool, &bob, &post, &[&alice], "@alice back").await;
        bump(&pool, &reply).await;
        assert_eq!(
            thread_members(&pool, thread).await.expect("the read runs"),
            expected,
            "a reply by the same people adds nobody"
        );
        let mut others = vec![dave.id, carol.id];
        others.sort_unstable();
        assert_eq!(
            thread_members(&pool, other_thread)
                .await
                .expect("the read runs"),
            others,
            "the other thread has its own members, a remote mention included"
        );
        assert!(
            thread_members(&pool, Uuid::now_v7())
                .await
                .expect("the read runs")
                .is_empty(),
            "an unknown thread has no members"
        );
    }

    #[tokio::test]
    async fn set_recipients_rewrites_the_whole_list() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let post = direct(&pool, &alice, &[&bob], "@bob psst").await;
        bump(&pool, &post).await;
        let thread = linked_thread(&pool, &post).await;
        let alice_row = participation(&pool, alice.id, thread)
            .await
            .expect("alice participates");

        set_recipients(&pool, alice_row.id, &[bob.id, carol.id])
            .await
            .expect("the rewrite runs");
        assert_eq!(
            stored_recipients(&pool, alice_row.id).await,
            vec![bob.id, carol.id],
            "the new list is exactly what was asked"
        );
        set_recipients(&pool, alice_row.id, &[carol.id])
            .await
            .expect("the second rewrite runs");
        assert_eq!(
            stored_recipients(&pool, alice_row.id).await,
            vec![carol.id],
            "and it replaced the first, not accumulated onto it"
        );
        set_recipients(&pool, alice_row.id, &[])
            .await
            .expect("the clearing runs");
        assert!(
            stored_recipients(&pool, alice_row.id).await.is_empty(),
            "an empty list clears the roster"
        );
    }

    #[tokio::test]
    async fn withdrawing_a_participation_takes_its_recipient_list() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let post = direct(&pool, &alice, &[&bob], "@bob psst").await;
        bump(&pool, &post).await;
        let thread = linked_thread(&pool, &post).await;
        let alice_row = participation(&pool, alice.id, thread)
            .await
            .expect("alice participates");

        let foreign = delete_participation(&pool, bob.id, alice_row.id)
            .await
            .expect("the foreign withdrawal runs");
        assert!(!foreign, "bob cannot withdraw alice's row");
        assert!(
            delete_participation(&pool, alice.id, alice_row.id)
                .await
                .expect("alice's own withdrawal runs"),
            "hers goes"
        );
        assert!(
            !delete_participation(&pool, alice.id, alice_row.id)
                .await
                .expect("the repeat withdrawal runs"),
            "and a repeat finds nothing"
        );
        assert!(
            stored_recipients(&pool, alice_row.id).await.is_empty(),
            "its recipient list cascaded with it"
        );
    }
}
