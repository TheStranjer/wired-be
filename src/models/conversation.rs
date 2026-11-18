//! The `conversations` tables: one direct-message thread, each account's
//! side of it, and the accounts that side addresses.
//!
//! Reference: Pleroma's `Pleroma.Conversation`
//! (`lib/pleroma/conversation.ex`), `Pleroma.Conversation.Participation`
//! (`lib/pleroma/conversation/participation.ex`) and
//! `Pleroma.Conversation.Participation.RecipientShip`
//! (`lib/pleroma/conversation/participation/recipient_ship.ex`). Pleroma
//! keys a conversation by the AP `context` of the activities in it and
//! hands every recipient — local or remote — a participation row; here a
//! participation belongs to a **local** account alone and the remote
//! addressees ride the participation's recipient list instead, so the
//! thread's audience is stored without a remote account ever listing a
//! conversation it cannot read.
//!
//! [`participate`] is the write every consumer of this ticket hangs off:
//! the create service's `after_status_created` hook runs it (through
//! [`participate_for_status`]) for a `direct` status — statuses of any
//! other visibility do nothing. It finds-or-creates the thread's
//! `conversations` row — the `in_reply_to_conversation_id` create already
//! names it on the row, a reply inherits the parent's
//! [`direct_conversation_id`](Status::direct_conversation_id), a new root
//! keys a fresh one on its thread id — writes it back onto the status, and
//! for every local participant (author and local accounts mentioned by the
//! status) upserts the participation: its `last_status_id` and
//! `updated_at` move to this status, `read` becomes whether the
//! participant authored it (the recipients land unread), and its recipient
//! list is refreshed to every other participant, remote accounts included.
//!
//! The reads serve the surfaces the conversation endpoints build on:
//! [`get_participation`] one owned row, [`page_for_user`] the account's
//! list newest-activity-first (Pleroma's `recipients` restriction and all),
//! [`mark_read`] and [`mark_all_read`] the read flags,
//! [`delete_participation`] a withdrawal, [`unread_count`] the badge,
//! [`recipients`] the audience an `in_reply_to_conversation_id` post
//! addresses, [`recipients_map`] the same lists for a whole page in one
//! read, [`set_recipients`] the wholesale rewrite Pleroma's `PATCH`
//! performs, [`page_direct_statuses`] one thread's status listing, and
//! [`participation_ids`] the one batched read
//! [`crate::services::render_status`] runs per page to fill
//! `pleroma.direct_conversation_id`, and [`participations_for_status`] the
//! local roster one freshly threaded status is announced to, off the create
//! hook's [`crate::services::streaming::conversation_updated`] push.
//!
//! [`repoint_after_delete`] is the delete side of `last_status_id`: a soft
//! delete never fires the foreign key, so when the status one
//! participation last saw is tombstoned, the participation is repointed to
//! the conversation's newest remaining direct status — or deleted, with
//! its recipient rows cascading, when none remain.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        mention,
        status::{Status, Visibility},
    },
    pagination::PageParams,
    util::id::new_id,
};

/// One row of `conversation_participations`: one local account's side of
/// one direct-message thread.
///
/// Its `id` is what `pleroma.direct_conversation_id` renders and what
/// `POST /api/v1/statuses` takes back as `in_reply_to_conversation_id`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Participation {
    /// `UUIDv7` primary key, minted by [`participate`] (decision 4).
    pub id: Uuid,
    /// Whose side of the thread this is.
    pub user_id: Uuid,
    /// The thread's row in `conversations`.
    pub conversation_id: Uuid,
    /// Whether the account has seen [`last_status_id`](Self::last_status_id);
    /// the author's own post lands read, every other participant's lands
    /// unread until [`mark_read`] or [`mark_all_read`] says otherwise.
    pub read: bool,
    /// The conversation's newest direct status as of this row's last bump;
    /// moved on by [`participate`], re-pointed (or the row deleted) by
    /// [`repoint_after_delete`].
    pub last_status_id: Option<Uuid>,
    /// When the account joined the thread.
    pub inserted_at: DateTime<Utc>,
    /// When this row was last bumped by a new direct status.
    pub updated_at: DateTime<Utc>,
}

/// [`participate`]'s inheritance arm: the thread `status`'s parent stores,
/// read before the reply is linked. The column is nullable — a parent that
/// is no direct post has none — so the row answers `Option<Uuid>`, and a
/// parent that is not stored at all answers no row.
const PARENT_THREAD_SQL: &str = "SELECT direct_conversation_id FROM statuses WHERE id = $1";

/// [`participate`]'s create arm: one row per direct-message thread, keyed
/// by the instance's context id for it — the thread group's
/// [`conversation_id`](Status::conversation_id), which every status of one
/// thread shares. `ON CONFLICT (ap_context) DO UPDATE` restates the winner
/// instead of failing, so a thread is found-or-created by one statement
/// (Pleroma's `Conversation.create_for_ap_id` does the same).
const CREATE_THREAD_SQL: &str = "INSERT INTO conversations (id, ap_context)
     VALUES ($1, $2)
ON CONFLICT (ap_context) DO UPDATE SET ap_context = EXCLUDED.ap_context
 RETURNING id";

/// [`participate`]'s link: the thread id lands on the status, so the next
/// reply inherits it and the renderer's batch can key the viewer's
/// participation to the row.
const LINK_STATUS_SQL: &str = "UPDATE statuses
       SET direct_conversation_id = $2, updated_at = now()
     WHERE id = $1";

/// [`participate`]'s locality gate: which of the accounts a direct status
/// involves are this instance's own. Only these get a participation; the
/// rest still ride the recipient lists.
const LOCAL_IDS_SQL: &str = "SELECT id FROM users WHERE id = ANY($1) AND local";

/// [`participate`]'s bump: one participation per (account, thread) pair,
/// created at its first direct status and moved forward by every later one
/// — newest status, bumped time, and `read` set from who authored this one
/// (Pleroma's `Participation.create_for_user_and_conversation` upserts the
/// same pair). `RETURNING id` answers the row either way, which is what
/// the recipient rewrite needs.
const BUMP_PARTICIPATION_SQL: &str = "INSERT INTO conversation_participations (
     id, user_id, conversation_id, read, last_status_id
) VALUES (
     $1, $2, $3, $4, $5
) ON CONFLICT (user_id, conversation_id) DO UPDATE SET
     read = EXCLUDED.read,
     last_status_id = EXCLUDED.last_status_id,
     updated_at = now()
 RETURNING id";

/// [`participate`]'s recipient arm, first half: the list is refreshed, not
/// accumulated, so the old set goes before the new one lands.
const CLEAR_RECIPIENTS_SQL: &str = "DELETE FROM conversation_participation_recipients \
     WHERE participation_id = $1";

/// [`participate`]'s recipient arm, second half: every addressee of one
/// participation in one statement, one row per element of the bound list.
const INSERT_RECIPIENTS_SQL: &str = "INSERT INTO conversation_participation_recipients (
     participation_id, user_id
 ) SELECT $1, u.value FROM unnest($2::uuid[]) AS u(value)";

/// [`recipients`]: every account one participation addresses besides its
/// owner, in the pair key's order: the audience the status that last
/// bumped the participation was addressed to.
const RECIPIENTS_SQL: &str = "SELECT user_id FROM conversation_participation_recipients \
     WHERE participation_id = $1 ORDER BY user_id";

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

/// [`set_recipients`]' and [`recipients_map`]'s batched read: the whole
/// addressee roster of a page of participations in one statement, each row
/// one (participation, addressee) pair, ordered so one list per
/// participation comes back in the pair key's order.
const RECIPIENTS_BATCH_SQL: &str = "SELECT participation_id, user_id \
     FROM conversation_participation_recipients \
     WHERE participation_id = ANY($1) ORDER BY participation_id, user_id";

/// [`repoint_after_delete`]'s gate: does any participation still name the
/// deleted status, and of which thread are they — all of them, since a
/// participation's `last_status_id` only ever names a status of its own
/// conversation.
const REPOINTED_THREAD_SQL: &str = "SELECT conversation_id FROM conversation_participations \
     WHERE last_status_id = $1 LIMIT 1";

/// [`repoint_after_delete`]'s target: the conversation's newest live direct
/// status left (id descending is creation order, decision 4). The deleted
/// row is a tombstone by the time this runs, so it cannot name itself.
const NEWEST_REMAINING_SQL: &str = "SELECT id FROM statuses
     WHERE direct_conversation_id = $1
       AND visibility = 'direct'
       AND deleted_at IS NULL
     ORDER BY id DESC
     LIMIT 1";

/// [`repoint_after_delete`]'s write: every participation that named the
/// deleted status moves to the newest one left. `updated_at` stays: a
/// repoint restates the past, it is not a new bump.
const REPOINT_SQL: &str = "UPDATE conversation_participations
      SET last_status_id = $2
    WHERE last_status_id = $1";

/// [`repoint_after_delete`]'s other arm: the conversation ran out of live
/// direct statuses, so the participations that named its last one go — and
/// their recipient rows cascade with them.
const DROP_POINTING_SQL: &str = "DELETE FROM conversation_participations WHERE last_status_id = $1";

/// Records `status` in the direct-message thread it belongs to and bumps
/// every local participant's side of it.
///
/// A status of any visibility but [`Direct`] does nothing at all: only a
/// DM thread exists here. For a direct status the [module docs](self)
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

/// [`participate`]'s thread arm: the conversation `status` belongs to. The
/// row's own [`direct_conversation_id`](Status::direct_conversation_id)
/// wins (the `in_reply_to_conversation_id` create already placed it
/// there); else a reply inherits its parent's; else the thread is created
/// off the status's context id — and a reply whose parent stores no thread
/// (an answer to a non-`direct` post, or one to a parent this instance
/// never saw) starts its own, exactly as the ticket words it: a new root
/// creates one.
async fn thread_of(conn: &mut PgConnection, status: &Status) -> AppResult<Uuid> {
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

/// [`participate`]'s link arm: the thread lands on the status row unless
/// the row already carries exactly that id — then the write was the
/// create's own and nothing moves.
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
fn membership(status: &Status, participants: &[Uuid]) -> Vec<Uuid> {
    let mut everyone: Vec<Uuid> = Vec::with_capacity(participants.len() + 1);
    for id in std::iter::once(status.user_id).chain(participants.iter().copied()) {
        if !everyone.contains(&id) {
            everyone.push(id);
        }
    }
    everyone
}

/// [`participate`]'s locality gate: the roster's accounts this instance
/// owns, in roster order. A remote mentioned account is addressed (it
/// rides every recipient list) but never gets a participation of its own.
async fn local_members(conn: &mut PgConnection, everyone: &[Uuid]) -> AppResult<Vec<Uuid>> {
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

/// [`participate`]'s bump arm: upsert `member`'s participation of
/// `conversation_id` around `status` and answer its row id — the id the
/// recipient rewrite keys on. `read` is decided by who wrote this status:
/// the author's own post is read, everyone else's lands unread (Pleroma's
/// `create_or_bump_for` sets the same flag from the same question).
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

/// [`participate`]'s recipient arm: `participation_id`'s audience becomes
/// every member of the roster except its owner, remote included — deleted
/// and reinserted wholesale, so the set always matches the status that
/// bumped the participation last.
async fn store_recipients(
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

/// Every local side of the direct thread `status_id` was recorded into, each
/// as its own [`Participation`] row, ordered by its owning account.
///
/// The row's `user_id` is the reader to tell and its `id` is the participation
/// their socket renders — the id [`crate::streaming::hub::RenderSpec::Conversation`]
/// names, which means nothing to any other side of the thread. The thread is
/// found through the status's stored [`direct_conversation_id`](Status::direct_conversation_id),
/// so the answer is exactly the [`participate`] roster the create hook just
/// wrote: a status of any other visibility (threaded by nothing) and a thread
/// whose only members are remote (a participation is a local account's row)
/// both answer the empty `Vec`. [`participate_in_conversation`] is its only
/// caller, publishing one conversation update per row it returns.
///
/// [`participate_in_conversation`]: crate::services::create_status
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
pub async fn recipients(pool: &PgPool, participation_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_scalar::<_, Uuid>(RECIPIENTS_SQL)
        .bind(participation_id)
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
    page.apply_keyset(&mut builder, "p.id");
    let rows = builder
        .build_query_as::<Participation>()
        .fetch_all(pool)
        .await?;
    let mut rows = page.finish(rows);
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
    page.apply_keyset(&mut builder, "s.id");
    let rows = builder.build_query_as::<Status>().fetch_all(pool).await?;
    Ok(page.finish(rows))
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
    let mut lists: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (participation_id, user_id) in rows {
        lists.entry(participation_id).or_default().push(user_id);
    }
    Ok(lists)
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

/// Moves every participation that named the just-deleted status
/// `deleted_id` to its conversation's newest remaining direct status.
///
/// Or deletes it, when the conversation has no live direct status left
/// and its recipient rows cascade with the participation.
///
/// The soft delete of a status that is nobody's `last_status_id` (an older
/// post of the thread, a non-DM post, a reblog) is the no-op it should be:
/// no participation names it, and nothing else runs.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`] and the rollback
/// leaves the pointers exactly as the delete left them.
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn repoint_after_delete(pool: &PgPool, deleted_id: Uuid) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    repoint_pointers(&mut tx, deleted_id).await?;
    tx.commit().await?;
    Ok(())
}

/// [`repoint_after_delete`]'s work on the caller's transaction: gate on
/// some participation still naming the deleted status, then take the one
/// arm the conversation's state leaves — repoint every pointer onto the
/// newest status still live, or drop the pointers whose thread went dark.
async fn repoint_pointers(tx: &mut PgConnection, deleted_id: Uuid) -> AppResult<()> {
    let pointed = sqlx::query_as::<_, (Uuid,)>(REPOINTED_THREAD_SQL)
        .bind(deleted_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((conversation_id,)) = pointed else {
        return Ok(());
    };
    let remaining = sqlx::query_as::<_, (Uuid,)>(NEWEST_REMAINING_SQL)
        .bind(conversation_id)
        .fetch_optional(&mut *tx)
        .await?;
    match remaining {
        Some((newest_id,)) => {
            sqlx::query(REPOINT_SQL)
                .bind(deleted_id)
                .bind(newest_id)
                .execute(&mut *tx)
                .await
                .map(drop)?;
        }
        None => {
            sqlx::query(DROP_POINTING_SQL)
                .bind(deleted_id)
                .execute(&mut *tx)
                .await
                .map(drop)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{
        config::TEST_BASE_URL,
        models::{
            status::{self, NewStatus, Visibility},
            user::{self, NewUser, User},
        },
        test_db::TestDb,
    };

    use super::*;

    /// A migrated, emptied database slot.
    async fn slot() -> (TestDb, PgPool) {
        let db = crate::test_db::test_db()
            .await
            .expect("a migrated test database slot");
        let pool = db.pool().clone();
        (db, pool)
    }

    /// A fresh local account named `nickname`.
    async fn local(pool: &PgPool, nickname: &str) -> User {
        user::insert(
            pool,
            &NewUser::local(nickname, &format!("https://test.example/users/{nickname}")),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// A mirror of the remote actor `carol@remote.example`.
    async fn remote(pool: &PgPool, nickname: &str) -> User {
        user::insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id: format!("https://remote.example/users/{nickname}"),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// `author`'s `direct` status mentioning everyone in `mentioned`,
    /// through the model — the row the create's hook would then see.
    async fn direct(pool: &PgPool, author: &User, mentioned: &[&User], text: &str) -> Status {
        status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Direct,
                mentioned_user_ids: mentioned.iter().map(|who| who.id).collect(),
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("the direct fixture inserts")
    }

    /// A `direct` reply by `author` to `parent`, mentioning `mentioned`:
    /// the thread shape the create's reply arm stores.
    async fn direct_reply(
        pool: &PgPool,
        author: &User,
        parent: &Status,
        mentioned: &[&User],
        text: &str,
    ) -> Status {
        status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Direct,
                in_reply_to_id: Some(parent.id),
                in_reply_to_account_id: Some(parent.user_id),
                conversation_id: Some(parent.conversation_id),
                mentioned_user_ids: mentioned.iter().map(|who| who.id).collect(),
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("the direct reply fixture inserts")
    }

    /// Runs the create hook's whole write for `status`.
    async fn bump(pool: &PgPool, status: &Status) {
        participate_for_status(pool, status)
            .await
            .expect("participating for the fixture status runs");
    }

    /// The stored thread id of `status`, read back off the row.
    async fn linked_thread(pool: &PgPool, status: &Status) -> Uuid {
        let row = status::get_by_id(pool, status.id)
            .await
            .expect("the row reads")
            .expect("the fixture status is live");
        row.direct_conversation_id
            .expect("the fixture status got its thread")
    }

    /// The number of `conversations` rows.
    async fn thread_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversations")
            .fetch_one(pool)
            .await
            .expect("the count reads")
    }

    /// The stored participation of `owner` in `conversation_id`, for an
    /// assertion.
    async fn participation(
        pool: &PgPool,
        owner: Uuid,
        conversation_id: Uuid,
    ) -> Option<Participation> {
        sqlx::query_as::<_, Participation>(
            "SELECT * FROM conversation_participations \
             WHERE user_id = $1 AND conversation_id = $2",
        )
        .bind(owner)
        .bind(conversation_id)
        .fetch_optional(pool)
        .await
        .expect("the participation reads")
    }

    /// The stored recipient ids of one participation.
    async fn stored_recipients(pool: &PgPool, participation_id: Uuid) -> Vec<Uuid> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT user_id FROM conversation_participation_recipients \
              WHERE participation_id = $1 ORDER BY user_id",
        )
        .bind(participation_id)
        .fetch_all(pool)
        .await
        .expect("the recipients read")
    }

    /// A page asking for `limit` rows with no cursor.
    fn page(limit: i64) -> PageParams {
        PageParams {
            max_id: None,
            min_id: None,
            since_id: None,
            limit,
            offset: None,
        }
    }

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
    async fn deleting_the_newest_status_repoints_and_emptiness_removes() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let first = direct(&pool, &alice, &[&bob], "@bob psst").await;
        bump(&pool, &first).await;
        let second = direct_reply(&pool, &bob, &first, &[&alice], "@alice back").await;
        bump(&pool, &second).await;
        let thread = linked_thread(&pool, &first).await;

        let _gone = status::soft_delete(&pool, second.id)
            .await
            .expect("the newest fixture deletes");
        repoint_after_delete(&pool, second.id)
            .await
            .expect("the repoint runs");
        let alice_row = participation(&pool, alice.id, thread)
            .await
            .expect("alice still participates");
        assert_eq!(
            alice_row.last_status_id,
            Some(first.id),
            "her pointer moved to the newest status left"
        );

        let _older_gone = status::soft_delete(&pool, first.id)
            .await
            .expect("the older fixture deletes");
        repoint_after_delete(&pool, first.id)
            .await
            .expect("the second repoint runs");
        assert!(
            participation(&pool, alice.id, thread).await.is_none(),
            "with no live status left, alice's participation is gone"
        );
        assert!(
            participation(&pool, bob.id, thread).await.is_none(),
            "and bob's with it"
        );
    }

    #[tokio::test]
    async fn deleting_a_status_nobody_points_at_moves_nothing() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let first = direct(&pool, &alice, &[&bob], "@bob psst").await;
        bump(&pool, &first).await;
        let second = direct_reply(&pool, &alice, &first, &[&bob], "@bob again").await;
        bump(&pool, &second).await;
        let thread = linked_thread(&pool, &first).await;

        let _gone = status::soft_delete(&pool, first.id)
            .await
            .expect("the older status deletes");
        repoint_after_delete(&pool, first.id)
            .await
            .expect("the repoint runs");

        let alice_row = participation(&pool, alice.id, thread)
            .await
            .expect("the participation survived");
        assert_eq!(
            alice_row.last_status_id,
            Some(second.id),
            "the pointer never named the deleted row"
        );
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
    async fn the_migration_shapes_the_conversation_tables() {
        let (_db, pool) = slot().await;

        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns
              WHERE table_schema = 'public' AND table_name = 'conversation_participations'
           ORDER BY ordinal_position",
        )
        .fetch_all(&pool)
        .await
        .expect("the columns read");
        assert_eq!(
            columns,
            vec![
                "id".to_owned(),
                "user_id".to_owned(),
                "conversation_id".to_owned(),
                "read".to_owned(),
                "last_status_id".to_owned(),
                "inserted_at".to_owned(),
                "updated_at".to_owned(),
            ],
            "the migration's seven columns, in its order"
        );

        let status_column: Option<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns
              WHERE table_schema = 'public' AND table_name = 'statuses'
                AND column_name = 'direct_conversation_id'",
        )
        .fetch_optional(&pool)
        .await
        .expect("the column reads");
        assert_eq!(
            status_column.as_deref(),
            Some("direct_conversation_id"),
            "statuses grew the thread column"
        );

        let recipient_key: Vec<String> = sqlx::query_scalar(
            "SELECT att.attname
               FROM pg_constraint con
               JOIN pg_class cls ON cls.oid = con.conrelid
               JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
              WHERE con.contype = 'p' AND cls.relname = 'conversation_participation_recipients'
           ORDER BY con.conkey",
        )
        .fetch_all(&pool)
        .await
        .expect("the key reads");
        assert_eq!(
            recipient_key,
            vec!["participation_id".to_owned(), "user_id".to_owned(),],
            "the recipient table's pair primary key"
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
