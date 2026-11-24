//! Remote-status ingestion: folding one received `Note`-family object into
//! a stored status.
//!
//! [`ingest_note`] is the database half of a federated `Create`: given the
//! already-parsed author's object document and the [`User`] mirror it was
//! delivered by, it decides whether this instance gains a post, and writes
//! everything that post owns — the `statuses` row with its reply link and
//! context, its `status_mentions` and hashtag links, its
//! `media_attachments`, its `polls`/`poll_options` rows and the
//! `custom_emojis` its `Emoji` tags announce — in one transaction, so a
//! half-folded remote post never stands visible. The reference is
//! Pleroma's `Pleroma.Web.ActivityPub.Transmogrifier.handle_incoming`
//! `Create` clause feeding `SideEffects.handle_incoming`
//! (`lib/pleroma/web/activity_pub/transmogrifier.ex`,
//! `lib/pleroma/web/activity_pub/side_effects.ex`); the ticket rules here
//! are authoritative where the two differ.
//!
//! The [`crate::federation::note_parser`] did the document half already:
//! [`parse_note`](crate::federation::note_parser::parse_note) answers a
//! [`ParsedNote`] of insert-ready parts. This
//! module resolves what only a live database (and, for unknown mention
//! actors, the network) can settle, in the order the ticket fixes:
//!
//! 1. parse; a malformed or refusing document is a
//!    [`FederationError::Permanent`] the dispatcher discards the job for;
//! 2. the `uri` dedupe: a status with the same object id is nobody's news
//!    twice — a live one answers [`IngestOutcome::Existing`], a tombstone
//!    [`IngestOutcome::Ignored`] with `"tombstoned"` (a deleted remote post
//!    is never re-imported, the reason its `uri` stays reserved);
//! 3. a deactivated author [`IngestOutcome::Ignored`]s the post: the
//!    mirror was retired, its new content is not admitted;
//! 4. the reply link: a stored parent (by `uri`) supplies
//!    `in_reply_to_id`, `in_reply_to_account_id` and its `conversation_id`;
//!    an unstored one keeps the `in_reply_to_ap_id` reference and, when a
//!    stored status shares the object's `context`, that status's
//!    conversation — a thread joins itself together before its root ever
//!    arrives; with neither, [`status::insert`] makes the new row its own
//!    conversation. The quote `uri` resolves locally only: a stored, live
//!    original becomes `quote_of_id`, anything else leaves the quote
//!    unfilled (this ticket does not fetch objects);
//! 5. mentions: each `Mention` tag `href` maps to a stored user by
//!    `ap_id`; an unknown one is fetched through
//!    [`get_or_fetch_by_ap_id`] under `MENTION_LOOKUP_TIMEOUT`, at most
//!    `MAX_MENTION_LOOKUPS` of them per status, and whatever stays
//!    unresolved — a dead peer, an elapsed timeout, a mention past the
//!    cap — is dropped rather than refused. The local accounts named in
//!    `to`/`cc` join them as silent mentions (stored rows only, never
//!    fetched), so an untagged direct post still reaches its recipient;
//! 6. remote emojis upsert `custom_emojis` under the author's domain —
//!    only once the unsolicited gate has let the post through, and only
//!    the image URL of an emoji already stored, so a moderator's flags
//!    survive;
//! 7. attachments become `media_attachments` rows with no `user_id` (nobody
//!    uploaded them) and `remote_url` beside `url` (the media proxy decides
//!    what to mirror), ordered as the document lists them, written on the
//!    status's own transaction; a `Question`'s poll likewise lands its
//!    options and the counts the remote reported;
//! 8. the status itself is inserted with `local = false` and every counter
//!    at zero — Mastodon-family reblog and favourite counts of remote
//!    statuses are learned only from the `Announce`es and `Like`s that
//!    arrive later, never from what the object's own `likes`/`shares`
//!    happened to say when it was fetched;
//! 9. after the commit, the create path's `after_status_created` hook
//!    runs over the row exactly as it does for a local post — mention and
//!    subscriber notifications, the link-card job, DM-thread participation
//!    — every one of which decides on the *recipient's* locality and none
//!    on the author's;
//! 10. [`Config::accept_unsolicited_public`](crate::config::Config::accept_unsolicited_public)
//!     off drops `public` and
//!     `unlisted` statuses from authors no local account follows, unless a
//!     local user is mentioned or the status answers a local one — the
//!     explicit anti-spam gate, evaluated after mentions and the reply
//!     link are known and before anything is written;
//! 11. a created row that carries an unresolved `in_reply_to_ap_id` gets a
//!     [`federator.backfill`] job on the `background` queue (deduped per
//!     status id), so the thread closes on its own once the parent is
//!     fetchable — see [`crate::federation::fetch_object`].
//!
//! [`federator.backfill`]: crate::federation::fetch_object::KIND_BACKFILL
//! Database failures are [`FederationError::Transient`]: the dispatcher
//! releases the dedupe row and the retry folds the activity again — safe
//! because the whole write is one transaction and a status committed
//! before a retry's death answers step 2's `Existing` instead of doubling.
//!
//! The module is a tree, the fold's halves in children: `resolve` keeps the
//! database- and network-backed reference arms (steps 4 to 6), `write` the
//! row assembly and the one-transaction write of status, media and poll
//! (steps 7 and 8), and here the ordering itself — [`ingest_note`] with its
//! dedupe read `seen_before`, the unsolicited gate `drops_unsolicited` and
//! the backfill enqueue `enqueue_backfill`. `test_support` holds the
//! fixtures the children's unit tests share, and each module's own `tests`
//! the tests beside the code they cover. The `pub(crate)` helpers the
//! inbound edit handler calls are re-exported here, so an outside path
//! stays `crate::services::ingest::…`.

use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use tracing::debug;
use uuid::Uuid;

use crate::{
    federation::{
        dispatch::FederationError,
        fetch_object::{BACKFILL_MAX_ATTEMPTS, KIND_BACKFILL, backfill_key},
        note_parser::{self, ParsedNote},
    },
    jobs::QUEUE_BACKGROUND,
    models::{
        follow,
        job::{self, NewJob},
        status::{self, Status, Visibility},
        user::User,
    },
    services::create_status::after_status_created,
    state::AppState,
};

#[cfg(doc)]
use crate::federation::fetch_actor::get_or_fetch_by_ap_id;

pub mod resolve;
pub mod write;

#[cfg(test)]
mod test_support;

pub(crate) use self::{
    resolve::{merge_accounts, resolve_local_audience, resolve_mentions},
    write::{insert_media, insert_poll},
};
use self::{
    resolve::{resolve_quote, resolve_reply, upsert_emojis},
    write::{build_new_status, write_ingested},
};

/// The time one unknown-mention actor fetch may take before the mention is
/// dropped: three seconds, the same order as the outbound client's request
/// budgets — a slow peer may delay an ingestion slightly, never stall it.
const MENTION_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// How many unknown mention actors one status may look up over the
/// network; further mentions of the same document drop out unresolved, the
/// way [`crate::services::mentions`] caps its acct lookups through
/// [`crate::config::Config::max_remote_mention_lookups`].
const MAX_MENTION_LOOKUPS: usize = 10;

/// What [`ingest_note`] decided about one received object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    /// The instance gained the post: the fresh row, hook already run.
    Created(Status),
    /// The object's `uri` names a status this instance already stores —
    /// a redelivered `Create`, a second activity about the same object:
    /// the stored row, written again by nobody.
    Existing(Status),
    /// Nothing was written; the reason is static and rides the
    /// dispatcher's debug log.
    Ignored(&'static str),
}

/// The reply facts [`ingest_note`] resolved against stored statuses.
#[derive(Clone, Debug, Default)]
struct ReplyLink {
    /// The stored parent the object answers, when this instance has it.
    parent: Option<Status>,
    /// The answered object's `uri` when no stored row answers it — kept as
    /// the status's `in_reply_to_ap_id` so a later fetch of the parent can
    /// close the link.
    unresolved_ap_id: Option<String>,
    /// The conversation of the oldest stored status sharing the object's
    /// `context`, consulted only when the parent is not stored.
    context_conversation: Option<Uuid>,
}

/// Ingests one received `Note`-family object authored by `author`.
///
/// `object` is the embedded document (the dispatcher's routing already
/// proved its `type` one the create handlers register and, for a `Create`,
/// that its host matches the actor's); `author` is the [`User`] mirror
/// the delivery was authenticated by — the document's `attributedTo` must
/// name exactly that account, which [`note_parser::parse_note`] enforces.
///
/// The function owns the whole fold the [module docs](self) lay out and
/// reports what it decided as an [`IngestOutcome`].
///
/// # Errors
///
/// [`FederationError::Permanent`] when the document refuses parsing — it
/// is no note, carries no usable id, or fails containment; no retry can
/// make it one. [`FederationError::Transient`] for any database failure
/// (or a mention fetch's own database write failing): the dedupe ledger
/// row goes back and the activity is folded again.
pub async fn ingest_note(
    state: &AppState,
    author: &User,
    object: &Value,
) -> Result<IngestOutcome, FederationError> {
    ingest_note_delivered(state, author, object, None).await
}

/// [`ingest_note`] for a delivery to a personal inbox: `owner` is the local
/// account whose inbox received the object, if any.
///
/// Senders strip `bto`/`bcc` before delivery, so a privately addressed post
/// can arrive with nothing naming its recipient, and a `direct` status is
/// only visible to its `status_mentions`. Like Mastodon's
/// `delivered_to_account_id` handling, a local `owner` is therefore added
/// to the mentioned accounts when the document did not mention them. Only
/// `direct` posts get this: every other visibility already reaches the
/// owner through follows, and a mention would notify them for each post a
/// followed account delivers to their personal inbox.
///
/// # Errors
///
/// As [`ingest_note`].
pub async fn ingest_note_delivered(
    state: &AppState,
    author: &User,
    object: &Value,
    owner: Option<&User>,
) -> Result<IngestOutcome, FederationError> {
    let parsed = note_parser::parse_note(object, author, Utc::now()).map_err(|err| {
        FederationError::Permanent(format!("the object is not ingestable: {err}"))
    })?;
    let uri =
        parsed.status.uri.as_deref().ok_or_else(|| {
            FederationError::Permanent("the parsed object carries no uri".to_owned())
        })?;
    if let Some(decision) = seen_before(state, uri).await? {
        return Ok(decision);
    }
    if !author.is_active.get() {
        debug!(account = %author.ap_id, "a deactivated author's post is not ingested");
        return Ok(IngestOutcome::Ignored("deactivated author"));
    }
    let reply = resolve_reply(state, &parsed).await?;
    let quote_of_id = resolve_quote(state, parsed.quote_uri.as_deref()).await?;
    let mut mentioned = resolve_mentions(state, &parsed.mention_hrefs).await?;
    merge_accounts(
        &mut mentioned,
        resolve_local_audience(state, &parsed.audience).await?,
    );
    if let Some(owner) = owner
        && parsed.status.visibility == Visibility::Direct
        && owner.local.get()
        && mentioned.iter().all(|account| account.id != owner.id)
    {
        mentioned.push(owner.clone());
    }
    if drops_unsolicited(state, &parsed, author, &reply, &mentioned).await? {
        debug!(account = %author.ap_id, "an unsolicited public post from an unfollowed author is dropped");
        return Ok(IngestOutcome::Ignored("unsolicited public"));
    }
    upsert_emojis(state, &parsed.emojis).await?;
    let new = build_new_status(&parsed, &reply, quote_of_id, &mentioned);
    let row = write_ingested(state, &new, &parsed).await?;
    after_status_created(state, &row).await;
    enqueue_backfill(state, &row).await;
    debug!(
        status = %row.id,
        visibility = %row.visibility,
        "a remote note was ingested"
    );
    Ok(IngestOutcome::Created(row))
}

/// Step 11: a remote reply stored with an unresolved parent id gets the
/// [`federator.backfill`] job that closes the thread once the parent can
/// be fetched (see [`crate::federation::fetch_object`]). A row without a
/// deferred link has nothing to backfill; an enqueue the database refuses
/// is logged and swallowed — losing one backfill pass is cheaper than
/// refusing a post whose row is already committed, and the same unique
/// key keeps a redelivered `Create` from queueing the work twice.
///
/// [`federator.backfill`]: crate::federation::fetch_object::KIND_BACKFILL
async fn enqueue_backfill(state: &AppState, row: &Status) {
    let Some(parent_ap_id) = row.in_reply_to_ap_id.as_deref() else {
        return;
    };
    let unique_key = backfill_key(row.id);
    let args = json!({ "status_id": row.id.to_string(), "depth": 0 });
    debug!(status = %row.id, parent = %parent_ap_id, "queueing the thread backfill of a deferred reply");
    let new_job = NewJob {
        queue: QUEUE_BACKGROUND,
        kind: KIND_BACKFILL,
        args,
        run_at: None,
        max_attempts: Some(BACKFILL_MAX_ATTEMPTS),
        unique_key: Some(&unique_key),
    };
    if let Err(err) = job::enqueue(&state.db, new_job).await {
        debug!(status = %row.id, %err, "the backfill enqueue failed");
    }
}

/// Step 2: what a repeat sight of `uri` answers — the tombstone's refusal,
/// the live row's restatement, or [`None`] when this instance has never
/// stored the object.
async fn seen_before(
    state: &AppState,
    uri: &str,
) -> Result<Option<IngestOutcome>, FederationError> {
    let stored = status::get_by_uri(&state.db, uri)
        .await
        .map_err(|err| FederationError::transient("the status uri read failed", &err))?;
    Ok(stored.map(|row| {
        if row.deleted_at.is_some() {
            IngestOutcome::Ignored("tombstoned")
        } else {
            IngestOutcome::Existing(row)
        }
    }))
}

/// Step 10: whether the `ACCEPT_UNSOLICITED_PUBLIC` gate drops this post.
///
/// The gate answers `true` — drop — only while the setting is off, for a
/// `public` or `unlisted` status whose author no local account follows and
/// that neither mentions a local user nor answers a local status. Every
/// other post keeps its author's reasons for having arrived here:
/// followers-addressed and direct posts were aimed at somebody on this
/// instance, and a followed author's post is solicited by definition.
async fn drops_unsolicited(
    state: &AppState,
    parsed: &ParsedNote,
    author: &User,
    reply: &ReplyLink,
    mentioned: &[User],
) -> Result<bool, FederationError> {
    if state.config.accept_unsolicited_public.get() {
        return Ok(false);
    }
    if !matches!(
        parsed.status.visibility,
        Visibility::Public | Visibility::Unlisted
    ) {
        return Ok(false);
    }
    if reply.parent.as_ref().is_some_and(|parent| parent.local)
        || mentioned.iter().any(|account| account.local.get())
    {
        return Ok(false);
    }
    let solicited = follow::has_local_followers(&state.db, author.id)
        .await
        .map_err(|err| FederationError::transient("the follower gate read failed", &err))?;
    Ok(!solicited)
}

/// A remote count narrowed to the `integer` column's width: counts beyond
/// it clamp to the column's maximum rather than refuse the whole post for
/// a number nobody can vote into being true.
fn whole(count: i64) -> i32 {
    i32::try_from(count).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests;
