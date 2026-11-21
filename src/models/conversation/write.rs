//! The write path of the ticket: [`participate`] — the one record-a-thread
//! entry every consumer hangs off, its roster folded out of the status and
//! the accounts the caller names — and [`participate_for_status`], the
//! create hook's wrapper that reads the roster itself and runs the whole
//! bump in one transaction.
//!
//! The arms it drives live with the rows they name: the thread
//! find-or-create and the locality gate in [`threads`](super::threads), the
//! recipient rewrite in [`recipients`](super::recipients), and the reads
//! and later writes of the rows it bumps in
//! [`participation`](super::participation).
//!
//! [`participate`]: super::participate

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{
    recipients::store_recipients,
    threads::{local_members, thread_of},
};
use crate::{
    error::AppResult,
    models::{
        mention,
        status::{Status, Visibility},
    },
    util::id::new_id,
};

/// [`participate`]'s link: the thread id lands on the status, so the next
/// reply inherits it and the renderer's batch can key the viewer's
/// participation to the row.
///
/// [`participate`]: super::participate
const LINK_STATUS_SQL: &str = "UPDATE statuses
       SET direct_conversation_id = $2, updated_at = now()
     WHERE id = $1";

/// [`participate`]'s bump: one participation per (account, thread) pair,
/// created at its first direct status and moved forward by every later one
/// — newest status, bumped time, and `read` set from who authored this one
/// (Pleroma's `Participation.create_for_user_and_conversation` upserts the
/// same pair). `RETURNING id` answers the row either way, which is what
/// the recipient rewrite needs.
///
/// [`participate`]: super::participate
const BUMP_PARTICIPATION_SQL: &str = "INSERT INTO conversation_participations (
     id, user_id, conversation_id, read, last_status_id
) VALUES (
     $1, $2, $3, $4, $5
) ON CONFLICT (user_id, conversation_id) DO UPDATE SET
     read = EXCLUDED.read,
     last_status_id = EXCLUDED.last_status_id,
     updated_at = now()
  RETURNING id";

/// Records `status` in the direct-message thread it belongs to and bumps
/// every local participant's side of it.
///
/// A status of any visibility but [`Direct`] does nothing at all: only a
/// DM thread exists here. For a direct status the [module docs]
/// state the whole write: the thread is found-or-created, the status is
/// linked to it, and each local participant's participation is upserted
/// (`last_status_id` this status, `updated_at` now, `read` true exactly
/// for the author) with its recipient list refreshed to every other
/// participant — remote accounts included, since the list stores
/// addressees, not readers.
///
/// `participants` are the accounts the status involves: every mentioned
/// user, local and remote (the author joins the roster through this
/// function even when the slice leaves them out, and repeats fold).
///
/// [`Direct`]: Visibility::Direct
/// [module docs]: crate::models::conversation
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn participate(
    conn: &mut PgConnection,
    status: &Status,
    participants: &[Uuid],
) -> AppResult<()> {
    if status.visibility != Visibility::Direct && status.direct_conversation_id.is_none() {
        return Ok(());
    }
    let conversation_id = thread_of(&mut *conn, status).await?;
    link_status(&mut *conn, status, conversation_id).await?;
    let everyone = membership(status, participants);
    for member in local_members(&mut *conn, &everyone).await? {
        let participation_id =
            bump_participation(&mut *conn, status, conversation_id, member).await?;
        store_recipients(&mut *conn, participation_id, member, &everyone).await?;
    }
    Ok(())
}

/// [`participate`]'s link arm: the thread lands on the status row unless
/// the row already carries exactly that id — then the write was the
/// create's own and nothing moves.
///
/// [`participate`]: super::participate
async fn link_status(
    conn: &mut PgConnection,
    status: &Status,
    conversation_id: Uuid,
) -> AppResult<()> {
    if status.direct_conversation_id == Some(conversation_id) {
        return Ok(());
    }
    sqlx::query(LINK_STATUS_SQL)
        .bind(status.id)
        .bind(conversation_id)
        .execute(conn)
        .await
        .map(drop)?;
    Ok(())
}

/// [`participate`]'s roster: the author first, then every participant the
/// caller named that is not already on the list.
///
/// [`participate`]: super::participate
fn membership(status: &Status, participants: &[Uuid]) -> Vec<Uuid> {
    let mut everyone: Vec<Uuid> = Vec::with_capacity(participants.len() + 1);
    for id in std::iter::once(status.user_id).chain(participants.iter().copied()) {
        if !everyone.contains(&id) {
            everyone.push(id);
        }
    }
    everyone
}

/// [`participate`]'s bump arm: upsert `member`'s participation of
/// `conversation_id` around `status` and answer its row id — the id the
/// recipient rewrite keys on. `read` is decided by who wrote this status:
/// the author's own post is read, everyone else's lands unread (Pleroma's
/// `create_or_bump_for` sets the same flag from the same question).
///
/// [`participate`]: super::participate
async fn bump_participation(
    conn: &mut PgConnection,
    status: &Status,
    conversation_id: Uuid,
    member: Uuid,
) -> AppResult<Uuid> {
    let row = sqlx::query_as::<_, (Uuid,)>(BUMP_PARTICIPATION_SQL)
        .bind(new_id())
        .bind(member)
        .bind(conversation_id)
        .bind(member == status.user_id)
        .bind(status.id)
        .fetch_one(&mut *conn)
        .await?;
    Ok(row.0)
}

/// [`participate`] for the create hook, reading the roster itself.
///
/// The status's author beside every account its stored `status_mentions`
/// links name, local and remote, handed to one transaction that commits
/// once the whole bump has landed.
///
/// A status of any visibility but `direct` touches no thread and costs no
/// read: that is the hook's "statuses with other visibilities do nothing".
///
/// # Errors
///
/// Those of [`participate`], plus any database failure behind the mention
/// read, arriving as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn participate_for_status(pool: &PgPool, status: &Status) -> AppResult<()> {
    if status.visibility != Visibility::Direct {
        return Ok(());
    }
    let mut everyone: Vec<Uuid> = vec![status.user_id];
    let mentioned = mention::for_statuses(pool, std::slice::from_ref(&status.id)).await?;
    for account in mentioned.get(&status.id).map_or(&[][..], Vec::as_slice) {
        if !everyone.contains(&account.id) {
            everyone.push(account.id);
        }
    }
    let mut tx = pool.begin().await?;
    participate(&mut tx, status, &everyone).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        models::status::{self, NewStatus},
    };

    use crate::models::conversation::{
        Participation, mark_all_read, mark_read,
        test_support::{
            bump, direct, direct_reply, linked_thread, local, participation, slot, thread_count,
        },
        unread_count,
    };

    #[tokio::test]
    async fn a_reply_continues_the_thread_it_answers() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let first = direct(&pool, &alice, &[&bob], "@bob psst").await;
        bump(&pool, &first).await;
        let thread = linked_thread(&pool, &first).await;

        let second = direct_reply(&pool, &bob, &first, &[&alice], "@alice back").await;
        bump(&pool, &second).await;

        assert_eq!(
            linked_thread(&pool, &second).await,
            thread,
            "the reply inherits the parent's thread"
        );
        assert_eq!(thread_count(&pool).await, 1, "and opens no second one");
        let bob_row = participation(&pool, bob.id, thread)
            .await
            .expect("bob participates");
        assert!(bob_row.read, "the replier reads their own answer");
        assert_eq!(bob_row.last_status_id, Some(second.id));
        let alice_row = participation(&pool, alice.id, thread)
            .await
            .expect("alice still participates");
        assert!(!alice_row.read, "and alice's side lands unread again");
        assert_eq!(
            alice_row.last_status_id,
            Some(second.id),
            "her pointer moved to the newest status too"
        );
    }

    #[tokio::test]
    async fn anything_but_direct_records_no_thread() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let post = status::insert(
            &pool,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Public,
                mentioned_user_ids: vec![bob.id],
                ..NewStatus::local(&alice, "@bob hello")
            },
        )
        .await
        .expect("the public fixture inserts");

        bump(&pool, &post).await;

        assert_eq!(thread_count(&pool).await, 0, "no thread row");
        let rows = sqlx::query("SELECT 1 FROM conversation_participations")
            .fetch_optional(&pool)
            .await
            .expect("the table reads");
        assert!(rows.is_none(), "and no participations");
    }

    #[tokio::test]
    async fn bumping_the_same_status_twice_restates_one_thread() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let post = direct(&pool, &alice, &[&bob], "@bob psst").await;

        bump(&pool, &post).await;
        let row = status::get_by_id(&pool, post.id)
            .await
            .expect("the row reads")
            .expect("the fixture is live");
        bump(&pool, &row).await;

        assert_eq!(thread_count(&pool).await, 1, "still one thread row");
        let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversation_participations")
            .fetch_one(&pool)
            .await
            .expect("the count reads");
        assert_eq!(rows, 2, "and still one participation each");
    }

    #[tokio::test]
    async fn read_flags_move_one_row_then_every_unread_one() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        for n in 0..3 {
            let post = direct(&pool, &alice, &[&bob], &format!("@bob {n}")).await;
            bump(&pool, &post).await;
        }
        assert_eq!(
            unread_count(&pool, bob.id).await.unwrap(),
            3,
            "three threads all unread"
        );
        let bobs_rows: Vec<Participation> = sqlx::query_as::<_, Participation>(
            "SELECT * FROM conversation_participations WHERE user_id = $1 ORDER BY id",
        )
        .bind(bob.id)
        .fetch_all(&pool)
        .await
        .expect("bob's rows read");
        let oldest = bobs_rows.first().expect("bob has three rows").clone();

        let flipped = mark_read(&pool, bob.id, oldest.id)
            .await
            .expect("the mark runs");
        assert_eq!(
            flipped.map(|row| (row.id, row.read)),
            Some((oldest.id, true)),
            "the row answers read"
        );
        assert_eq!(
            mark_read(&pool, alice.id, oldest.id)
                .await
                .expect("the foreign mark runs"),
            None,
            "somebody else's participation is no row to mark"
        );
        assert_eq!(unread_count(&pool, bob.id).await.unwrap(), 2);

        let all = mark_all_read(&pool, bob.id).await.expect("the sweep runs");
        let mut flipped_ids: Vec<Uuid> = all.iter().map(|row| row.id).collect();
        flipped_ids.sort_unstable();
        let mut expected_ids: Vec<Uuid> = bobs_rows.iter().skip(1).map(|row| row.id).collect();
        expected_ids.sort_unstable();
        assert_eq!(
            flipped_ids, expected_ids,
            "exactly the two unread rows flipped"
        );
        assert_eq!(unread_count(&pool, bob.id).await.unwrap(), 0);
        assert!(
            mark_all_read(&pool, alice.id)
                .await
                .expect("the author's sweep runs")
                .is_empty(),
            "whose own posts left nothing unread"
        );
    }
}
