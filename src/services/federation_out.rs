//! Outbound federation triggered by client-API writes.
//!
//! The client API keeps its post-write hooks deliberately thin: the endpoint
//! writes its relational fact, answers the client, and the federating half of
//! the job lands here. The entry points arrive:
//!
//! - [`after_block_changed`]: a `Block` (or its `Undo`) written through
//!   `POST /api/v1/accounts/{id}/block` or the C2S endpoints, fed to the
//!   blocked account's instance only — a block of a local account is settled
//!   in our own database and never federates, as in Pleroma's
//!   `CommonApi.block`/`unblock` (the target's `local?` gate);
//! - [`after_favourite_changed`] / [`after_reaction_changed`]: the `Like`
//!   or `EmojiReact` behind the favourite or reaction row (and its `Undo`),
//!   delivered to the status author's personal inbox and — when the
//!   acted-on status is public — to the actor's remote followers, as in
//!   Pleroma's `CommonAPI.favorite`/`react_with_emoji` pipelines;
//! - [`after_reblog_changed`]: the `Announce` a reblog row *is* (decision 6
//!   — the document's id is the row's stored `uri`), addressed by the
//!   boost's own visibility, and its `Undo` when the boost is withdrawn;
//!   reblogs of `local`-visibility statuses, like `local`-visibility
//!   statuses themselves, never federate;
//! - [`after_status_created`] / [`after_status_edited`] /
//!   [`after_status_deleted`]: the note lifecycle of a status row — the
//!   `Create` that carries it out when its author posts, the `Update` that
//!   re-drafts it when its author edits it, and, on a delete, the
//!   withdrawal the network is owed: the `Delete` of a tombstoned plain
//!   status, or the `Undo` of the `Announce` a tombstoned reblog row was
//!   (never both: a boost is withdrawn, not deleted). All three are built
//!   from the documents [`crate::federation::note`] renders — the same
//!   `Note` the object route serves and the same addressing rule — so what
//!   leaves this instance is what a peer can read back from it, and a
//!   `local`-visibility post or a post by a switched-off account never
//!   leaves it at all;
//! - [`after_profile_updated`]: an `Update` of the actor document after
//!   `PATCH /api/v1/accounts/update_credentials`, delivered to the account's
//!   remote followers' inboxes — Pleroma's
//!   `update_and_followers_notif_if_local` gate, minus the follow-notify
//!   relay (our local followers are told by their timelines, not by an
//!   activity). One burst of edits collapses into one queued activity: the
//!   job rides a per-account `unique_key`, so a repeat while the first is
//!   still live is dropped — and the announcement carries the document of
//!   the edit that was queued, not of whichever later edit the dedup
//!   swallowed; the freshest row speaks again with the next write that
//!   gets its own job.
//! - [`after_account_moved`]: the pair that announces an account's move to
//!   another actor — the `Move` Pleroma's `ActivityPub.move` inserts, `to`
//!   the origin's followers collection so the REMOTE followers (the local
//!   ones were already transferred) learn where their follow went, and the
//!   profile `Update` of the re-read row so the actor document peers hold
//!   grows its `movedTo`;
//! - [`after_pin_changed`]: the `Add` / `Remove` that features a post in
//!   its author's `featured` collection or withdraws it — Pleroma's
//!   `Builder.pin` and `Builder.unpin`, `to` the public with the author's
//!   followers collection in `cc`, delivered to the remote followers;
//! - [`after_list_member_changed`]: the `Add` / `Remove` that announces a
//!   REMOTE account's joining or leaving one of the caller's lists —
//!   `actor` the list's owner, `object` the member's actor id and `target`
//!   the list's curated inbox url, addressed to the member alone so the
//!   publication reaches their inbox and no follower fan-out happens;
//! - [`after_account_deleted`]: a `Delete` of the actor, run as the first
//!   act of the `users.delete` sweep (before it clears the relationship
//!   rows the fan-out reads), delivered to the union of the account's
//!   remote followers' and remote followees' inboxes (both sides learned
//!   something from the actor, both must learn it is gone);
//! - [`after_account_disabled`]: nothing — deactivation deliberately
//!   federates no activity, so the hook is the documented landing point for
//!   a ticket that ever changes that answer;
//! - [`after_report_created`]: the `Flag` a forwarded report owes a REMOTE
//!   target's instance — the instance actor speaks it, the reported account
//!   and its cited status uris are the object and the reporter's comment is
//!   the content, delivered to the target's inbox alone (Mastodon's
//!   forwarded-report convention). A local target or a filing that asked
//!   for no forwarding stays with our own moderators;
//! - [`after_poll_vote`]: the answers a ballot on a REMOTE status's poll
//!   becomes — one `Create` per chosen option, its object a bodyless `Note`
//!   naming the option title and answering the question's uri (the shape
//!   [`crate::federation::handlers::vote`] folds on the way in), addressed
//!   to the poll's author alone and delivered to their inbox. A poll on a
//!   local status is this instance's own fact and announces nothing;
//! - [`after_chat_message_created`] / [`after_chat_message_deleted`]: the
//!   `Create{ChatMessage}` a local account's message to a REMOTE
//!   counterpart is, delivered to that counterpart's inbox alone, and the
//!   `Delete` of its reserved object id when the sender withdraws the
//!   message. Chat messages to local recipients never travel over HTTP,
//!   and a recipient whose `accepts_chat_messages` flag is down is not
//!   delivered to;
//! - [`accept_follow`] / [`reject_follow`]: the answer to a remote account's
//!   `Follow` of a local one, generated by the inbound follow handler (and,
//!   with a locked account's authorization, the follow-request endpoint).
//!   The `Accept` embeds the delivered `Follow` as `{id, type, actor,
//!   object}`, because Mastodon matches the answer by what it carries rather
//!   than by an id it would have to fetch, and both answers go to the
//!   follower's inbox alone — `only_to` is the follower, so the fan-out never
//!   touches the followee's other followers;
//! - [`after_follow_changed`]: the outbound side of the follow write itself
//!   — a local follower's `Follow` of a REMOTE account, under the row's own
//!   stored `ap_id` (Pleroma's `ActivityPub.follow`), and, when the
//!   relationship is withdrawn, the `Undo` embedding that same document
//!   under a fresh id (`ActivityPub.unfollow`). A re-follow of an open pair
//!   restates the same activity id, so the per-inbox delivery dedup keeps
//!   the handshake a single sending; a LOCAL followee is settled in the
//!   database and hears nothing;
//! - [`after_follow_request_resolved`]: the follow-request endpoint's
//!   landing point for the answers above — [`accept_follow`] for an
//!   authorization, [`reject_follow`] for a refusal, run only for a REMOTE
//!   requester over the delivered activity's stored id;
//! - [`after_follower_removed`]: the `Reject` of a forced removal —
//!   `remove_from_followers` telling the dropped REMOTE follower that their
//!   `Follow` no longer stands, Pleroma's `User.remove_follower` answer;
//! - [`after_chat_message_deleted_by_admin`]: the `Delete` of a withdrawn
//!   local-origin chat message, run by the admin chat endpoint after its
//!   tombstone — `actor` the message's (local) sender, `object` the stored
//!   `ap_id` as an embedded `ChatMessage` reference, and `to` the chat's
//!   REMOTE other participant: exactly the peer that received the message
//!   hears it withdrawn. A message with no `ap_id` or one whose id belongs
//!   to another instance was never ours to announce, and a pair of local
//!   accounts needs no delivery; nothing goes out in those cases.
//!
//! The interaction documents come from the [`activities`] builders under
//! the addressing the acted-on status earns: the author stands in `to`,
//! the actor's followers collection (and, for a public like, the public
//! address) in `cc` — and a withdrawal repeats the addressing of the
//! activity it embeds, so exactly the inboxes that heard the fact hear its
//! taking-back. Each document's id is the row's own stored `ap_id` (the
//! reblog row's `uri`), and an `Undo`'s is a fresh
//! `{BASE_URL}/activities/{uuid}` (decision 6).
//!
//! Every lifecycle hook is fire-and-forget: it builds the document, hands it
//! to [`crate::federation::publisher`] (which wraps it in one
//! `federator.publish` job, resolved into per-inbox `federator.deliver`
//! jobs when the worker runs it), and returns. Failures to enqueue are
//! logged, never propagated: the user-visible write has already committed,
//! and a client that got its 200 must not see an `ActivityPub` problem. The
//! follow activities are the deliberate exception: the local side of a
//! follow pair announces itself with
//! [`publish`](crate::federation::publisher::publish) directly, `only_to`
//! naming the one remote actor — there is no follower expansion to defer,
//! so the request pays for an address lookup and a job insert, and the
//! network stays the delivery worker's work either way. The inbound
//! dispatcher's answers hand their failure back to its retry; the endpoint
//! hooks run the same calls fire-and-forget and log theirs.
//!
//! [`activities`]: crate::federation::activities

use serde_json::{Value, json};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    config::Config,
    error::AppResult,
    federation::{
        activities::{
            AnsweredActivity, EmojiReactContext, FlagContext, ObjectActivityContext,
            TargetedActivityContext, accept_json, activity_id, add_json, announce_json, block_json,
            emoji_react_json, flag_json, follow_json, like_json, move_json, reject_json,
            remove_json, undo_json,
        },
        actor::actor_json,
        note,
        publisher::{self, PublishOpts},
    },
    models::{
        block::Block,
        chat::{self, ChatMessageReference},
        emoji_reaction::EmojiReaction,
        favourite::Favourite,
        follow::{self, Follow},
        media_attachment::{self, MediaAttachment},
        pinned_status::PinnedStatus,
        poll::{self, Poll as PollRow, PollOption},
        report::{self, Report as ReportRow},
        status::{self, Status, Visibility},
        user::{self, User, featured},
    },
    services::instance_actor,
    state::AppState,
    util::{id::new_id, time::mastodon_time},
};

/// The `ActivityStreams` public address, the `to` entry of a profile
/// `Update` and an account `Delete`.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// Announce the block or unblock the caller just performed.
///
/// `undo` selects the direction: `false` publishes the `Block`, `true`
/// publishes its `Undo` embedding it. Only a REMOTE blocked account is
/// worth announcing (a local one never sees an activity); the recipient is
/// the blocked user alone, so the document's `to` names their actor id and
/// the publisher's recipient resolution delivers to their personal inbox.
/// The Block's activity id is the stored `blocks.ap_id` — the row minted it
/// when the block was written and the id must not change — while the Undo
/// gets a fresh `{BASE_URL}/activities/{uuid}` id, the shape every
/// activity this instance sends carries.
///
/// A block row without an `ap_id` (a mirrored remote block, which this
/// instance does not own) and a pair whose users are gone or unkeyed
/// publish nothing.
pub async fn after_block_changed(state: &AppState, block: &Block, undo: bool) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let (Some(blocker), Some(target)) = (
        load_user(state, block.blocker_id).await,
        load_user(state, block.blocked_id).await,
    ) else {
        return; // a pair whose rows are gone has nothing left to federate
    };
    if target.local.get() || !blocker.local.get() {
        return; // a local pair is settled in the database; only our actors sign
    }
    let Some(document) = block_document(state, block, &blocker, &target, undo) else {
        return; // a mirrored block belongs to its maker's instance, not ours
    };
    // One recipient address, the blocked actor: the publisher resolves it to
    // their personal inbox and delivers there alone.
    match publisher::publish_in_background(state, blocker.id, &document, &[], None).await {
        Ok(()) => debug!(
            actor = %blocker.ap_id,
            target = %target.ap_id,
            undo,
            "queued block federation"
        ),
        Err(err) => warn!(
            %err,
            actor = %blocker.ap_id,
            target = %target.ap_id,
            "could not queue block federation"
        ),
    }
}

/// The `Block` (or the `Undo` wrapping it) for the row `block`.
///
/// The `Block` half is [`block_json`] with its `id` replaced by the stored
/// string — the builders mint ids from a fresh uuid and the row's `ap_id`
/// is its own (decision 6), so only the stored text keeps a peer's
/// `Undo`-lookup pointed at the activity it already holds. `None` when the
/// row carries no id to stand behind the document.
fn block_document(
    state: &AppState,
    block: &Block,
    blocker: &User,
    target: &User,
    undo: bool,
) -> Option<Value> {
    let stored_id = block.ap_id.as_deref()?;
    let mut document = block_json(
        &state.config,
        block.id,
        &blocker.ap_id,
        &target.ap_id,
        &block.inserted_at,
    );
    if let Some(fields) = document.as_object_mut() {
        let _previous = fields.insert("id".to_owned(), json!(stored_id));
    }
    if undo {
        let now = chrono::Utc::now();
        let to = [target.ap_id.as_str()];
        Some(undo_json(&AnsweredActivity {
            config: &state.config,
            id: new_id(),
            actor_ap_id: &blocker.ap_id,
            to: &to,
            cc: &[],
            answered: &document,
            published: &now,
        }))
    } else {
        Some(document)
    }
}

/// The local actor, the acted-on status and its author: the three rows
/// every interaction document is built from.
#[derive(Debug)]
struct StatusAction {
    /// The local account whose write is announced.
    actor: User,
    /// The status the interaction names — the original, for a boost.
    status: Status,
    /// The status' author, the `to` entry of every delivered interaction.
    author: User,
    /// Whether the status is public: the condition under which the actor's
    /// followers (and, for a like, the relays) hear about the interaction
    /// at all.
    public: bool,
}

/// Reads the three rows behind one interaction with status `status_id` by
/// `actor_id`, answering `None` when there is nothing to announce: an
/// actor that is gone or not ours to publish as, a status that is gone or
/// `local`-visibility (which never federates, whatever the action), or an
/// author that is gone.
///
/// The author's `is_active` flag is not a gate here, as it is not one in
/// [`note_withdrawal`]: every arm this feeds announces a fact about a
/// status that already reached the network (or takes one back), and a peer
/// that holds the copy has to hear about the change whichever flag the
/// acting account now wears. A NEW post is the case the flag does gate —
/// see [`note_publisher`].
async fn status_action(state: &AppState, actor_id: Uuid, status_id: Uuid) -> Option<StatusAction> {
    let actor = load_user(state, actor_id).await?;
    if !actor.local.get() {
        return None; // a mirror's interaction belongs to its own instance
    }
    let status = load_status(state, status_id).await?;
    if status.visibility == Visibility::Local {
        return None; // a local-only post federates nothing about itself
    }
    let author = load_user(state, status.user_id).await?;
    Some(StatusAction {
        public: status.visibility == Visibility::Public,
        actor,
        status,
        author,
    })
}

/// Reads one status row for the hooks, answering `None` for a gone row or
/// a database that will not answer (a federation miss, never a failure).
async fn load_status(state: &AppState, id: Uuid) -> Option<Status> {
    status::get_by_id(&state.db, id)
        .await
        .unwrap_or_else(|err| {
            warn!(%err, %id, "federation hook could not read a status row");
            None
        })
}

/// The address naming the actor's followers collection: the stored
/// `followers_url`, which is what [`publisher::publish`] expands, with the
/// `{ap_id}/followers` construction behind a row that carries none.
///
/// [`publisher::publish`]: crate::federation::publisher::publish
fn followers_address(actor: &User) -> String {
    actor
        .followers_url
        .clone()
        .unwrap_or_else(|| format!("{}/followers", actor.ap_id))
}

/// Hand one built activity to the publisher's background fan-out: the
/// document's own `to` and `cc` name its recipients, so no extra inboxes
/// ride along. As every lifecycle hook here, an enqueue that fails is
/// logged, never returned: the client's write already stands.
async fn queue_activity(state: &AppState, actor: &User, document: &Value, what: &str) {
    match publisher::publish_in_background(state, actor.id, document, &[], None).await {
        Ok(()) => debug!(actor = %actor.ap_id, "queued {what} federation"),
        Err(err) => warn!(%err, actor = %actor.ap_id, "could not queue {what} federation"),
    }
}

/// Hand one built activity to the publisher with the recipient set fixed to
/// `recipient_ap_id` alone: [`publish`] resolves that one address to the
/// peer's personal inbox and queues the delivery, so nothing fans out
/// beyond the actor the activity is about. As every hook here a failed
/// fan-out is logged, never returned: the client's write already stands.
///
/// [`publish`]: crate::federation::publisher::publish
async fn publish_only_to(
    state: &AppState,
    actor: &User,
    recipient_ap_id: &str,
    document: &Value,
    what: &str,
) {
    let opts = PublishOpts {
        extra_inboxes: Vec::new(),
        only_to: Some(vec![recipient_ap_id.to_owned()]),
    };
    match publisher::publish(state, actor, document, opts).await {
        Ok(count) => debug!(
            actor = %actor.ap_id,
            recipient = recipient_ap_id,
            count,
            "queued {what} federation"
        ),
        Err(err) => warn!(
            %err,
            actor = %actor.ap_id,
            recipient = recipient_ap_id,
            "could not queue {what} federation"
        ),
    }
}

/// The `Undo` that withdraws `answered`: its own fresh
/// `{BASE_URL}/activities/{uuid}` id (decision 6), `actor_ap_id` as its
/// actor, and the very `to` and `cc` the answered activity travelled
/// under — exactly who heard the fact hears its taking-back.
fn withdrawal(
    state: &AppState,
    actor_ap_id: &str,
    to: &[&str],
    cc: &[&str],
    answered: &Value,
) -> Value {
    let now = chrono::Utc::now();
    undo_json(&AnsweredActivity {
        config: &state.config,
        id: new_id(),
        actor_ap_id,
        to,
        cc,
        answered,
        published: &now,
    })
}

/// Announce the favourite or unfavourite the caller just performed.
///
/// The document is the `Like` behind the row (or the `Undo` wrapping it),
/// delivered to the status author's inbox and — for a public status — to
/// the actor's remote followers, the pair Pleroma's
/// `CommonAPI.favorite`/`unfavorite` send.
/// The like's activity id is the row's stored `ap_id` — the id the
/// favourite write minted (decision 6) — and the unfavourite's `Undo`
/// carries a fresh one while embedding that same id. A LOCAL status behind
/// the like changes the fan-out, not the document: the publisher resolves
/// the local author to no delivery target, so a local-only audience
/// queues nothing and the remote followers' inboxes stand as the only
/// recipients. A `local`-visibility status federates nothing at all.
///
/// A row with no `ap_id` (a mirrored remote like, which this instance does
/// not own), a gone status or author, and a non-local actor publish
/// nothing.
pub async fn after_favourite_changed(state: &AppState, favourite: &Favourite, favouriting: bool) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(like_id) = favourite.ap_id.as_deref() else {
        return; // an id-less row has no document to stand behind
    };
    let Some(action) = status_action(state, favourite.user_id, favourite.status_id).await else {
        return;
    };
    let followers = followers_address(&action.actor);
    let to = [action.author.ap_id.as_str()];
    let cc: Vec<&str> = if action.public {
        vec![followers.as_str(), PUBLIC]
    } else {
        Vec::new()
    };
    let like = like_json(&ObjectActivityContext {
        config: &state.config,
        id: like_id,
        actor_ap_id: &action.actor.ap_id,
        object_uri: &action.status.uri,
        to: &to,
        cc: &cc,
        published: &favourite.inserted_at,
    });
    let document = if favouriting {
        like
    } else {
        withdrawal(state, &action.actor.ap_id, &to, &cc, &like)
    };
    queue_activity(state, &action.actor, &document, "favourite").await;
}

/// Announce the reaction or its withdrawal.
///
/// The document is the `EmojiReact` behind the row — `content` the stored
/// emoji, a custom one also carrying its image as an `Emoji` `tag` —
/// under the same addressing [`like_json`] is sent with, minus the public
/// address (Pleroma's `react_with_emoji` arm), and the `Undo` repeating it
/// when the reaction is taken back.
///
/// The reaction's activity id is the row's stored `ap_id` (decision 6);
/// the same id-less, gone-row and non-local-actor refusals as
/// [`after_favourite_changed`] answer with silence. A repeat `PUT` fires
/// the hook again on the unchanged row, and the per-activity, per-inbox
/// deduplication of the delivery queue is what keeps the restated fact a
/// single sending.
pub async fn after_reaction_changed(state: &AppState, reaction: &EmojiReaction, reacting: bool) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(react_id) = reaction.ap_id.as_deref() else {
        return; // an id-less row has no document to stand behind
    };
    let Some(action) = status_action(state, reaction.user_id, reaction.status_id).await else {
        return;
    };
    let followers = followers_address(&action.actor);
    let to = [action.author.ap_id.as_str()];
    let cc: Vec<&str> = if action.public {
        vec![followers.as_str()]
    } else {
        Vec::new()
    };
    let document = emoji_react_json(&EmojiReactContext {
        config: &state.config,
        id: react_id,
        actor_ap_id: &action.actor.ap_id,
        object_uri: &action.status.uri,
        to: &to,
        cc: &cc,
        emoji: &reaction.emoji,
        emoji_url: reaction.url.as_deref(),
        published: &reaction.inserted_at,
    });
    let document = if reacting {
        document
    } else {
        withdrawal(state, &action.actor.ap_id, &to, &cc, &document)
    };
    queue_activity(state, &action.actor, &document, "reaction").await;
}

/// Announce the boost or unboost the caller just performed.
///
/// The document is the `Announce` the reblog row *is* (decision 6 — its
/// id is the row's stored `uri`, which is what a withdrawal's `Undo`
/// embeds and a peer's later fetch names back), or the `Undo` wrapping it.
///
/// The addressing follows the boost's own visibility as Pleroma's
/// `Builder.announce` follows it: a public one names the world in `to`,
/// an unlisted or private one the actor's followers collection, and the
/// `cc` line carries the followers collection beside the boosted status'
/// author either way — so the fan-out reaches the remote followers' and
/// the original author's inboxes, and a boost of a LOCAL status federates
/// to the followers alone (the publisher resolves the local author to
/// nothing). A `local`-visibility boost federates nothing, and neither
/// does one whose original was `local`-visibility: those facts are this
/// instance's alone.
pub async fn after_reblog_changed(state: &AppState, reblog: &Status, reblogging: bool) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(original_id) = reblog.reblog_of_id else {
        return; // not a reblog row: there is no Announce behind it
    };
    if reblog.visibility == Visibility::Local {
        return; // a local-only boost stays local
    }
    let Some(action) = status_action(state, reblog.user_id, original_id).await else {
        return;
    };
    let document = reblog_activity(state, reblog, &action, !reblogging);
    queue_activity(state, &action.actor, &document, "reblog").await;
}

/// Announce the status the caller just posted: the `Create` activity
/// carrying the note as this instance federates it.
///
/// The document is [`note::note_json`] wrapped in
/// [`note::create_activity_json`] — the very pair the `/objects/{uuid}` and
/// `/activities/{id}` routes serve, so what a follower's instance receives
/// and what it later fetches are one document — published as the status'
/// author, whose followers collection and mentionees the note's own `to` and
/// `cc` name and whom the publisher therefore expands. Nothing is added to
/// the fan-out beside that: a reply to a REMOTE parent needs no special
/// arm, because the parent's author rides in the mention list the note's
/// addressing is built from, and a `direct` status addresses its mentionees
/// alone, so the followers never hear it.
///
/// Silence is the answer of a mirrored row (a remote post belongs to its
/// origin's `Create`), of a reblog row (decision 6 gives it the `Announce`
/// of [`after_reblog_changed`]), of a `local`-visibility post, and of a post
/// whose author is not one of our own active actors.
pub async fn after_status_created(state: &AppState, status: &Status) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(author) = note_publisher(state, status).await else {
        return;
    };
    let Some(document) = note_document(state, status.id, NoteActivity::Create).await else {
        return;
    };
    queue_activity(state, &author, &document, "note").await;
}

/// Announce the edit the caller just wrote: the `Update` activity carrying
/// the status' NEW note.
///
/// The document is the re-rendered [`note::note_json`] — content, warning,
/// attachments and the `updated` stamp the row now carries — wrapped in
/// [`note::update_activity_json`] under a freshly minted
/// `{BASE_URL}/activities/{uuid}` id, and it travels under the addressing
/// the note itself states: the same audience the `Create` reached, so an
/// edit corrects the copy at exactly the instances that hold one. The hook
/// is handed the *pre-edit* row, which is why the document is re-read by id
/// rather than built from the argument.
///
/// A `local`-visibility, mirrored or reblog row and a gone or
/// non-publishing author answer with silence, as in
/// [`after_status_created`].
pub async fn after_status_edited(state: &AppState, status: &Status) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(author) = note_publisher(state, status).await else {
        return;
    };
    let Some(document) = note_document(state, status.id, NoteActivity::Update).await else {
        return;
    };
    queue_activity(state, &author, &document, "note edit").await;
}

/// The two shapes [`note_document`] wraps a status' live note into: the
/// `Create` that first announces it and the `Update` that re-drafts it,
/// which differ only in the wrapper they put on the same document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoteActivity {
    /// The status' `Create`: the note wrapped by
    /// [`note::create_activity_json`] under the deterministic
    /// `{BASE_URL}/activities/{status id}` id.
    Create,
    /// The edited status' `Update`: the note wrapped by
    /// [`note::update_activity_json`] under a freshly minted activity id.
    Update,
}

/// The status' live note, wrapped in the lifecycle activity `kind` names,
/// and `None` (logged) when there is nothing to announce: the row or its
/// author died between the write and this read, or the row wears a
/// visibility [`note::note_json`] refuses to render.
async fn note_document(state: &AppState, status_id: Uuid, kind: NoteActivity) -> Option<Value> {
    let ctx = match note::load_note_context(&state.db, &state.config, status_id).await {
        Ok(ctx) => ctx,
        Err(err) => {
            warn!(
                %err,
                status = %status_id,
                "note federation could not read the status document"
            );
            return None;
        }
    };
    let note = note::note_json(&ctx)?;
    let document = match kind {
        NoteActivity::Create => note::create_activity_json(&note, &ctx.status, &state.config),
        NoteActivity::Update => note::update_activity_json(&note, new_id(), &state.config),
    };
    Some(document)
}

/// The author of a status this instance may announce as a note, `None` when
/// the row is not its business: a mirrored row (its origin sent the
/// `Create`), a reblog row (decision 6: its activity is the `Announce`
/// [`after_reblog_changed`] sends), a `local`-visibility post (which never
/// left the database), a gone author, an author that is not one of our own,
/// or an author switched off — a deactivated account's new post stays inside
/// this instance, the way the ingest fold refuses to store a live post for
/// one.
async fn note_publisher(state: &AppState, status: &Status) -> Option<User> {
    if !federates_as_note(status) {
        return None;
    }
    let author = load_user(state, status.user_id).await?;
    (author.local.get() && author.is_active.get()).then_some(author)
}

/// Whether a status row is the kind this instance sends note activities
/// for: one it owns, a plain post rather than a boost, and a visibility
/// that reaches past this instance.
const fn federates_as_note(status: &Status) -> bool {
    status.local && status.reblog_of_id.is_none() && !matches!(status.visibility, Visibility::Local)
}

/// Take the boost down when its row is deleted, or the post itself.
///
/// The two arms answer the two kinds of row: a deleted REBLOG withdraws
/// itself with the `Undo` of the `Announce` it was (never a `Delete` — the
/// original it named stands), and a deleted plain status is taken off the
/// network with the `Delete` of its note, whose object is the `Tombstone` the
/// object route serves at that uri from here on.
///
/// One call announces the one row the delete service hands over: the reblogs
/// that `status::soft_delete_with_reblogs` tombstones along with a deleted
/// original go down unannounced — a boost is withdrawn by the delete of the
/// boost, and a delete of the post it boosted is not that withdrawal.
pub async fn after_status_deleted(state: &AppState, deleted: &Status) {
    if !state.config.federation_enabled.get() {
        return;
    }
    match deleted.reblog_of_id {
        Some(original_id) => boost_withdrawal(state, deleted, original_id).await,
        None => note_withdrawal(state, deleted).await,
    }
}

/// The `Undo` of the `Announce` a deleted reblog row was.
///
/// A `local`-visibility boost never went out, so nothing is withdrawn, and
/// the rest of the refusals are [`status_action`]'s: an actor that is gone
/// or not ours, an original that has died in the meantime.
async fn boost_withdrawal(state: &AppState, deleted: &Status, original_id: Uuid) {
    if deleted.visibility == Visibility::Local {
        return; // a local-only boost stays local
    }
    let Some(action) = status_action(state, deleted.user_id, original_id).await else {
        return;
    };
    let document = reblog_activity(state, deleted, &action, true);
    queue_activity(state, &action.actor, &document, "boost withdrawal").await;
}

/// The `Delete` of a deleted plain status: the reserved object id as a
/// `Tombstone`, under the deterministic
/// `{BASE_URL}/activities/delete-{status id}` id and the addressing the
/// status itself travelled with.
///
/// The audience is the part that cannot be re-read: the live row is a
/// tombstone by the time this runs ([`note::load_note_context`] answers one
/// with a miss), so the two lines come from [`note::address_lines`] run over
/// the pre-delete row the delete service holds and the mention links still
/// attached to it — the very rule that wrote the note's own `to` and `cc`,
/// which is what makes the withdrawal reach exactly the inboxes that hold
/// the post and no others. A `direct` post's `Delete` therefore goes to its
/// mentionees alone; a public one's to the author's remote followers.
///
/// The author's `is_active` flag is deliberately not a gate here, unlike
/// [`note_publisher`]'s: a withdrawal is not a post going out but a copy
/// coming down, and a peer that still holds the note of a since-deactivated
/// account has to learn it is gone.
async fn note_withdrawal(state: &AppState, deleted: &Status) {
    if !federates_as_note(deleted) {
        return;
    }
    let Some(author) = load_user(state, deleted.user_id).await else {
        return;
    };
    if !author.local.get() {
        return; // a mirror's row belongs to its own instance's history
    }
    let mentions = match note::load_mentions(&state.db, deleted).await {
        Ok(mentions) => mentions,
        Err(err) => {
            warn!(
                %err,
                status = %deleted.id,
                "note withdrawal could not read the status' audience"
            );
            return;
        }
    };
    let (to, cc) = note::address_lines(deleted.visibility, &author, &mentions);
    let row = withdrawn_row(state, deleted).await;
    let document = note::delete_activity_json(&note::NoteDeletion {
        config: &state.config,
        status: &row,
        actor_ap_id: &author.ap_id,
        to: &to,
        cc: &cc,
    });
    queue_activity(state, &author, &document, "note withdrawal").await;
}

/// The row as the delete left it, for the `Tombstone` to report: the
/// database's own, whose `deleted_at` is the instant the withdrawal names
/// and whose document is the one `/objects/{uuid}` answers with beside its
/// `410`. The snapshot the caller holds stands in for it when the row cannot
/// be re-read at all — a purge between the delete and this hook — leaving
/// [`note::tombstone_json`] to report that row's last write instead.
async fn withdrawn_row(state: &AppState, deleted: &Status) -> Status {
    match status::get_by_id_including_deleted(&state.db, deleted.id).await {
        Ok(Some(row)) => row,
        Ok(None) => deleted.clone(),
        Err(err) => {
            warn!(
                %err,
                status = %deleted.id,
                "note withdrawal could not re-read the tombstoned row"
            );
            deleted.clone()
        }
    }
}

/// The `Announce` behind the reblog row `reblog`, in the undo arm wrapped
/// in the `Undo` that withdraws it; both travel under the addressing the
/// boost's visibility states ([`after_reblog_changed`]'s module-doc rule).
fn reblog_activity(state: &AppState, reblog: &Status, action: &StatusAction, undo: bool) -> Value {
    let followers = followers_address(&action.actor);
    let to: [&str; 1] = if reblog.visibility == Visibility::Public {
        [PUBLIC]
    } else {
        [followers.as_str()]
    };
    let cc = [followers.as_str(), action.author.ap_id.as_str()];
    let announce = announce_json(&ObjectActivityContext {
        config: &state.config,
        id: &reblog.uri,
        actor_ap_id: &action.actor.ap_id,
        object_uri: &action.status.uri,
        to: &to,
        cc: &cc,
        published: &reblog.inserted_at,
    });
    if undo {
        withdrawal(state, &action.actor.ap_id, &to, &cc, &announce)
    } else {
        announce
    }
}

/// Announce the profile that `update_credentials` just wrote.
///
/// The `Update`'s object is the actor document itself ([`actor_json`] of
/// the updated row), addressed to the public with the account's followers
/// collection in `cc` — the shape both Mastodon and Pleroma send, and the
/// publisher expands the collection into the remote followers' shared
/// (preferred) or personal inboxes when the job runs, so the fan-out never
/// blocks the request.
///
/// Debouncing lives in the queue: every job this function enqueues carries
/// the per-account key `profile-update:{user id}`, and [`job::enqueue`]'s
/// dedup drops a repeat while an earlier one is still live — so a client
/// `PATCH`ing five fields in five requests pays for exactly one activity.
///
/// [`job::enqueue`]: crate::models::job::enqueue
pub async fn after_profile_updated(state: &AppState, account: &User) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !account.local.get() {
        return; // only our own actors publish
    }
    let Some(followers_collection) = account.followers_url.as_deref() else {
        return; // an actor with no followers collection has nobody to tell
    };
    let document = update_document(state, account, followers_collection);
    let unique_key = format!("profile-update:{}", account.id);
    match publisher::publish_in_background(state, account.id, &document, &[], Some(&unique_key))
        .await
    {
        Ok(()) => debug!(actor = %account.ap_id, "queued profile Update"),
        Err(err) => warn!(%err, actor = %account.ap_id, "could not queue profile Update"),
    }
}

/// The actor document for the profile in `account`, as an `Update` activity
/// addressed to the public and to the account's followers collection.
fn update_document(state: &AppState, account: &User, followers_collection: &str) -> Value {
    json!({
        "@context": note::json_ld_context(&state.config),
        "id": activity_id(&state.config, new_id()),
        "type": "Update",
        "actor": account.ap_id,
        "object": actor_json(account, &state.config),
        "to": [PUBLIC],
        "cc": [followers_collection],
    })
}

/// Announce the account `move_account` just moved onto `new`.
///
/// Two documents tell a move, both published as the origin:
///
/// - the `Move` — [`move_json`] with the origin's own actor id as both
///   `actor` and `object` and the destination's as `target`, `to` the
///   origin's followers collection. The endpoint transferred the LOCAL
///   followers before this hook runs; the remote ones keep following the
///   origin until their own instances act on this activity, so the
///   publisher's follower expansion is exactly the audience Pleroma's
///   `ActivityPub.move` delivers to. Its id is a fresh
///   `{BASE_URL}/activities/{uuid}` (decision 6);
/// - a profile `Update` — [`after_profile_updated`] run off the re-read
///   row, so the actor document it embeds carries the
///   `movedTo` the account just acquired (and its `alsoKnownAs` aliases).
///   The per-account debounce key of that hook is shared here by design:
///   a move straight after a burst of profile edits is one announcement
///   of the freshest document.
///
/// The `old` row is the pre-move snapshot the endpoint holds; the hook
/// re-reads it, and a row that vanished in between announces nothing.
pub async fn after_account_moved(state: &AppState, old: &User, new: &User) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !old.local.get() {
        return; // only our own actors announce a move
    }
    let Some(moved) = load_user(state, old.id).await else {
        return; // the account is gone; there is nothing left to tell
    };
    let followers = followers_address(&moved);
    let now = chrono::Utc::now();
    let to = [followers.as_str()];
    let document = move_json(&TargetedActivityContext {
        config: &state.config,
        id: new_id(),
        actor_ap_id: &moved.ap_id,
        object: &moved.ap_id,
        target: &new.ap_id,
        to: &to,
        cc: &[],
        published: &now,
    });
    queue_activity(state, &moved, &document, "account move").await;
    after_profile_updated(state, &moved).await;
}

/// Announce the pin the caller just stored or withdrew.
///
/// The document is Pleroma's `Builder.pin` / `Builder.unpin`: [`add_json`]
/// or [`remove_json`] with the pinned status' uri as `object` and the
/// author's featured collection as `target`, `to` the public and the
/// author's followers collection in `cc` — the fan-out that reaches the
/// REMOTE followers, whose instances mirror the profile the feature rides.
/// A local actor mirrors nothing and announces nothing; a gone author or a
/// gone status has no document to build. (Only `public` and `unlisted`
/// posts can be pinned at all — the endpoint's gate, not checked here.)
pub async fn after_pin_changed(state: &AppState, pinned: &PinnedStatus, pinning: bool) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(actor) = load_user(state, pinned.user_id).await else {
        return; // an author whose row is gone has no collection to move
    };
    if !actor.local.get() {
        return; // a mirrored pin belongs to its own instance
    }
    let Some(pinned_status) = load_status(state, pinned.status_id).await else {
        return; // a gone status leaves no feature to announce
    };
    let followers = followers_address(&actor);
    let featured_collection = featured_address(&actor);
    let to = [PUBLIC];
    let cc = [followers.as_str()];
    let ctx = TargetedActivityContext {
        config: &state.config,
        id: new_id(),
        actor_ap_id: &actor.ap_id,
        object: &pinned_status.uri,
        target: &featured_collection,
        to: &to,
        cc: &cc,
        published: &pinned.inserted_at,
    };
    let document = if pinning {
        add_json(&ctx)
    } else {
        remove_json(&ctx)
    };
    queue_activity(state, &actor, &document, "pin").await;
}

/// The address naming the actor's featured (pinned) collection: the stored
/// `featured_url` — the very url a peer's `Add`/`Remove` handler matches
/// the `target` against — with the [`featured`] construction behind a row
/// that carries none, the way [`followers_address`] treats its column.
///
/// [`featured`]: crate::models::user::featured
fn featured_address(actor: &User) -> String {
    actor
        .featured_url
        .clone()
        .unwrap_or_else(|| featured(&actor.ap_id))
}

/// The url of a list's curated inbox: the endpoint its owner's instance
/// keeps for the list, where a member's instance posts what joins the
/// curation. It is this instance's own url — the list lives here — and it
/// rides in the announced [`Add`] / [`Remove`] as the `target` that tells
/// the member's instance where to send.
///
/// [`Add`]: crate::federation::activities::add_json
/// [`Remove`]: crate::federation::activities::remove_json
fn list_inbox_address(config: &Config, list_id: Uuid) -> String {
    format!("{}/api/v1/lists/{list_id}/inbox", config.base_url)
}

/// Announce a REMOTE account's joining or leaving one of the caller's
/// lists.
///
/// The document is [`add_json`] or [`remove_json`]: `actor` the list's
/// owner (whose write it was), `object` the member's actor id and `target`
/// the list's curated inbox (`list_inbox_address`), so the member's
/// instance learns both the membership and where the curation is asked to
/// arrive. The addressing is the member alone: `to` names their actor id,
/// which the publisher resolves to their personal inbox, and `cc` is
/// empty, so the owner's followers never hear about someone joining a
/// private curation.
///
/// A LOCAL member announces nothing: their timelines already hold the
/// list's effect, the way every local pair in this module does. As every
/// hook here the fan-out is queued, never awaited, and an enqueue that
/// fails is logged, never returned: the membership write already stands.
pub async fn after_list_member_changed(
    state: &AppState,
    owner: &User,
    list_id: Uuid,
    member: &User,
    adding: bool,
) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !owner.local.get() || member.local.get() {
        return; // only our own writes about a foreign member are news
    }
    let curated_inbox = list_inbox_address(&state.config, list_id);
    let to = [member.ap_id.as_str()];
    let ctx = TargetedActivityContext {
        config: &state.config,
        id: new_id(),
        actor_ap_id: &owner.ap_id,
        object: &member.ap_id,
        target: &curated_inbox,
        to: &to,
        cc: &[],
        published: &chrono::Utc::now(),
    };
    let document = if adding {
        add_json(&ctx)
    } else {
        remove_json(&ctx)
    };
    queue_activity(state, owner, &document, "list membership").await;
}

/// Announce the account whose deletion sweep is running.
///
/// The `Delete` names the actor as both `actor` and `object` — the peer's
/// cue to tombstone the account it mirrors — and is addressed to the public
/// with the followers collection in `cc`. Its id is the deterministic
/// `{BASE_URL}/activities/delete-user-{user id}`: the account's row id is
/// unique and never reused, so every (re)run of the sweep announces the
/// same activity id and the per-inbox delivery dedup swallows a retry.
///
/// The recipients are the snapshot taken *now* rather than an expansion the
/// delivery job computes later: the sweep clears the account's follow rows
/// right after this returns, and a `federator.deliver` job cannot re-query
/// a graph that is gone. [`after_account_deleted`] therefore lists the
/// remote followers' and remote followees' inboxes itself and hands the set
/// to the publisher as `extra_inboxes`, so both sides of every relationship
/// the account had learn it is gone.
pub async fn after_account_deleted(state: &AppState, account: &User) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !account.local.get() {
        return; // only our own actors announce themselves
    }
    let mut recipients: Vec<String> = Vec::new();
    match follow::remote_followers(&state.db, account.id).await {
        Ok(followers) => recipients.extend(followers.iter().filter_map(deliverable_inbox)),
        Err(err) => {
            warn!(%err, actor = %account.ap_id, "could not read the followers for Delete federation");
        }
    }
    match follow::remote_followees(&state.db, account.id).await {
        Ok(followees) => recipients.extend(followees.iter().filter_map(deliverable_inbox)),
        Err(err) => {
            warn!(%err, actor = %account.ap_id, "could not read the followees for Delete federation");
        }
    }
    if recipients.is_empty() {
        return; // nobody on another instance ever knew this account
    }
    // Two remote accounts on one instance name one shared inbox; the
    // snapshot POSTs there once.
    recipients.sort_unstable();
    recipients.dedup();
    let document = delete_document(state, account);
    match publisher::publish_in_background(state, account.id, &document, &recipients, None).await {
        Ok(()) => debug!(
            actor = %account.ap_id,
            inboxes = recipients.len(),
            "queued account Delete federation"
        ),
        Err(err) => {
            warn!(%err, actor = %account.ap_id, "could not queue account Delete federation");
        }
    }
}

/// The account `Delete` activity: the actor id as both `actor` and
/// `object`, addressed to the public with the followers collection in `cc`,
/// under the deterministic id the ticket pins.
fn delete_document(state: &AppState, account: &User) -> Value {
    let id = format!(
        "{}/activities/delete-user-{}",
        state.config.base_url, account.id
    );
    let followers = account
        .followers_url
        .clone()
        .unwrap_or_else(|| format!("{}/followers", account.ap_id));
    json!({
        "@context": note::json_ld_context(&state.config),
        "id": id,
        "type": "Delete",
        "actor": account.ap_id,
        "object": account.ap_id,
        "to": [PUBLIC],
        "cc": [followers],
    })
}

/// The inbox to deliver to one remote account: its shared (instance-wide)
/// inbox when it publishes one, its personal inbox otherwise — the
/// publisher's own [`best_inbox`] rule, applied by the `Delete` snapshot
/// while the rows still exist. An account with no inbox at all answers
/// `None`; refused and local-origin inboxes are filtered by the publisher
/// when the queued fan-out runs.
///
/// [`best_inbox`]: crate::federation::delivery::best_inbox
fn deliverable_inbox(account: &User) -> Option<String> {
    let inbox = account
        .shared_inbox
        .as_deref()
        .or(account.inbox.as_deref())?;
    (!inbox.trim().is_empty() && inbox.starts_with("http")).then(|| inbox.to_owned())
}

/// The `disabled` landing hook: a deactivated account publishes nothing.
///
/// An account switched off with `POST /api/pleroma/disable_account` keeps
/// its actor document resolvable — its posts and follows stay visible in
/// other people's timelines — so nothing is announced. Pleroma's
/// `User.deactivate` federates nothing either. The function exists as the
/// single place a future ticket changes that answer, and at the call site
/// that states the intent.
pub const fn after_account_disabled(_state: &AppState, _account: &User) {}

/// Forward the report `POST /api/v1/reports` (or the inbound `Flag`
/// handler) just filed, when the reporter asked for it and the target
/// lives on another instance.
///
/// The document is [`flag_json`]'s, Pleroma's `Flag` reduced to the fields
/// the ticket pins: the INSTANCE actor speaks it (the reporter's own
/// instance vouches on the wire, which is who the target's moderators must
/// hear from), `object` names the reported account first and the cited
/// live statuses' object uris after it, and `content` carries the
/// reporter's comment. Its id is the report row's stored `ap_id` (decision
/// 6 — the id [`report::create`] minted for exactly this document), so the
/// per-inbox delivery dedup swallows a repeat filing's redelivery.
///
/// Nothing goes out when the reporter asked for no forwarding or the
/// target is one of our own accounts — those facts stay with our
/// moderators — and the audience of a forwarded remote one is the target's
/// inbox alone: `to` names the target's actor id, which the publisher
/// resolves to their personal inbox, and the fan-out never touches
/// followers (the publishing actor is the instance actor anyway).
pub async fn after_report_created(state: &AppState, report: &ReportRow) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !report.forwarded {
        return; // an unforwarded filing is this instance's own business
    }
    let Some(stored_id) = report.ap_id.as_deref() else {
        return; // an id-less row has no document to stand behind
    };
    let Some(target) = load_user(state, report.target_id).await else {
        return; // a gone target is nobody to report to
    };
    if target.local.get() {
        return; // a local target's moderators already have the row
    }
    let Some(instance_actor) = forwarding_actor(state).await else {
        return; // without the instance's own actor there is no signer
    };
    let object_ids = flag_object_ids(state, &target, report.id).await;
    let to = [target.ap_id.as_str()];
    let document = flag_json(&FlagContext {
        config: &state.config,
        id: stored_id,
        actor_ap_id: &instance_actor.ap_id,
        object_ids: &object_ids,
        content: &report.comment,
        to: &to,
        cc: &[],
        published: &report.inserted_at,
    });
    queue_activity(state, &instance_actor, &document, "report forward").await;
}

/// The instance actor's row, provisioned on first use: the account a
/// forwarded report is attributed to and signed as. A failure to read or
/// create it is logged and answers `None` — the filing stands, this
/// forward does not go out.
async fn forwarding_actor(state: &AppState) -> Option<User> {
    instance_actor::ensure_instance_actor(&state.db, &state.config)
        .await
        .map_err(|err| {
            warn!(%err, "could not provision the instance actor for a forwarded report");
        })
        .ok()
}

/// The `Flag` document's `object` array: the reported account's actor id
/// first, then the cited statuses' object uris, oldest first (the order
/// [`report::statuses_for`] keeps, which [`status::get_many`]’s id order
/// preserves). A citation deleted between the filing and the fan-out
/// drops out on its own; a read that fails is logged and leaves the array
/// naming the account alone — the report itself is worth forwarding even
/// with no context beside it.
async fn flag_object_ids(state: &AppState, target: &User, report_id: Uuid) -> Vec<String> {
    let mut objects = vec![target.ap_id.clone()];
    let cited = report::statuses_for(&state.db, report_id)
        .await
        .unwrap_or_else(|err| {
            warn!(%err, %report_id, "federation hook could not read the report's citations");
            Vec::new()
        });
    if cited.is_empty() {
        return objects;
    }
    let rows = status::get_many(&state.db, &cited)
        .await
        .unwrap_or_else(|err| {
            warn!(%err, %report_id, "federation hook could not read the cited statuses");
            Vec::new()
        });
    objects.extend(rows.into_iter().map(|row| row.uri));
    objects
}

/// Announce the chat message the admin API just withdrew from both sides.
///
/// The document is Pleroma's `Delete` of a `ChatMessage`: the message's own
/// sender speaks it (the actor that signed its `Create`, and the shape the
/// receiving sender-gate reads), `object` embeds the stored id under its
/// `ChatMessage` type — the key a peer's dispatcher routes on — and `to`
/// names the chat's other participant, the one peer that could ever have
/// received the message. Like every hook here the fan-out is queued, never
/// awaited: the tombstone already stands, and a federation problem must not
/// answer the moderator's `DELETE` with anything but its 200.
///
/// Silence is the answer of a message that never left this instance to be
/// taken back: federation switched off, no stored `ap_id` (a local post the
/// outgoing chat pipeline has never announced), an id belonging to another
/// instance (whose own origin withdraws it), a gone or non-local sender, and
/// a chat whose other side is a local account — that pair's copies are both
/// this database.
pub async fn after_chat_message_deleted_by_admin(
    state: &AppState,
    chat_row: &chat::Chat,
    message: &ChatMessageReference,
) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(object_ap_id) = message.ap_id.as_deref() else {
        return; // a message that never carried an id was never announced
    };
    if !is_local_object_id(&state.config, object_ap_id) {
        return; // a remote object's withdrawal belongs to its own instance
    }
    let Some(sender) = load_user(state, message.sender_id).await else {
        return; // a gone sender has no identity to speak as
    };
    if !sender.local.get() {
        return; // only our own actors announce their messages
    }
    let other_id = if sender.id == chat_row.user_id {
        chat_row.recipient_id
    } else {
        chat_row.user_id
    };
    let Some(other) = load_user(state, other_id).await else {
        return; // the other participant is gone; nobody stands to hear
    };
    if other.local.get() {
        return; // a local pair is settled in the database
    }
    let document = chat_delete_json(
        &state.config,
        new_id(),
        &sender.ap_id,
        object_ap_id,
        &other.ap_id,
    );
    queue_activity(state, &sender, &document, "chat message withdrawal").await;
}

/// Whether `ap_id` names an object this instance minted: the
/// `{BASE_URL}/objects/` prefix every locally owned document carries
/// (decision 6). Any other id is a remote mirror's own object.
fn is_local_object_id(config: &Config, ap_id: &str) -> bool {
    ap_id.starts_with(&format!("{}/objects/", config.base_url))
}

/// The chat message `Delete` document: a fresh `{BASE_URL}/activities/{uuid}`
/// id (decision 6), the sender as `actor`, the withdrawn object embedded
/// under the type the receiver's dispatcher routes on, and the remote
/// participant alone in `to` — a chat is direct by nature, so no followers
/// and no public address ever appear on it.
fn chat_delete_json(
    config: &Config,
    id: Uuid,
    actor_ap_id: &str,
    object_ap_id: &str,
    recipient_ap_id: &str,
) -> Value {
    json!({
        "@context": note::json_ld_context(config),
        "id": activity_id(config, id),
        "type": "Delete",
        "actor": actor_ap_id,
        "to": [recipient_ap_id],
        "object": { "id": object_ap_id, "type": "ChatMessage" },
    })
}

/// The parts one federated answer document of a remote poll is built from:
/// the ballot as [`after_poll_vote`] resolved it — the poll the marks count
/// on (whose row id names the answer ids), the REMOTE question they answer,
/// and the two actor ends of the telling. Read in as plain borrows so the
/// builder stays pure, the way
/// [`ObjectActivityContext`](crate::federation::activities::ObjectActivityContext)
/// is for the interaction documents.
#[derive(Debug)]
struct Ballot<'a> {
    /// The poll's own row id: the `{vote id}` of the pinned answer ids.
    poll_id: Uuid,
    /// The question's object uri — the answers' `inReplyTo`.
    question_uri: &'a str,
    /// The voting account's actor id: `actor` and `attributedTo`.
    voter_ap_id: &'a str,
    /// The poll author's actor id: the answer's only addressee.
    author_ap_id: &'a str,
}

/// The `Create` of one answer: the bodyless `Note` naming `option`'s title
/// and answering the question behind `ballot`, under the deterministic
/// `{BASE_URL}/objects/vote-{vote id}-{option position}` object id and the
/// matching `{BASE_URL}/activities/vote-{vote id}-{option position}`
/// activity id the ticket pins (`{vote id}` is the poll's own row id, which
/// is exactly what `POST /api/v1/polls/{id}/votes` is keyed on).
///
/// The shape is the one this instance's own inbound fold
/// ([`crate::federation::handlers::vote`]) reads back: a `name`, no
/// `content`, an `inReplyTo` naming the question and an `attributedTo` of
/// the voter. The object's `to` names the poll's author alone and its `cc`
/// is empty — the answer is a mark on somebody's poll, not a post on the
/// voter's timeline — and the envelope repeats that one-address line so the
/// fan-out resolves exactly the author's personal inbox.
fn answer_document(state: &AppState, ballot: &Ballot<'_>, option: &PollOption) -> Value {
    let name = format!("vote-{}-{}", ballot.poll_id, option.position);
    let object = json!({
        "id": format!("{}/objects/{name}", state.config.base_url),
        "type": "Note",
        "attributedTo": ballot.voter_ap_id,
        "inReplyTo": ballot.question_uri,
        "name": option.title,
        "to": [ballot.author_ap_id],
        "cc": [],
    });
    json!({
        "@context": note::json_ld_context(&state.config),
        "id": format!("{}/activities/{name}", state.config.base_url),
        "type": "Create",
        "actor": ballot.voter_ap_id,
        "to": [ballot.author_ap_id],
        "cc": [],
        "object": object,
    })
}

/// Announce the ballot just cast on a REMOTE status's poll: one `Create`
/// per chosen option, all delivered to the poll author's inbox alone.
///
/// Pleroma's `CommonAPI.vote` answers a remote question by running each
/// choice through its pipeline as an `Answer`, and the relational ballot
/// here mints one answer document per mark instead (the documents' own
/// `to` names the author, so the follower of nobody's timeline is
/// expanded). Silence is the answer beside [`crate::services::notify`]'s
/// telling in every other case: a poll on a LOCAL status keeps its ballots
/// inside this instance (its author reads the counters, not the wire), a
/// gone status or author has nobody to answer, a non-local voter's ballot
/// arrived from the wire and goes no further, and an option id the poll
/// does not hold names nothing to announce.
pub async fn after_poll_vote(state: &AppState, poll: &PollRow, voter: &User, option_ids: &[Uuid]) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if option_ids.is_empty() || !voter.local.get() {
        return; // only our own actors sign an answer
    }
    let Some(posted) = load_status(state, poll.status_id).await else {
        return; // a question whose row is gone has no author to answer
    };
    if posted.local {
        return; // a local poll's ballot is this instance's own fact
    }
    let Some(author) = load_user(state, posted.user_id).await else {
        return; // an author whose row is gone is nobody to tell
    };
    let options = match poll::options(&state.db, poll.id).await {
        Ok(options) => options,
        Err(err) => {
            warn!(
                %err,
                poll = %poll.id,
                "federation hook could not read the poll's options"
            );
            return;
        }
    };
    let ballot = Ballot {
        poll_id: poll.id,
        question_uri: &posted.uri,
        voter_ap_id: &voter.ap_id,
        author_ap_id: &author.ap_id,
    };
    for option_id in option_ids {
        let Some(option) = options.iter().find(|row| row.id == *option_id) else {
            continue; // a mark on no stored option federates no answer
        };
        let document = answer_document(state, &ballot, option);
        queue_activity(state, voter, &document, "poll answer").await;
    }
}

/// The delivered `Follow` a local followee answers: the activity's own id,
/// which the answer embeds, and the follower's actor id, which is the
/// answer's sole recipient.
#[derive(Debug)]
pub struct FollowAnswer<'a> {
    /// The `Follow` activity's `ActivityPub` id — embedded verbatim in the
    /// answer, because Mastodon matches an `Accept` by the activity it
    /// carries, not by a reference it would have to fetch.
    pub follow_id: &'a str,
    /// The follower's actor id — the delivered `Follow`'s own `actor`.
    pub follower_ap_id: &'a str,
}

/// Answer a remote account's follow of `local_followee` with an `Accept`.
///
/// The document is [`accept_json`]'s: a fresh `{BASE_URL}/activities/{uuid}`
/// id (decision 6), `actor` the followee, `to` the follower, and the
/// delivered `Follow` embedded as `{id, type, actor, object}`. The fan-out
/// runs [`publisher::publish`] right here — this caller is the incoming
/// dispatcher's own job, and `only_to` is the follower, so the answer lands
/// in their personal inbox alone (the followee's other followers have no
/// business with who follows them). Returns how many deliveries were
/// queued: `0` for a follower nothing can be delivered to, and for an
/// instance with federation disabled.
///
/// # Errors
///
/// Any database failure behind the fan-out's recipient reads, or a failed
/// delivery-job insert, arrives as
/// [`AppError::Database`](crate::error::AppError) — the dispatcher's retry
/// runs the idempotent fold again.
pub async fn accept_follow(
    state: &AppState,
    local_followee: &User,
    follow: &FollowAnswer<'_>,
) -> AppResult<usize> {
    answer_follow(state, local_followee, follow, true).await
}

/// Refuse a remote account's follow of `local_followee` with a `Reject`.
///
/// [`accept_follow`]'s refusal twin: same document shape under
/// [`reject_json`]'s type and the same one-recipient fan-out. The inbound
/// handler runs it for a pair blocked either way, before anything of the
/// follow is stored.
///
/// # Errors
///
/// Those of [`accept_follow`].
pub async fn reject_follow(
    state: &AppState,
    local_followee: &User,
    follow: &FollowAnswer<'_>,
) -> AppResult<usize> {
    answer_follow(state, local_followee, follow, false).await
}

/// The shared body of [`accept_follow`] and [`reject_follow`]: the embedded
/// `Follow`, the answer document, the one-recipient publish.
async fn answer_follow(
    state: &AppState,
    local_followee: &User,
    follow: &FollowAnswer<'_>,
    accepted: bool,
) -> AppResult<usize> {
    let embedded = json!({
        "id": follow.follow_id,
        "type": "Follow",
        "actor": follow.follower_ap_id,
        "object": local_followee.ap_id,
    });
    let published = chrono::Utc::now();
    let to = [follow.follower_ap_id];
    let answer = AnsweredActivity {
        config: &state.config,
        id: new_id(),
        actor_ap_id: &local_followee.ap_id,
        to: &to,
        cc: &[],
        answered: &embedded,
        published: &published,
    };
    let document = if accepted {
        accept_json(&answer)
    } else {
        reject_json(&answer)
    };
    let opts = PublishOpts {
        extra_inboxes: Vec::new(),
        only_to: Some(vec![follow.follower_ap_id.to_owned()]),
    };
    publisher::publish(state, local_followee, &document, opts).await
}

/// Announce the follow or unfollow the caller just performed.
///
/// Only a REMOTE followee is worth announcing (a local one is settled in
/// our own database) and only a local follower's activity may be signed.
/// `following` selects the direction: the row's own `Follow` —
/// [`follow_json`] under the stored `ap_id`, the follower as `actor`, the
/// followee as `object` and as the only `to` address — or, for the
/// withdrawal, the `Undo` wrapping that very document under a fresh
/// `{BASE_URL}/activities/{uuid}` id (decision 6), Pleroma's
/// `ActivityPub.follow`/`unfollow` pair. [`publish`] is then run with
/// `only_to` naming the followee, so the activity lands in their personal
/// inbox alone and the fan-out never touches the follower's own remote
/// followers.
///
/// A restated follow — the endpoint's idempotent repeat of an open pair, an
/// import re-run — re-sends the same activity id, and the per-activity,
/// per-inbox deduplication of the delivery queue keeps the restatement a
/// single sending. A pair with no row (a withdrawal that found nothing to
/// take back) and a row with no `ap_id` announce nothing.
///
/// [`publish`]: crate::federation::publisher::publish
pub async fn after_follow_changed(
    state: &AppState,
    follower: &User,
    target: &User,
    follow: Option<&Follow>,
    following: bool,
) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if target.local.get() || !follower.local.get() {
        return; // a local pair is settled in the database; only our actors sign
    }
    let Some(document) = follow_announcement(state, follower, target, follow, following) else {
        return; // no row, or an id-less one: there is nothing to announce
    };
    publish_only_to(state, follower, &target.ap_id, &document, "follow").await;
}

/// The document [`after_follow_changed`] announces: the row's own `Follow`
/// when the write opened (or restated) the relationship, and the `Undo`
/// wrapping that document — a fresh id, the withdrawal's own moment — when
/// it took the follow back. `None` when the write left no row, or the row
/// carries no id to stand behind the document.
fn follow_announcement(
    state: &AppState,
    follower: &User,
    target: &User,
    follow: Option<&Follow>,
    following: bool,
) -> Option<Value> {
    let row = follow?;
    let document = follow_document(state, row, follower, target)?;
    if following {
        return Some(document);
    }
    let now = chrono::Utc::now();
    let to = [target.ap_id.as_str()];
    Some(undo_json(&AnsweredActivity {
        config: &state.config,
        id: new_id(),
        actor_ap_id: &follower.ap_id,
        to: &to,
        cc: &[],
        answered: &document,
        published: &now,
    }))
}

/// The `Follow` behind the relationship row `row`: [`follow_json`] with its
/// `id` replaced by the stored string — the builder mints ids from a fresh
/// uuid and the row's `ap_id` is its own (decision 6), so only the stored
/// text names the activity the peer was sent and must hear withdrawn.
/// `None` when the row carries no id at all (a mirrored follow, which this
/// instance does not own).
fn follow_document(
    state: &AppState,
    row: &Follow,
    follower: &User,
    target: &User,
) -> Option<Value> {
    let stored_id = row.ap_id.as_deref()?;
    let mut document = follow_json(
        &state.config,
        row.id,
        &follower.ap_id,
        &target.ap_id,
        &row.inserted_at,
    );
    if let Some(fields) = document.as_object_mut() {
        let _previous = fields.insert("id".to_owned(), json!(stored_id));
    }
    Some(document)
}

/// Answer the follow request `resolver` just resolved when the requester
/// lives on another instance.
///
/// An authorization is an [`accept_follow`], a refusal a [`reject_follow`],
/// both answering the delivered `Follow` by the id the request row stores
/// and travelling to the follower's inbox alone. A local requester's answer
/// is the row itself (their timeline is the delivery), and a request that
/// carries no delivered activity id has nothing to answer by. As every
/// endpoint hook here the answer is fire-and-forget: a failed fan-out
/// arrives in the log, never in the client's response — the resolution
/// itself already stands.
pub async fn after_follow_request_resolved(
    state: &AppState,
    resolver: &User,
    request: &Follow,
    granted: bool,
) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !resolver.local.get() {
        return; // only our own actors answer their follow requests
    }
    let Some(follower) = load_user(state, request.follower_id).await else {
        return; // a requester whose row is gone is nobody to answer
    };
    answer_follow_request(state, resolver, &follower, request, granted).await;
}

/// Refuse a REMOTE follower whose accepted follow of the local `target` was
/// just forced out of the table by [`remove_follower`].
///
/// The document is the `Reject` [`reject_follow`] publishes for the pair,
/// answering the delivered `Follow` by its stored id. A local follower, a
/// gone row and an id-less request answer with silence, as in
/// [`after_follow_request_resolved`].
///
/// [`remove_follower`]: crate::models::follow::remove_follower
pub async fn after_follower_removed(state: &AppState, target: &User, removed: &Follow) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !target.local.get() {
        return; // only our own actors answer their followers
    }
    let Some(follower) = load_user(state, removed.follower_id).await else {
        return; // a follower whose row is gone is nobody to refuse
    };
    answer_follow_request(state, target, &follower, removed, false).await;
}

/// The shared body of [`after_follow_request_resolved`] and
/// [`after_follower_removed`]: the REMOTE gate on the requester, the
/// delivered activity's stored id, and the answer [`accept_follow`] /
/// [`reject_follow`] publish to the follower's inbox alone — logged at both
/// outcomes, since the endpoint's write never waits on the wire.
async fn answer_follow_request(
    state: &AppState,
    target: &User,
    follower: &User,
    request: &Follow,
    granted: bool,
) {
    if follower.local.get() {
        return; // a local pair is settled in the database
    }
    let Some(follow_id) = request.ap_id.as_deref() else {
        return; // no delivered activity to answer by its id
    };
    let answer = FollowAnswer {
        follow_id,
        follower_ap_id: &follower.ap_id,
    };
    let outcome = if granted {
        accept_follow(state, target, &answer).await
    } else {
        reject_follow(state, target, &answer).await
    };
    match outcome {
        Ok(count) => debug!(
            actor = %target.ap_id,
            follower = %follower.ap_id,
            granted,
            count,
            "queued follow-request answer"
        ),
        Err(err) => warn!(
            %err,
            actor = %target.ap_id,
            follower = %follower.ap_id,
            "could not queue follow-request answer"
        ),
    }
}

/// One chat message as the REMOTE counterpart must see it: the stored
/// view ([`ChatMessageReference`]'s message columns), its reserved object
/// id, and the two actor ends of the pair. Grouped so the two builders
/// below stay inside the parameter budget, the way [`Ballot`] does for the
/// poll answers.
#[derive(Debug)]
struct ChatDelivery<'a> {
    /// The message's reserved `chat_messages.ap_id` object id (decision 6).
    ap_id: &'a str,
    /// The stored message: its id names the activity, its text, attachment
    /// and creation moment fill the object.
    message: &'a ChatMessageReference,
    /// The sending account's actor id: `actor` and `attributedTo`.
    sender_ap_id: &'a str,
    /// The remote counterpart's actor id: the message's only recipient.
    recipient_ap_id: &'a str,
}

/// The delivered message's attachment row, read for the federated document
/// — `None` when the message carries none or the read fails (a message
/// whose media cannot be read still reaches its recipient as text).
async fn chat_attachment(
    state: &AppState,
    message: &ChatMessageReference,
) -> Option<MediaAttachment> {
    let media_id = message.attachment_media_id?;
    match media_attachment::get_by_id(&state.db, media_id).await {
        Ok(row) => row,
        Err(err) => {
            warn!(
                %err,
                media = %media_id,
                "federation hook could not read the chat attachment"
            );
            None
        }
    }
}

/// The `ChatMessage` object the `Create` carries: the reserved id, the
/// sender as `attributedTo`, the recipient as the only `to` address, the
/// stored HTML, and the message's own moment as `published` — the fields
/// the ticket pins, with the first media riding as `attachment` when the
/// message carries one (Pleroma's `Builder.chat_message`, with the
/// object's `attributedTo` spelling this instance's inbound fold — and
/// decision 6's addressing — read).
fn chat_message_object(delivery: &ChatDelivery<'_>, attachment: Option<&MediaAttachment>) -> Value {
    let mut object = json!({
        "id": delivery.ap_id,
        "type": "ChatMessage",
        "attributedTo": delivery.sender_ap_id,
        "to": [delivery.recipient_ap_id],
        "content": delivery.message.content,
        "published": mastodon_time(&delivery.message.message_inserted_at),
    });
    if let (Some(media), Some(fields)) = (attachment, object.as_object_mut()) {
        let _previous = fields.insert("attachment".to_owned(), note::attachment_json(media));
    }
    object
}

/// The `Create` of a chat message posted to a REMOTE counterpart: the
/// object above under the deterministic `{BASE_URL}/activities/{message id}`
/// id (decision 6 — the same pair rule as a status' `Create`), `actor` the
/// sender, and the counterpart as the only address `to` names so the
/// fan-out resolves exactly their personal inbox.
fn chat_create_document(
    state: &AppState,
    delivery: &ChatDelivery<'_>,
    attachment: Option<&MediaAttachment>,
) -> Value {
    json!({
        "@context": note::json_ld_context(&state.config),
        "id": format!(
            "{}/activities/{}",
            state.config.base_url, delivery.message.message_id
        ),
        "type": "Create",
        "actor": delivery.sender_ap_id,
        "to": [delivery.recipient_ap_id],
        "cc": [],
        "object": chat_message_object(delivery, attachment),
    })
}

/// The `Delete` withdrawing a chat message the sender took back: the
/// message's reserved object id named as a `{id, type}` `ChatMessage`
/// reference (the shape the inbound fold's `delete_by_object_type` route
/// reads — a bare-id `Delete` declares no type and would fall past it),
/// under the deterministic `{BASE_URL}/activities/delete-{message id}` id
/// and the same one-address line as the `Create` it takes back.
fn chat_delete_document(state: &AppState, delivery: &ChatDelivery<'_>) -> Value {
    json!({
        "@context": note::json_ld_context(&state.config),
        "id": format!(
            "{}/activities/delete-{}",
            state.config.base_url, delivery.message.message_id
        ),
        "type": "Delete",
        "actor": delivery.sender_ap_id,
        "to": [delivery.recipient_ap_id],
        "cc": [],
        "object": { "id": delivery.ap_id, "type": "ChatMessage" },
    })
}

/// Announce the chat message just posted to a REMOTE counterpart: the
/// `Create{ChatMessage}` Pleroma's `post_chat_message` pipeline sends.
///
/// The message is delivered to the counterpart's inbox alone — a chat is
/// the pair's own conversation, so the follower of nobody's timeline is
/// expanded and no other address hears it. Silence in every other case: a
/// LOCAL recipient is told by their rows, never over HTTP; a remote
/// sender's message arrived from the wire (the inbound fold lands it and
/// stops); an id-less message has no object to name; and a recipient whose
/// `accepts_chat_messages` flag is down is not delivered to — opening the
/// pair checked the flag, and a flag since lowered closes the door again.
pub async fn after_chat_message_created(
    state: &AppState,
    message: &ChatMessageReference,
    sender: &User,
    recipient: &User,
) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if recipient.local.get() || !sender.local.get() {
        return; // local sides are settled in the database; only our actors sign
    }
    if !recipient.accepts_chat_messages.get() {
        return;
    }
    let Some(ap_id) = message.ap_id.as_deref() else {
        return; // an id-less message has no object to stand behind
    };
    let attachment = chat_attachment(state, message).await;
    let delivery = ChatDelivery {
        ap_id,
        message,
        sender_ap_id: &sender.ap_id,
        recipient_ap_id: &recipient.ap_id,
    };
    let document = chat_create_document(state, &delivery, attachment.as_ref());
    queue_activity(state, sender, &document, "chat message").await;
}

/// Announce the chat message its SENDER just withdrew from a REMOTE
/// counterpart: the [`after_chat_message_created`] counterpart, a `Delete`
/// of the same reserved object id.
///
/// `user` is the account whose delete this was and `other` the chat's
/// counterpart: only the sender's withdrawal federates — a participant who
/// merely took the message out of their own list withdraws nobody's copy —
/// and a LOCAL counterpart is again told by their rows alone.
pub async fn after_chat_message_deleted(
    state: &AppState,
    message: &ChatMessageReference,
    user: &User,
    other: &User,
) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if message.sender_id != user.id {
        return; // only the sender's delete takes the message off the wire
    }
    if other.local.get() || !user.local.get() {
        return; // local sides are settled in the database; only our actors sign
    }
    let Some(ap_id) = message.ap_id.as_deref() else {
        return; // nothing was announced, so there is nothing to withdraw
    };
    let delivery = ChatDelivery {
        ap_id,
        message,
        sender_ap_id: &user.ap_id,
        recipient_ap_id: &other.ap_id,
    };
    let document = chat_delete_document(state, &delivery);
    queue_activity(state, user, &document, "chat message withdrawal").await;
}

/// Reads one user row for the hooks, answering `None` for a gone row or a
/// database that will not answer (a federation miss, never a failure).
async fn load_user(state: &AppState, id: Uuid) -> Option<User> {
    user::get_by_id(&state.db, id).await.unwrap_or_else(|err| {
        warn!(%err, %id, "federation hook could not read a user row");
        None
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL, Toggle},
        http_client::HttpClient,
        models::{
            block,
            chat::{self, NewChatMessage},
            emoji_reaction::{self, NewReaction},
            favourite, pinned_status,
            poll::{self, PollParams},
            status::NewStatus,
            user::{Flag, NewUser, insert, local_ap_id},
        },
        uploads::UploaderBackend,
        util::time::mastodon_time,
    };

    /// The test state over `pool` with the default (federating) config.
    fn app_state(pool: &PgPool) -> AppState {
        state_with(pool, Config::for_tests())
    }

    /// The test state over `pool` with an injected config.
    fn state_with(pool: &PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// The config with federation switched off.
    fn unfederated() -> Config {
        let mut config = Config::for_tests();
        config.federation_enabled = Toggle::new(false);
        config
    }

    /// A registered-looking local actor: keys, inbox, followers collection,
    /// exactly the row `create_local_user` writes.
    async fn local(pool: &PgPool, nickname: &str) -> User {
        let ap_id = local_ap_id(TEST_BASE_URL, nickname);
        insert(
            pool,
            &NewUser {
                private_key: Some("PEM".to_owned()),
                public_key: Some("PUB".to_owned()),
                inbox: Some(format!("{ap_id}/inbox")),
                followers_url: Some(format!("{ap_id}/followers")),
                ..NewUser::local(nickname, &ap_id)
            },
        )
        .await
        .expect("the local fixture inserts")
    }

    /// A remote mirror `nickname@remote.example`, shared inbox included.
    async fn remote(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id: format!("https://remote.example/users/{nickname}"),
                local: Flag::new(false),
                domain: Some("remote.example".to_owned()),
                inbox: Some(format!("https://remote.example/users/{nickname}/inbox")),
                shared_inbox: Some("https://remote.example/inbox".to_owned()),
                public_key: Some("PUB".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// `follower` follows `target` with an accepted relationship: a local
    /// target grants on the spot, a remote one is answered the way a peer's
    /// `Accept` would.
    async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
        let row = follow::follow(pool, TEST_BASE_URL, follower, target)
            .await
            .expect("the follow fixture writes");
        if row.state != follow::FollowState::Accepted {
            let _granted = follow::accept(pool, row.id)
                .await
                .expect("the fixture grants it");
        }
    }

    /// The queued `federator.publish` jobs: `(args, unique_key)` pairs.
    async fn publish_jobs(pool: &PgPool) -> Vec<(Value, Option<String>)> {
        sqlx::query_as::<_, (Value, Option<String>)>(
            "SELECT args, unique_key FROM jobs WHERE kind = $1 ORDER BY inserted_at",
        )
        .bind(publisher::KIND_PUBLISH)
        .fetch_all(pool)
        .await
        .expect("the jobs read")
    }

    /// How many jobs the database holds, of any kind.
    async fn job_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
            .fetch_one(pool)
            .await
            .expect("the job count")
    }

    /// The `Block` a local blocker wrote against a remote target publishes
    /// the stored activity id, addressed to the blocked actor alone, and
    /// nothing else is queued.
    #[tokio::test]
    async fn blocking_a_remote_account_publishes_the_stored_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the block writes");
        let state = app_state(&pool);

        after_block_changed(&state, &row, false).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the block is announced once");
        let (args, unique_key) = &jobs[0];
        assert!(unique_key.is_none(), "blocks never debounce");
        assert_eq!(args["actor_id"], alice.id.to_string());
        let activity = &args["activity"];
        assert_eq!(activity["type"], "Block");
        assert_eq!(
            activity["id"],
            row.ap_id.unwrap(),
            "the id is the stored blocks.ap_id, not a freshly minted one"
        );
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"], bob.ap_id);
        assert_eq!(
            activity["to"][0], bob.ap_id,
            "the blocked account is the only address"
        );
    }

    /// The undo publishes the same `Block` document wrapped in an `Undo`
    /// with its own fresh id; the inner document keeps the stored one.
    #[tokio::test]
    async fn unblocking_publishes_the_undo_of_the_stored_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the block writes");
        let stored_id = row.ap_id.clone().expect("a local block has an id");
        let state = app_state(&pool);

        after_block_changed(&state, &row, true).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1);
        let activity = &jobs[0].0["activity"];
        assert_eq!(activity["type"], "Undo");
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["to"][0], bob.ap_id);
        let id = activity["id"].as_str().expect("the undo has an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "the undo carries this instance's own activity shape: {id}"
        );
        assert_ne!(id, stored_id, "and it is a fresh id, not the block's");
        assert_eq!(activity["object"]["type"], "Block");
        assert_eq!(
            activity["object"]["id"], stored_id,
            "the embedded block keeps the stored id"
        );
    }

    /// A block of a LOCAL account announces nothing: the pair's instances
    /// are the same database, and the local target's timeline is the whole
    /// delivery.
    #[tokio::test]
    async fn blocking_a_local_account_federates_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the block writes");
        let state = app_state(&pool);

        after_block_changed(&state, &row, false).await;
        after_block_changed(&state, &row, true).await;

        assert!(
            publish_jobs(&pool).await.is_empty(),
            "a local pair publishes neither the block nor the undo"
        );
    }

    /// The WBE-181 fixtures of the disabled test: a REMOTE question with a
    /// two-option [`multiple`](PollRow::multiple) poll — answered with its
    /// row and the option ids one ballot marks — beside the sender's
    /// reference to a chat message of the `alice → bob` pair.
    async fn remote_ballot_and_chat(
        pool: &PgPool,
        alice: &User,
        bob: &User,
    ) -> (PollRow, Vec<Uuid>, ChatMessageReference) {
        let asked = remote_status(pool, bob, "question-disabled", Visibility::Public).await;
        let stand = {
            let mut conn = pool.acquire().await.expect("a slot connection");
            poll::create(
                &mut conn,
                asked.id,
                &PollParams {
                    options: vec!["tea".to_owned(), "coffee".to_owned()],
                    multiple: true,
                    ..PollParams::default()
                },
            )
            .await
            .expect("the poll creates")
        };
        let marks: Vec<Uuid> = poll::options(pool, stand.id)
            .await
            .expect("the options read")
            .iter()
            .map(|option| option.id)
            .collect();
        let ballot = {
            let mut tx = pool.begin().await.expect("a transaction begins");
            let (_, reference, _) = chat::add_message(
                &mut tx,
                TEST_BASE_URL,
                alice,
                bob,
                &NewChatMessage::text("unheard"),
            )
            .await
            .expect("the chat message writes");
            tx.commit().await.expect("the transaction commits");
            reference
        };
        (stand, marks, ballot)
    }

    /// With `FEDERATION_ENABLED` off the hooks never touch the queue.
    #[tokio::test]
    async fn a_disabled_instance_federates_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the block writes");
        let liked = remote_status(&pool, &bob, "post-disabled", Visibility::Public).await;
        let fav = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let new = NewReaction {
            user_id: alice.id,
            status_id: liked.id,
            emoji: "🎉",
            url: None,
        };
        let reaction = emoji_reaction::react(&pool, TEST_BASE_URL, &new)
            .await
            .expect("the reaction writes");
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &liked, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let own = local_status(&pool, &alice, "pin me anyway").await;
        let pin = pinned_status::pin(&pool, alice.id, own.id)
            .await
            .expect("the pin writes")
            .expect("the pair pinned");
        let report_row = filed(&pool, &alice, &bob, &filing("unheard", true, Vec::new())).await;
        let (stand, marks, ballot) = remote_ballot_and_chat(&pool, &alice, &bob).await;
        let state = state_with(&pool, unfederated());

        after_status_created(&state, &own).await;
        after_status_edited(&state, &own).await;
        after_status_deleted(&state, &own).await;
        after_block_changed(&state, &row, false).await;
        after_profile_updated(&state, &alice).await;
        after_account_deleted(&state, &alice).await;
        after_favourite_changed(&state, &fav, true).await;
        after_reaction_changed(&state, &reaction, true).await;
        after_reblog_changed(&state, &reblog, true).await;
        after_status_deleted(&state, &reblog).await;
        after_account_moved(&state, &alice, &bob).await;
        after_pin_changed(&state, &pin, true).await;
        after_report_created(&state, &report_row).await;
        let stranger = remote(&pool, "stranger").await;
        let following = follow::follow(&pool, TEST_BASE_URL, &alice, &stranger)
            .await
            .expect("the follow fixture writes");
        after_follow_changed(&state, &alice, &stranger, Some(&following), true).await;
        after_follow_changed(&state, &alice, &stranger, Some(&following), false).await;
        after_follow_request_resolved(&state, &alice, &following, true).await;
        after_follower_removed(&state, &alice, &following).await;
        after_poll_vote(&state, &stand, &alice, &marks).await;
        after_chat_message_created(&state, &ballot, &alice, &bob).await;
        after_chat_message_deleted(&state, &ballot, &alice, &bob).await;

        assert!(
            publish_jobs(&pool).await.is_empty(),
            "an instance that does not federate queues nothing"
        );
        assert_eq!(
            job_count(&pool).await,
            0,
            "not even a follow-side delivery job"
        );
    }

    /// A poll on a LOCAL status keeps its ballots inside the database: the
    /// author's counters are the whole telling, and no answer goes out —
    /// beside the other refusals of the hook: a ballot signed by nobody of
    /// ours, and marks naming options the poll does not hold.
    #[tokio::test]
    async fn a_ballot_this_instance_has_no_answer_to_send_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let state = app_state(&pool);

        let own = local_status(&pool, &alice, "our own question").await;
        let local_stand = {
            let mut conn = pool.acquire().await.expect("a slot connection");
            poll::create(
                &mut conn,
                own.id,
                &PollParams {
                    options: vec!["tea".to_owned()],
                    ..PollParams::default()
                },
            )
            .await
            .expect("the local poll creates")
        };
        let own_options = poll::options(&pool, local_stand.id)
            .await
            .expect("the options read");
        after_poll_vote(&state, &local_stand, &alice, &[own_options[0].id]).await;
        assert!(
            publish_jobs(&pool).await.is_empty(),
            "a local poll's ballot announces nothing"
        );

        let asked = remote_status(&pool, &bob, "question-remote-voter", Visibility::Public).await;
        let stand = {
            let mut conn = pool.acquire().await.expect("a slot connection");
            poll::create(
                &mut conn,
                asked.id,
                &PollParams {
                    options: vec!["tea".to_owned()],
                    ..PollParams::default()
                },
            )
            .await
            .expect("the remote poll creates")
        };
        let mirror = remote(&pool, "voter").await;
        let options = poll::options(&pool, stand.id)
            .await
            .expect("the options read");
        after_poll_vote(&state, &stand, &mirror, &[options[0].id]).await;
        assert!(
            publish_jobs(&pool).await.is_empty(),
            "a mirror's ballot came from the wire and goes nowhere"
        );

        after_poll_vote(&state, &stand, &alice, &[new_id()]).await;
        assert!(
            publish_jobs(&pool).await.is_empty(),
            "an id no option of the poll holds names nothing to announce"
        );
        assert_eq!(
            job_count(&pool).await,
            0,
            "and none of it queued anything at all"
        );
    }

    /// The chat gates beside the local-recipient one: an inbound fold's
    /// landing (a remote sender's message, already delivered here), an
    /// id-less message with no object to name, and a withdrawal by an
    /// account that is not the sender all announce nothing even over a
    /// federating instance.
    #[tokio::test]
    async fn a_chat_write_that_may_not_federate_announces_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let erin = remote(&pool, "erin").await;
        let state = app_state(&pool);

        let (_, alice_ref, bob_ref) = {
            let mut tx = pool.begin().await.expect("a transaction begins");
            let written = chat::add_message(
                &mut tx,
                TEST_BASE_URL,
                &alice,
                &bob,
                &NewChatMessage::text("between locals"),
            )
            .await
            .expect("the chat message writes");
            tx.commit().await.expect("the transaction commits");
            written
        };
        let bob_ref = bob_ref.expect("bob is local");

        after_chat_message_created(&state, &bob_ref, &erin, &bob).await;
        assert!(
            publish_jobs(&pool).await.is_empty(),
            "a message the fold already landed is announced by nobody"
        );

        let mut idless = alice_ref.clone();
        idless.ap_id = None;
        after_chat_message_created(&state, &alice_ref, &alice, &erin).await;
        assert_eq!(
            publish_jobs(&pool).await.len(),
            1,
            "with its id the same remote post does federate"
        );

        after_chat_message_created(&state, &idless, &alice, &erin).await;
        after_chat_message_deleted(&state, &idless, &alice, &erin).await;
        after_chat_message_deleted(&state, &bob_ref, &bob, &alice).await;
        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "and none of the refusals queued more");
    }

    /// A profile edit publishes an `Update` of the actor document, to the
    /// public and the followers collection, under the per-account debounce
    /// key — and a burst of edits publishes exactly one activity.
    #[tokio::test]
    async fn a_profile_edit_publishes_the_update_once_per_burst() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let state = app_state(&pool);

        after_profile_updated(&state, &alice).await;
        after_profile_updated(&state, &alice).await;
        after_profile_updated(&state, &alice).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(
            jobs.len(),
            1,
            "the two follow-ups collapse into the live job"
        );
        let (args, unique_key) = &jobs[0];
        assert_eq!(
            unique_key.as_deref(),
            Some(&*format!("profile-update:{}", alice.id))
        );
        let activity = &args["activity"];
        assert_eq!(activity["type"], "Update");
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"]["id"], alice.ap_id);
        assert_eq!(activity["to"][0], PUBLIC);
        assert_eq!(activity["cc"][0], alice.followers_url.unwrap());
    }

    /// The account `Delete` names the actor id as both `actor` and
    /// `object`, carries the deterministic id, and hands the publisher the
    /// union of the remote followers' and followees' inboxes (a shared
    /// inbox per instance, once).
    #[tokio::test]
    async fn deleting_an_account_publishes_the_delete_to_both_sides() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        let star = remote(&pool, "star").await;
        accepted_follow(&pool, &fan, &alice).await;
        accepted_follow(&pool, &alice, &star).await;
        let state = app_state(&pool);

        after_account_deleted(&state, &alice).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1);
        let (args, unique_key) = &jobs[0];
        assert!(unique_key.is_none(), "the delete id itself dedups");
        let activity = &args["activity"];
        assert_eq!(activity["type"], "Delete");
        assert_eq!(
            activity["id"],
            format!("{TEST_BASE_URL}/activities/delete-user-{}", alice.id)
        );
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"], alice.ap_id);
        assert_eq!(activity["to"][0], PUBLIC);
        let mut inboxes: Vec<&str> = args["extra_inboxes"]
            .as_array()
            .expect("the snapshot rides along")
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .expect("every entry is a string")
            .into_iter()
            .collect();
        inboxes.sort_unstable();
        assert_eq!(
            inboxes,
            vec!["https://remote.example/inbox"],
            "both remote accounts share one instance inbox, and the snapshot names it once"
        );
    }

    /// An account that no remote instance ever touched publishes no
    /// `Delete`: there is nobody to tell.
    #[tokio::test]
    async fn a_lonely_deleted_account_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let state = app_state(&pool);

        after_account_deleted(&state, &alice).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "nobody on another instance knew this account"
        );
    }

    /// A remote mirror's row is not this instance's to announce.
    #[tokio::test]
    async fn a_remote_account_publishes_no_delete() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bob = remote(&pool, "bob").await;
        let state = app_state(&pool);

        after_account_deleted(&state, &bob).await;
        after_profile_updated(&state, &bob).await;

        assert_eq!(job_count(&pool).await, 0, "a mirror announces nothing");
    }

    /// Deactivation keeps the actor document live for the histories that
    /// cite it: the hook is documented silence, in every configuration.
    #[tokio::test]
    async fn deactivating_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let state = app_state(&pool);

        after_account_disabled(&state, &alice);

        assert_eq!(job_count(&pool).await, 0, "a deactivation queues nothing");
    }

    /// The Delete snapshot reaches a followee that publishes no shared
    /// inbox at their personal one, and skips a mirror with no inbox at
    /// all.
    #[tokio::test]
    async fn the_delete_snapshot_falls_back_to_personal_inboxes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let personal = insert(
            &pool,
            &NewUser {
                nickname: "solo@other.example".to_owned(),
                ap_id: "https://other.example/users/solo".to_owned(),
                local: Flag::new(false),
                domain: Some("other.example".to_owned()),
                inbox: Some("https://other.example/users/solo/inbox".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the personal-inbox fixture inserts");
        let headless = insert(
            &pool,
            &NewUser {
                nickname: "ghost@other.example".to_owned(),
                ap_id: "https://other.example/users/ghost".to_owned(),
                local: Flag::new(false),
                domain: Some("other.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the headless fixture inserts");
        accepted_follow(&pool, &personal, &alice).await;
        accepted_follow(&pool, &alice, &headless).await;
        let state = app_state(&pool);

        after_account_deleted(&state, &alice).await;

        let jobs = publish_jobs(&pool).await;
        let inboxes = jobs[0].0["extra_inboxes"]
            .as_array()
            .expect("a snapshot rides along");
        assert_eq!(
            inboxes.as_slice(),
            &[Value::String(
                "https://other.example/users/solo/inbox".to_owned()
            )],
            "the account with an inbox is named; the one without is skipped"
        );
    }

    /// The block document is only minted from the stored id; a row without
    /// one (a mirrored remote block) publishes nothing.
    #[tokio::test]
    async fn a_block_without_a_stored_id_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let mut row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the block writes");
        row.ap_id = None;
        let state = app_state(&pool);

        after_block_changed(&state, &row, false).await;

        assert_eq!(job_count(&pool).await, 0, "an id-less row has no document");
    }

    /// `published` on the block document is the row's own moment; on the
    /// undo too, and the shape matches the activity builders' golden
    /// (the id swapped for the stored one).
    #[test]
    fn the_block_document_carries_the_row_clock_and_stored_id() {
        let config = Config::for_tests();
        let published = Utc::now();
        let document = block_json(&config, new_id(), "alice-ap", "bob-ap", &published);
        assert!(document["published"].is_string());
        assert!(
            document["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("http"))
        );
    }

    // ───────────────  the interaction hooks  ───────────────

    /// A status mirrored from `remote.example`, authored by `author` and
    /// seen as `visibility`.
    async fn remote_status(
        pool: &PgPool,
        author: &User,
        n: &str,
        visibility: Visibility,
    ) -> Status {
        status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                uri: Some(format!("https://remote.example/objects/{n}")),
                user_id: author.id,
                visibility,
                ..NewStatus::default()
            },
        )
        .await
        .expect("the remote status fixture inserts")
    }

    /// A local public post by `author`.
    async fn local_status(pool: &PgPool, author: &User, text: &str) -> Status {
        status::insert(pool, TEST_BASE_URL, &NewStatus::local(author, text))
            .await
            .expect("the local status fixture inserts")
    }

    /// The first attempt a background handler runs on.
    const fn first_attempt() -> crate::jobs::AttemptInfo {
        crate::jobs::AttemptInfo {
            attempts: 1,
            max_attempts: 5,
        }
    }

    /// Runs every queued `federator.publish` job through the publisher's
    /// own handler, so the fan-out expands into `federator.deliver` jobs
    /// the test can name.
    async fn run_publishes(state: &AppState, pool: &PgPool) {
        let rows = sqlx::query_as::<_, (Value,)>(
            "SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at",
        )
        .bind(publisher::KIND_PUBLISH)
        .fetch_all(pool)
        .await
        .expect("the publish jobs read");
        for (args,) in rows {
            publisher::handle(state.clone(), first_attempt(), args)
                .await
                .expect("the fan-out job completes");
        }
    }

    /// The inboxes the `federator.deliver` jobs queued so far ask for,
    /// sorted, so fan-out sets compare directly.
    async fn delivered_inboxes(pool: &PgPool) -> Vec<String> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT args->>'inbox' FROM jobs WHERE kind = $1 ORDER BY 1",
        )
        .bind(crate::federation::delivery::KIND_DELIVER)
        .fetch_all(pool)
        .await
        .expect("the delivery jobs read");
        rows.into_iter().map(|(inbox,)| inbox).collect()
    }

    /// The favourite document the queue holds: exactly one publish job,
    /// unwrapped for the assertions every interaction test shares.
    async fn the_single_activity(pool: &PgPool) -> Value {
        let jobs = publish_jobs(pool).await;
        assert_eq!(jobs.len(), 1, "the write is announced once");
        assert!(jobs[0].1.is_none(), "interactions never debounce");
        jobs[0].0["activity"].clone()
    }

    /// Favouriting a public REMOTE status publishes the `Like` under the
    /// row's stored id, `to` the author and `cc` the followers collection
    /// and the public address — and the fan-out lands in the author's
    /// personal inbox and the followers' instance inbox, nowhere else.
    #[tokio::test]
    async fn favouriting_a_remote_status_publishes_a_like() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let liked = remote_status(&pool, &bob, "post-1", Visibility::Public).await;
        let row = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let state = app_state(&pool);

        after_favourite_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Like");
        assert_eq!(
            activity["id"],
            row.ap_id.expect("a local favourite carries its id"),
            "the document id is the row's stored ap_id, not a re-mint"
        );
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"], liked.uri, "object is the status uri");
        assert_eq!(
            activity["to"].as_array().expect("to is a line").len(),
            1,
            "the author is the only primary addressee"
        );
        assert_eq!(activity["to"][0], bob.ap_id);
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), PUBLIC]),
            "the followers collection and the public address ride cc \
             because the status is public"
        );
        assert_eq!(activity["published"], mastodon_time(&row.inserted_at));

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![
                "https://remote.example/inbox",
                "https://remote.example/users/bob/inbox",
            ],
            "the followers' shared inbox (once) and the author's personal inbox"
        );
    }

    /// Unfavouriting publishes the `Undo` that wraps the same `Like`: a
    /// fresh own-instance id for the wrapper, the stored id inside, and
    /// the like's own addressing repeated so the same inboxes hear it.
    #[tokio::test]
    async fn unfavouriting_publishes_the_undo_of_the_stored_like() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let liked = remote_status(&pool, &bob, "post-undo", Visibility::Public).await;
        let created = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let stored_id = created.ap_id.expect("a local favourite carries its id");
        let removed = favourite::unfavourite(&pool, alice.id, liked.id)
            .await
            .expect("the withdrawal runs")
            .expect("the like went");
        let state = app_state(&pool);

        after_favourite_changed(&state, &removed, false).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Undo");
        assert_eq!(activity["actor"], alice.ap_id);
        let id = activity["id"].as_str().expect("the undo has an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "the undo carries this instance's own activity shape: {id}"
        );
        assert_ne!(id, stored_id, "and it is a fresh id, not the like's");
        assert_eq!(activity["object"]["type"], "Like");
        assert_eq!(
            activity["object"]["id"], stored_id,
            "the embedded like keeps the row's stored id"
        );
        assert_eq!(activity["object"]["object"], liked.uri);
        assert_eq!(
            activity["to"],
            json!([bob.ap_id]),
            "the undo repeats the like's to"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), PUBLIC]),
            "and its cc"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![
                "https://remote.example/inbox",
                "https://remote.example/users/bob/inbox",
            ],
            "exactly the inboxes that heard the like hear the withdrawal"
        );
    }

    /// A like of a NOT-public status addresses the author alone: no
    /// followers collection, no public address, and the fan-out delivers
    /// to the author's inbox only.
    #[tokio::test]
    async fn a_like_of_a_non_public_status_addresses_the_author_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let liked = remote_status(&pool, &bob, "post-unlisted", Visibility::Unlisted).await;
        let row = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let state = app_state(&pool);

        after_favourite_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["to"], json!([bob.ap_id]));
        assert_eq!(
            activity["cc"],
            json!([]),
            "an unlisted post's like stays with the author"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/users/bob/inbox"],
            "the followers hear nothing about it"
        );
    }

    /// A like of a LOCAL public status still federates: the author is not
    /// a delivery target, so the remote followers' inboxes stand as the
    /// only recipients of the very same document.
    #[tokio::test]
    async fn a_like_of_a_local_public_status_reaches_the_remote_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let fan = remote(&pool, "fan").await;
        accepted_follow(&pool, &fan, &alice).await;
        let liked = local_status(&pool, &carol, "home soil").await;
        let row = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let state = app_state(&pool);

        after_favourite_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Like");
        assert_eq!(
            activity["to"],
            json!([carol.ap_id]),
            "the local author is addressed"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), PUBLIC]),
            "and the public fan-out line rides along"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "only the remote follower's instance is a delivery target"
        );
    }

    /// An interaction whose whole audience is local queues its publish job
    /// and expands it into nothing: a local author, local followers, and
    /// this instance's own origin are never delivery targets.
    #[tokio::test]
    async fn a_local_only_audience_yields_zero_deliveries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        accepted_follow(&pool, &bob, &alice).await;
        let liked = local_status(&pool, &bob, "between neighbours").await;
        let row = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let state = app_state(&pool);

        after_favourite_changed(&state, &row, true).await;
        run_publishes(&state, &pool).await;

        assert!(
            delivered_inboxes(&pool).await.is_empty(),
            "the fan-out runs and names nothing"
        );
    }

    /// A reaction on a public REMOTE status publishes the `EmojiReact`
    /// with the stored emoji in `content`, the custom emoji's image as an
    /// `Emoji` `tag`, the author in `to` and the followers collection (no
    /// public address) in `cc`.
    #[tokio::test]
    async fn a_custom_reaction_on_a_remote_status_publishes_the_tagged_emoji_react() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let reacted = remote_status(&pool, &bob, "react-target", Visibility::Public).await;
        let emoji_url = "https://test.example/emoji/railroad_track.png";
        let new = NewReaction {
            user_id: alice.id,
            status_id: reacted.id,
            emoji: ":railroad_track:",
            url: Some(emoji_url),
        };
        let row = emoji_reaction::react(&pool, TEST_BASE_URL, &new)
            .await
            .expect("the reaction writes");
        let state = app_state(&pool);

        after_reaction_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "EmojiReact");
        assert_eq!(
            activity["id"],
            row.ap_id.expect("a local reaction carries its id"),
            "the document id is the reaction row's stored ap_id"
        );
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"], reacted.uri);
        assert_eq!(
            activity["content"], ":railroad_track:",
            "the shortcode as content"
        );
        assert_eq!(
            activity["tag"],
            json!([{
                "type": "Emoji",
                "id": emoji_url,
                "name": ":railroad_track:",
                "icon": { "type": "Image", "url": emoji_url },
            }]),
            "the custom emoji rides as the note shape's Emoji tag"
        );
        assert_eq!(activity["to"], json!([bob.ap_id]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)]),
            "followers without the public address"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![
                "https://remote.example/inbox",
                "https://remote.example/users/bob/inbox",
            ],
            "the author's inbox plus the followers' instance"
        );
    }

    /// A plain unicode reaction carries `content` and no `tag` at all.
    #[tokio::test]
    async fn a_unicode_reaction_carries_no_tag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let reacted = remote_status(&pool, &bob, "react-unicode", Visibility::Public).await;
        let new = NewReaction {
            user_id: alice.id,
            status_id: reacted.id,
            emoji: "🎉",
            url: None,
        };
        let row = emoji_reaction::react(&pool, TEST_BASE_URL, &new)
            .await
            .expect("the reaction writes");
        let state = app_state(&pool);

        after_reaction_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "EmojiReact");
        assert_eq!(activity["content"], "🎉");
        assert!(
            activity.get("tag").is_none(),
            "a unicode reaction names no custom emoji to tag"
        );
    }

    /// Taking a reaction back publishes the `Undo` that embeds the stored
    /// `EmojiReact` and repeats its addressing.
    #[tokio::test]
    async fn unreacting_publishes_the_undo_of_the_reaction() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let reacted = remote_status(&pool, &bob, "react-undo", Visibility::Public).await;
        let new = NewReaction {
            user_id: alice.id,
            status_id: reacted.id,
            emoji: "🍺",
            url: None,
        };
        let created = emoji_reaction::react(&pool, TEST_BASE_URL, &new)
            .await
            .expect("the reaction writes");
        let stored_id = created.ap_id.expect("a local reaction carries its id");
        let removed = emoji_reaction::unreact(&pool, alice.id, reacted.id, "🍺")
            .await
            .expect("the withdrawal runs")
            .expect("the reaction went");
        let state = app_state(&pool);

        after_reaction_changed(&state, &removed, false).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Undo");
        assert_eq!(activity["object"]["type"], "EmojiReact");
        assert_eq!(
            activity["object"]["id"], stored_id,
            "the embedded reaction keeps the row's stored id"
        );
        assert_eq!(activity["object"]["content"], "🍺");
        let id = activity["id"].as_str().expect("the undo has an id");
        assert!(id.starts_with(&format!("{TEST_BASE_URL}/activities/")));
        assert_ne!(id, stored_id, "the wrapper stands under a fresh id");
        assert_eq!(activity["to"], json!([bob.ap_id]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)]),
            "the withdrawal repeats the reaction's cc line"
        );
    }

    /// Boosting a REMOTE status publishes the `Announce` the row *is*:
    /// its stored `uri` as the id, the world in `to`, and the followers
    /// collection beside the boosted author in `cc` — the fan-out lands
    /// in the author's inbox and the followers' instance inbox.
    #[tokio::test]
    async fn boosting_a_remote_status_publishes_the_announce() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let original = remote_status(&pool, &bob, "boost-me", Visibility::Public).await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_reblog_changed(&state, &reblog, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Announce");
        assert_eq!(
            activity["id"], reblog.uri,
            "the id is the reblog row's own uri (decision 6)"
        );
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(
            activity["object"], original.uri,
            "object is the original uri"
        );
        assert_eq!(
            activity["to"],
            json!([PUBLIC]),
            "a public boost addresses the world"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), bob.ap_id]),
            "the followers collection and the boosted author ride cc"
        );
        assert_eq!(activity["published"], mastodon_time(&reblog.inserted_at));

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![
                "https://remote.example/inbox",
                "https://remote.example/users/bob/inbox",
            ],
            "the boosted author's inbox plus the remote followers'"
        );
    }

    /// An unlisted boost names the followers collection in `to` — the
    /// public address goes — while the `cc` line stays the pair.
    #[tokio::test]
    async fn an_unlisted_boost_addresses_the_followers_in_to() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let original = remote_status(&pool, &bob, "boost-quietly", Visibility::Public).await;
        let (reblog, wrote) = status::create_reblog(
            &pool,
            TEST_BASE_URL,
            &alice,
            &original,
            Visibility::Unlisted,
        )
        .await
        .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_reblog_changed(&state, &reblog, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(
            activity["to"],
            json!([format!("{}/followers", alice.ap_id)]),
            "an unlisted boost talks to the followers, not the world"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), bob.ap_id]),
            "the cc line is the same pair"
        );
    }

    /// Boosting a LOCAL status federates: the document addresses the
    /// boosted local author in `cc` (never a delivery target) and the
    /// fan-out lands in the remote followers' inboxes alone.
    #[tokio::test]
    async fn boosting_a_local_status_federates_the_announce_to_the_remote_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let fan = remote(&pool, "fan").await;
        accepted_follow(&pool, &fan, &alice).await;
        let original = local_status(&pool, &carol, "boostable at home").await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_reblog_changed(&state, &reblog, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Announce");
        assert_eq!(activity["id"], reblog.uri);
        assert_eq!(activity["object"], original.uri);
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), carol.ap_id]),
            "the local author rides cc as any boosted author would"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "the announce travels to the remote followers alone"
        );
    }

    /// Unboosting publishes the `Undo` that embeds the `Announce` by its
    /// stored uri and repeats the boost's addressing.
    #[tokio::test]
    async fn unboosting_publishes_the_undo_of_the_announce() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let original = remote_status(&pool, &bob, "boost-then-drop", Visibility::Public).await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let announce_uri = reblog.uri.clone();
        let removed = status::remove_reblog(&pool, alice.id, original.id)
            .await
            .expect("the withdrawal runs")
            .expect("the boost went");
        let state = app_state(&pool);

        after_reblog_changed(&state, &removed, false).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Undo");
        assert_eq!(activity["object"]["type"], "Announce");
        assert_eq!(
            activity["object"]["id"], announce_uri,
            "the embedded announce keeps the row's uri, tombstone or not"
        );
        let id = activity["id"].as_str().expect("the undo has an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "the undo carries this instance's own activity shape: {id}"
        );
        assert_ne!(
            id, announce_uri,
            "the undo mints its OWN uuid, distinct from the boost's uri"
        );
        assert_eq!(
            activity["to"],
            json!([PUBLIC]),
            "the boost's to line repeats"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), bob.ap_id]),
            "and its cc line"
        );
    }

    /// Deleting a local reblog row sends the `Undo` of its `Announce` —
    /// never a `Delete` — and a deleted plain status answers the other way
    /// round: the `Delete` of its note, and no `Undo` beside it.
    #[tokio::test]
    async fn deleting_a_reblog_row_sends_the_undo_instead_of_a_delete() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let original = remote_status(&pool, &bob, "boost-doomed", Visibility::Public).await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let doomed = local_status(&pool, &alice, "plain post").await;
        let state = app_state(&pool);

        after_status_deleted(&state, &reblog).await;
        after_status_deleted(&state, &doomed).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(
            jobs.len(),
            2,
            "the boost withdraws itself and the plain status is deleted"
        );
        let undo = queued_activity(&jobs, "Undo");
        assert_eq!(undo["object"]["type"], "Announce");
        assert_eq!(
            undo["object"]["id"], reblog.uri,
            "a boost is never a Delete"
        );
        let delete = queued_activity(&jobs, "Delete");
        assert_eq!(
            delete["object"]["type"], "Tombstone",
            "and a plain status is never an Undo"
        );
        assert_eq!(delete["object"]["id"], doomed.uri);
    }

    /// A boost of a `local`-visibility original never federates, and
    /// neither do likes or reactions of a `local`-visibility status: the
    /// fact stays this instance's own.
    #[tokio::test]
    async fn nothing_federates_for_local_visibility_statuses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let hidden = remote_status(&pool, &bob, "instance-only", Visibility::Local).await;
        let liked = favourite::favourite(&pool, TEST_BASE_URL, alice.id, hidden.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let new = NewReaction {
            user_id: alice.id,
            status_id: hidden.id,
            emoji: "🎉",
            url: None,
        };
        let reacted = emoji_reaction::react(&pool, TEST_BASE_URL, &new)
            .await
            .expect("the reaction writes");
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &hidden, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_favourite_changed(&state, &liked, true).await;
        after_reaction_changed(&state, &reacted, true).await;
        after_reblog_changed(&state, &reblog, true).await;
        after_reblog_changed(&state, &reblog, false).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "a local-visibility post moves no queue"
        );
    }

    /// A boost whose own row is `local`-visibility never federates
    /// either — the row itself, not just its original, can hold the fact
    /// in.
    #[tokio::test]
    async fn a_local_visibility_boost_never_federates() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let original =
            remote_status(&pool, &bob, "public-for-local-boost", Visibility::Public).await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Local)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_reblog_changed(&state, &reblog, true).await;
        after_reblog_changed(&state, &reblog, false).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "a local-only boost moves no queue"
        );
    }

    /// A favourite row with no stored `ap_id` — a mirrored remote like,
    /// which this instance does not own — has no document to announce:
    /// silence, as the block's twin rule does.
    #[tokio::test]
    async fn a_like_without_a_stored_id_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let liked = remote_status(&pool, &bob, "post-idless", Visibility::Public).await;
        let mut row = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        row.ap_id = None;
        let state = app_state(&pool);

        after_favourite_changed(&state, &row, true).await;

        assert_eq!(job_count(&pool).await, 0, "an id-less row has no document");
    }

    /// An actor row that carries no `followers_url` still gets a whole
    /// document: the built `{ap_id}/followers` address rides the `cc`
    /// line, and the author's own inbox stands as the delivery target.
    #[tokio::test]
    async fn an_actor_without_a_stored_followers_url_names_the_built_address() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bare = insert(
            &pool,
            &NewUser::local("bare", &local_ap_id(TEST_BASE_URL, "bare")),
        )
        .await
        .expect("the bare local fixture inserts");
        let bob = remote(&pool, "bob").await;
        let liked = remote_status(&pool, &bob, "post-bare", Visibility::Public).await;
        let row = favourite::favourite(&pool, TEST_BASE_URL, bare.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let state = app_state(&pool);

        after_favourite_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", bare.ap_id), PUBLIC]),
            "the built address stands in for the missing column"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/users/bob/inbox"],
            "the author is reached; the unexpandable collection names nobody"
        );
    }

    /// The mirrored rows the inbound handlers fold belong to their maker's
    /// instance: a remote actor's like, reaction, boost — and the delete
    /// of a mirror boost — publish nothing.
    #[tokio::test]
    async fn a_remote_actors_interactions_publish_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let own = local_status(&pool, &alice, "alice speaks").await;
        let mirrored_like = favourite::favourite_like(
            &pool,
            bob.id,
            own.id,
            "https://remote.example/activities/like-9",
        )
        .await
        .expect("the mirror like writes")
        .expect("the pair liked");
        let new = NewReaction {
            user_id: bob.id,
            status_id: own.id,
            emoji: "🎉",
            url: None,
        };
        let mirrored_react = emoji_reaction::react_with_ap_id(
            &pool,
            &new,
            "https://remote.example/activities/react-9",
        )
        .await
        .expect("the mirror reaction writes");
        let mirror_reblog = status::insert(
            &pool,
            TEST_BASE_URL,
            &NewStatus {
                uri: Some("https://remote.example/activities/announce-9".to_owned()),
                user_id: bob.id,
                reblog_of_id: Some(own.id),
                ..NewStatus::default()
            },
        )
        .await
        .expect("the mirror boost writes");
        let state = app_state(&pool);

        after_favourite_changed(&state, &mirrored_like, true).await;
        after_reaction_changed(&state, &mirrored_react, true).await;
        after_reblog_changed(&state, &mirror_reblog, true).await;
        after_status_deleted(&state, &mirror_reblog).await;

        assert_eq!(job_count(&pool).await, 0, "a mirror announces nothing");
    }

    /// A registered-looking local actor with a featured collection too:
    /// exactly the row `register` writes, the pin hook's publisher.
    async fn pinner(pool: &PgPool, nickname: &str) -> User {
        let ap_id = local_ap_id(TEST_BASE_URL, nickname);
        insert(
            pool,
            &NewUser {
                private_key: Some("PEM".to_owned()),
                public_key: Some("PUB".to_owned()),
                inbox: Some(format!("{ap_id}/inbox")),
                followers_url: Some(format!("{ap_id}/followers")),
                featured_url: Some(featured(&ap_id)),
                ..NewUser::local(nickname, &ap_id)
            },
        )
        .await
        .expect("the pinning fixture inserts")
    }

    /// The queued publish job whose activity carries `type_name` — the
    /// move test reads the pair out of one queue without depending on the
    /// two inserts' order.
    fn queued_activity(jobs: &[(Value, Option<String>)], type_name: &str) -> Value {
        jobs.iter()
            .find(|(args, _)| args["activity"]["type"].as_str() == Some(type_name))
            .map_or_else(
                || panic!("a {type_name} job is queued"),
                |(args, _)| args["activity"].clone(),
            )
    }

    /// A local account files `new`'s report about `target`, asserting the
    /// row came back: the fixture shape every flag test starts from.
    async fn filed(
        pool: &PgPool,
        reporter: &User,
        target: &User,
        new: &report::NewReport,
    ) -> ReportRow {
        report::create(pool, TEST_BASE_URL, reporter, target, new)
            .await
            .expect("the report files")
    }

    /// The full body of a filing with nothing but a comment and the
    /// forward answer varied: the flag fixtures' shared shape.
    fn filing(comment: &str, forwarded: bool, status_ids: Vec<Uuid>) -> report::NewReport {
        report::NewReport {
            comment: comment.to_owned(),
            category: report::Category::Spam,
            forwarded,
            rule_ids: Vec::new(),
            status_ids,
        }
    }

    /// Moving an account publishes the `Move` to the origin's followers
    /// collection and the profile `Update` of the re-read row — the actor
    /// document it embeds carries `movedTo` — and the fan-out reaches the
    /// REMOTE followers (a local one is never a delivery target).
    #[tokio::test]
    async fn a_move_publishes_the_move_and_the_moved_update() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let neighbour = local(&pool, "neighbour").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        accepted_follow(&pool, &neighbour, &alice).await;
        let destination = "https://other.example/users/alice";
        let target = insert(
            &pool,
            &NewUser {
                nickname: "moved-to@other.example".to_owned(),
                ap_id: destination.to_owned(),
                local: Flag::new(false),
                domain: Some("other.example".to_owned()),
                inbox: Some(format!("{destination}/inbox")),
                ..NewUser::default()
            },
        )
        .await
        .expect("the destination fixture inserts");
        user::mark_moved(&pool, alice.id, &target.ap_id)
            .await
            .expect("the move marks in");
        let state = app_state(&pool);

        after_account_moved(&state, &alice, &target).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 2, "the Move and the profile Update");

        let activity = queued_activity(&jobs, "Move");
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(
            activity["object"], alice.ap_id,
            "the object is the origin's own id"
        );
        assert_eq!(activity["target"], destination);
        assert_eq!(
            activity["to"],
            json!([format!("{}/followers", alice.ap_id)]),
            "the followers collection is the only address"
        );
        assert_eq!(activity["cc"], json!([]));
        let id = activity["id"].as_str().expect("the move has an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "a fresh own-instance activity id: {id}"
        );

        let update = queued_activity(&jobs, "Update");
        assert_eq!(update["actor"], alice.ap_id);
        assert_eq!(update["object"]["id"], alice.ap_id);
        assert_eq!(
            update["object"]["movedTo"], destination,
            "the re-read row carries the move into the actor document"
        );
        assert!(
            jobs.iter()
                .any(|(_, key)| key.as_deref() == Some(&*format!("profile-update:{}", alice.id))),
            "the Update rides the per-account debounce key"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"; 2],
            "each activity reaches the remote follower's instance once, \
             and the local follower not at all"
        );
    }

    /// A mirrored account's move is its own instance's to announce.
    #[tokio::test]
    async fn a_mirror_moving_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        let state = app_state(&pool);

        after_account_moved(&state, &bob, &carol).await;

        assert_eq!(job_count(&pool).await, 0, "a mirror announces nothing");
    }

    /// A forwarded report about a REMOTE account publishes the `Flag` from
    /// the instance actor: the stored activity id, the account first and
    /// the cited statuses after it in the object, the comment as content —
    /// and the fan-out lands in the target's personal inbox alone.
    #[tokio::test]
    async fn a_forwarded_report_publishes_the_flag_to_the_target_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let fan = remote(&pool, "fan").await;
        let first = remote_status(&pool, &bob, "flagged-1", Visibility::Public).await;
        let second = remote_status(&pool, &bob, "flagged-2", Visibility::Public).await;
        let row = filed(
            &pool,
            &alice,
            &bob,
            &filing("they will not stop", true, vec![second.id, first.id]),
        )
        .await;
        let instance_actor = instance_actor::ensure_instance_actor(&pool, &Config::for_tests())
            .await
            .expect("the instance actor provisions");
        accepted_follow(&pool, &fan, &instance_actor).await;
        let state = app_state(&pool);

        after_report_created(&state, &row).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the forward is announced once");
        assert!(jobs[0].1.is_none(), "reports never debounce");
        assert_eq!(
            jobs[0].0["actor_id"],
            instance_actor.id.to_string(),
            "the publish runs as the instance actor"
        );
        let activity = &jobs[0].0["activity"];
        assert_eq!(activity["type"], "Flag");
        assert_eq!(
            activity["id"],
            row.ap_id.clone().expect("a local filing carries its id"),
            "the document id is the report row's stored ap_id"
        );
        assert_eq!(activity["actor"], instance_actor.ap_id);
        assert_eq!(
            activity["object"],
            json!([bob.ap_id, first.uri, second.uri]),
            "the account leads the object list, then the citations oldest first"
        );
        assert_eq!(activity["content"], "they will not stop");
        assert_eq!(
            activity["to"],
            json!([bob.ap_id]),
            "the reported account is the only address"
        );
        assert_eq!(activity["cc"], json!([]));
        assert_eq!(activity["published"], mastodon_time(&row.inserted_at));

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/users/bob/inbox"],
            "the target's personal inbox only — not even the instance actor's follower hears"
        );
    }

    /// A forwarded report that cited nothing still flags the account.
    #[tokio::test]
    async fn a_flag_without_citations_still_names_the_account() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let row = filed(&pool, &alice, &bob, &filing("spam", true, Vec::new())).await;
        let state = app_state(&pool);

        after_report_created(&state, &row).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Flag");
        assert_eq!(activity["object"], json!([bob.ap_id]));
        assert_eq!(activity["content"], "spam");
    }

    /// A report about a LOCAL account stays with our own moderators,
    /// forwarded or not.
    #[tokio::test]
    async fn a_report_about_a_local_account_publishes_no_flag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let forwarded = filed(&pool, &alice, &bob, &filing("noise", true, Vec::new())).await;
        let state = app_state(&pool);

        after_report_created(&state, &forwarded).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "a local target never sees an instance-to-instance Flag"
        );
    }

    /// A filing that asked for no forwarding federates nothing, even
    /// about a remote target.
    #[tokio::test]
    async fn an_unforwarded_report_publishes_no_flag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let row = filed(&pool, &alice, &bob, &filing("mine", false, Vec::new())).await;
        let state = app_state(&pool);

        after_report_created(&state, &row).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "the reporter chose not to tell the other instance"
        );
    }

    /// A report row with no stored `ap_id` — the inbound mirror's shape —
    /// has no document of ours to stand behind.
    #[tokio::test]
    async fn a_report_without_a_stored_id_publishes_no_flag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let mut row = filed(&pool, &alice, &bob, &filing("echo", true, Vec::new())).await;
        row.ap_id = None;
        let state = app_state(&pool);

        after_report_created(&state, &row).await;

        assert_eq!(job_count(&pool).await, 0, "an id-less row has no document");
    }

    /// Pinning publishes the `Add` that features the status in the
    /// author's collection, `to` the public with the followers in `cc` —
    /// and the fan-out reaches the REMOTE followers only.
    #[tokio::test]
    async fn a_pin_publishes_the_add_to_the_featured_collection() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = pinner(&pool, "alice").await;
        let neighbour = local(&pool, "neighbour").await;
        let fan = remote(&pool, "fan").await;
        accepted_follow(&pool, &fan, &alice).await;
        accepted_follow(&pool, &neighbour, &alice).await;
        let featured_post = local_status(&pool, &alice, "look at me").await;
        let row = pinned_status::pin(&pool, alice.id, featured_post.id)
            .await
            .expect("the pin writes")
            .expect("the pair pinned");
        let state = app_state(&pool);

        after_pin_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Add");
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"], featured_post.uri);
        assert_eq!(
            activity["target"],
            alice.featured_url.expect("the fixture has a collection"),
            "the target is the actor's own featured collection"
        );
        assert_eq!(activity["to"], json!([PUBLIC]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)]),
            "the followers ride cc — the public address itself names nobody"
        );
        let id = activity["id"].as_str().expect("the add has an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "a fresh own-instance activity id: {id}"
        );
        assert_eq!(activity["published"], mastodon_time(&row.inserted_at));

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "the remote follower's instance only; the local one heard nothing"
        );
    }

    /// Unpinning publishes the `Remove` on the same skeleton: the same
    /// object, the same target, the same audience.
    #[tokio::test]
    async fn an_unpin_publishes_the_remove() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = pinner(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        accepted_follow(&pool, &fan, &alice).await;
        let featured_post = local_status(&pool, &alice, "was featured").await;
        let _created = pinned_status::pin(&pool, alice.id, featured_post.id)
            .await
            .expect("the pin writes");
        let removed = pinned_status::unpin(&pool, alice.id, featured_post.id)
            .await
            .expect("the withdrawal runs")
            .expect("the pin went");
        let state = app_state(&pool);

        after_pin_changed(&state, &removed, false).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Remove");
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"], featured_post.uri);
        assert_eq!(
            activity["target"],
            alice.featured_url.expect("the fixture has a collection")
        );
        assert_eq!(activity["to"], json!([PUBLIC]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)])
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "the withdrawal travels to the remote followers alone"
        );
    }

    /// An actor row with no `featured_url` still gets a whole document:
    /// the built `{ap_id}/featured` address rides the `target`.
    #[tokio::test]
    async fn a_pin_without_a_stored_featured_url_names_the_built_address() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let featured_post = local_status(&pool, &alice, "feature me").await;
        let row = pinned_status::pin(&pool, alice.id, featured_post.id)
            .await
            .expect("the pin writes")
            .expect("the pair pinned");
        let state = app_state(&pool);

        after_pin_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(
            activity["target"],
            featured(&alice.ap_id),
            "the built address stands in for the missing column"
        );
    }

    /// A mirrored account's pin and a pin of a status that has since died
    /// announce nothing.
    #[tokio::test]
    async fn pins_ours_to_announce_answer_with_silence() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = pinner(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let mirrored_pin = pinned_status::pin(
            &pool,
            bob.id,
            local_status(&pool, &alice, "alice speaks").await.id,
        )
        .await
        .expect("the mirror pin writes")
        .expect("the pair pinned");
        let doomed_post = local_status(&pool, &alice, "about to go").await;
        let orphaned_pin = pinned_status::pin(&pool, alice.id, doomed_post.id)
            .await
            .expect("the pin writes")
            .expect("the pair pinned");
        let _erased = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(doomed_post.id)
            .execute(&pool)
            .await
            .expect("the fixture erases the status");
        let state = app_state(&pool);

        after_pin_changed(&state, &mirrored_pin, true).await;
        after_pin_changed(&state, &orphaned_pin, true).await;
        after_pin_changed(&state, &mirrored_pin, false).await;

        assert_eq!(job_count(&pool).await, 0, "neither row is ours to tell");
    }

    // ───────────────  the note lifecycle  ───────────────

    /// A remote mirror `nickname@peer.example`, shared inbox included: the
    /// second host the fan-out tests need beside [`remote`]'s one, so a
    /// delivery set proves the followers' expansion crosses instances.
    async fn peer(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@peer.example"),
                ap_id: format!("https://peer.example/users/{nickname}"),
                local: Flag::new(false),
                domain: Some("peer.example".to_owned()),
                inbox: Some(format!("https://peer.example/users/{nickname}/inbox")),
                shared_inbox: Some("https://peer.example/inbox".to_owned()),
                public_key: Some("PUB".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the peer fixture inserts")
    }

    /// A local post by `author` of `text`, seen as `visibility`.
    async fn local_post(
        pool: &PgPool,
        author: &User,
        text: &str,
        visibility: Visibility,
    ) -> Status {
        status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                visibility,
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("the local post fixture inserts")
    }

    /// The edit the route just wrote, applied straight to `posted`: the new
    /// text in both its spellings and the `edited_at` stamp
    /// [`crate::models::status_edit`] marks with. The test's stand-in for
    /// the edit transaction, so the hook runs on a row in the state a real
    /// edit leaves behind.
    async fn apply_edit(pool: &PgPool, posted: &Status, html: &str, text: &str) {
        let updated = sqlx::query(
            "UPDATE statuses SET content = $2, raw_text = $3, edited_at = now(), \
                updated_at = now() WHERE id = $1",
        )
        .bind(posted.id)
        .bind(html)
        .bind(text)
        .execute(pool)
        .await
        .expect("the edit fixture writes");
        assert_eq!(updated.rows_affected(), 1, "the post is there to edit");
    }

    /// Switches an account off the way a deactivation or suspension does.
    async fn deactivate(pool: &PgPool, account: &User) {
        let updated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(account.id)
            .execute(pool)
            .await
            .expect("the deactivation fixture writes");
        assert_eq!(
            updated.rows_affected(),
            1,
            "the account is there to switch off"
        );
    }

    /// Creating a public post for an account with followers on two remote
    /// hosts queues the exact `Create` the object route serves — the
    /// document and the status-derived id decision 6 pins — and the fan-out
    /// POSTs it to each host's shared inbox once.
    #[tokio::test]
    async fn a_public_post_publishes_the_create_to_the_remote_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        let dave = peer(&pool, "dave").await;
        accepted_follow(&pool, &carol, &alice).await;
        accepted_follow(&pool, &dave, &alice).await;
        let posted = local_post(&pool, &alice, "hello world", Visibility::Public).await;
        let state = app_state(&pool);

        after_status_created(&state, &posted).await;

        let config = Config::for_tests();
        let ctx = note::load_note_context(&pool, &config, posted.id)
            .await
            .expect("the note context reads");
        let note = note::note_json(&ctx).expect("a public note renders");
        let activity = the_single_activity(&pool).await;
        assert_eq!(
            activity,
            note::create_activity_json(&note, &posted, &config),
            "the queued document is exactly the `Create` of the served note"
        );
        assert_eq!(activity["type"], "Create");
        assert_eq!(
            activity["id"],
            format!("{TEST_BASE_URL}/activities/{}", posted.id),
            "the activity id is the status' own — the address the C2S create promises"
        );
        assert_eq!(activity["actor"], alice.ap_id, "the author publishes");
        assert_eq!(activity["to"], json!([PUBLIC]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)]),
            "the followers collection rides cc, and the publisher expands it"
        );
        assert_eq!(activity["object"]["id"], posted.uri);
        assert_eq!(activity["published"], mastodon_time(&posted.inserted_at));

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://peer.example/inbox", "https://remote.example/inbox"],
            "each host is posted to once, at its shared inbox"
        );
    }

    /// A reply to a REMOTE parent needs no special arm: the parent's author
    /// rides the note's `cc` (the mention list the document is built from
    /// carries them), and the fan-out reaches their personal inbox beside
    /// the followers' shared one.
    #[tokio::test]
    async fn a_reply_addresses_the_remote_parent_author() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let parent = remote_status(&pool, &bob, "parent-to-answer", Visibility::Public).await;
        let reply = status::insert(
            &pool,
            TEST_BASE_URL,
            &NewStatus {
                in_reply_to_id: Some(parent.id),
                in_reply_to_account_id: Some(bob.id),
                ..NewStatus::local(&alice, "answering you")
            },
        )
        .await
        .expect("the reply fixture inserts");
        let state = app_state(&pool);

        after_status_created(&state, &reply).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Create");
        assert_eq!(activity["to"], json!([PUBLIC]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), bob.ap_id]),
            "the parent's author stands in the reply's cc line"
        );
        assert_eq!(
            activity["object"]["inReplyTo"], parent.uri,
            "and the object names the parent it answers"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![
                "https://remote.example/inbox",
                "https://remote.example/users/bob/inbox",
            ],
            "the follower's shared inbox and the parent author's personal one"
        );
    }

    /// A `direct` post is addressed to its mentionees alone: the note's `to`
    /// names the remote account, `cc` is empty, and the author's followers —
    /// remote included — hear nothing at all.
    #[tokio::test]
    async fn a_direct_post_goes_only_to_the_mentioned() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let dm = status::insert(
            &pool,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Direct,
                mentioned_user_ids: vec![bob.id],
                ..NewStatus::local(&alice, "a private word")
            },
        )
        .await
        .expect("the direct fixture inserts");
        let state = app_state(&pool);

        after_status_created(&state, &dm).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Create");
        assert_eq!(
            activity["to"],
            json!([bob.ap_id]),
            "the recipient is the whole audience"
        );
        assert_eq!(
            activity["cc"],
            json!([]),
            "no followers collection rides along"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/users/bob/inbox"],
            "the mentioned account's personal inbox, and the follower is not told"
        );
    }

    /// An edit queues the `Update` carrying the re-drafted note: a freshly
    /// minted activity id (never the `Create`'s), the same reserved object
    /// uri inside, the new text with its `updated` stamp, and the status'
    /// own addressing so the copy every host holds is the one corrected.
    #[tokio::test]
    async fn an_edit_publishes_the_update_of_the_new_note() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let posted = local_post(&pool, &alice, "first draft", Visibility::Public).await;
        let before = posted.clone();
        apply_edit(&pool, &posted, "<p>second draft</p>", "second draft").await;
        let state = app_state(&pool);

        after_status_edited(&state, &before).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Update");
        assert_eq!(activity["actor"], alice.ap_id);
        let id = activity["id"].as_str().expect("the update carries an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "a fresh own-instance activity id: {id}"
        );
        assert_ne!(
            id,
            format!("{TEST_BASE_URL}/activities/{}", posted.id),
            "an edit is its own activity, not a restatement of the create"
        );
        assert_eq!(
            activity["object"]["id"], posted.uri,
            "the object is the same reserved uri the create carried"
        );
        assert_eq!(
            activity["object"]["content"], "<p>second draft</p>",
            "and it is the NEW note, read back after the write"
        );
        assert!(
            activity["object"]["updated"].is_string(),
            "the re-drafted note carries its edit stamp"
        );
        assert_eq!(
            activity["to"],
            json!([PUBLIC]),
            "the addressing is the status' own"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)]),
            "both lines"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "the host holding a copy is the host that hears about the edit"
        );
    }

    /// Deleting a post through the shared deletion service queues the
    /// `Delete` whose object is the status' `Tombstone`, under the
    /// deterministic id the ticket pins and the addressing the post
    /// travelled with — the withdrawal reaches exactly the hosts that hold
    /// a copy, and a retried delete names the same activity.
    #[tokio::test]
    async fn a_delete_publishes_the_delete_with_its_tombstone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let doomed = local_post(&pool, &alice, "here today", Visibility::Public).await;
        let state = app_state(&pool);

        let withdrawn = crate::services::delete_status::delete_status(&state, &doomed)
            .await
            .expect("the deletion service runs");
        assert!(withdrawn, "the row was live and went");

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Delete");
        assert_eq!(
            activity["id"],
            format!("{TEST_BASE_URL}/activities/delete-{}", doomed.id),
            "the deterministic withdrawal id"
        );
        assert_eq!(
            activity["actor"], alice.ap_id,
            "the author takes the post down"
        );
        assert_eq!(
            activity["to"],
            json!([PUBLIC]),
            "the addressing the post carried"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)]),
            "both of its lines"
        );
        assert_eq!(
            activity["object"]["id"], doomed.uri,
            "the reserved object id"
        );
        assert_eq!(activity["object"]["type"], "Tombstone");
        assert_eq!(activity["object"]["formerType"], "Note");
        let gone = status::get_by_id_including_deleted(&pool, doomed.id)
            .await
            .expect("the tombstone reads")
            .expect("the row is still stored, tombstoned");
        assert_eq!(
            activity["object"]["deleted"],
            mastodon_time(&gone.deleted_at.expect("the delete stamped its instant")),
            "the withdrawal reports the delete's own moment — the same one the \
             `/objects/{{uuid}}` route answers beside its 410"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "the withdrawal goes exactly where the post went"
        );
    }

    /// `local` visibility is the outright refusal, in every direction: none
    /// of the three lifecycle hooks queues a job for such a post, and none
    /// of them expands the followers collection either.
    #[tokio::test]
    async fn a_local_visibility_note_federates_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let hidden = local_post(&pool, &alice, "instance only", Visibility::Local).await;
        let state = app_state(&pool);

        after_status_created(&state, &hidden).await;
        after_status_edited(&state, &hidden).await;
        after_status_deleted(&state, &hidden).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "a local-visibility post moves no queue at all"
        );
    }

    /// A post whose whole audience is local is still announced — the
    /// publisher just finds no inbox to announce it to: a local follower's
    /// timeline is the delivery, and no HTTP job is ever queued.
    #[tokio::test]
    async fn local_followers_receive_no_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let neighbour = local(&pool, "neighbour").await;
        accepted_follow(&pool, &neighbour, &alice).await;
        let posted = local_post(&pool, &alice, "for our own eyes", Visibility::Public).await;
        let state = app_state(&pool);

        after_status_created(&state, &posted).await;
        assert_eq!(
            publish_jobs(&pool).await.len(),
            1,
            "the note itself is announced"
        );

        run_publishes(&state, &pool).await;
        assert!(
            delivered_inboxes(&pool).await.is_empty(),
            "and the fan-out names no inbox"
        );
    }

    /// An account that is switched off posts nothing and corrects nothing:
    /// both arms of the announce side refuse it. The withdrawal of a post it
    /// made while it was live still goes out — the peers that hold a copy
    /// have to learn it is gone, which is the whole point of the `Delete`.
    #[tokio::test]
    async fn a_switched_off_author_announces_no_new_post() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let posted = local_post(&pool, &alice, "before the freeze", Visibility::Public).await;
        deactivate(&pool, &alice).await;
        let state = app_state(&pool);

        after_status_created(&state, &posted).await;
        after_status_edited(&state, &posted).await;
        assert!(
            publish_jobs(&pool).await.is_empty(),
            "a deactivated account's post neither goes out nor is re-drafted"
        );

        after_status_deleted(&state, &posted).await;
        let activity = the_single_activity(&pool).await;
        assert_eq!(
            activity["type"], "Delete",
            "the withdrawal is not held back by the flag"
        );
    }

    /// The note hooks speak for local plain posts only: a mirrored row —
    /// which the ingest path runs through this very hook — and a reblog row,
    /// whose activity is the `Announce` of [`after_reblog_changed`], are both
    /// silence.
    #[tokio::test]
    async fn a_mirror_and_a_boost_publish_no_note_activities() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let mirrored = remote_status(&pool, &bob, "mirror-note", Visibility::Public).await;
        let original = local_post(&pool, &alice, "boost me", Visibility::Public).await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_status_created(&state, &mirrored).await;
        after_status_edited(&state, &mirrored).await;
        after_status_deleted(&state, &mirrored).await;
        after_status_created(&state, &reblog).await;
        after_status_edited(&state, &reblog).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "neither a mirror's note nor a boost row is announced as a note"
        );
    }

    // ───────────────  the follow-write hooks  ───────────────

    /// The args of every queued `federator.deliver` job, in insertion order.
    async fn delivery_jobs(pool: &PgPool) -> Vec<Value> {
        sqlx::query_as::<_, (Value,)>(
            "SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
        )
        .bind(crate::federation::delivery::KIND_DELIVER)
        .fetch_all(pool)
        .await
        .expect("the delivery jobs read")
        .into_iter()
        .map(|(args,)| args)
        .collect()
    }

    /// `follower`'s follow of `target` recorded through the model, asserted
    /// to carry what a local follower's row always does: a freshly minted
    /// `ap_id` (decision 6) — the id the announcement must reuse.
    async fn recorded_follow(pool: &PgPool, follower: &User, target: &User) -> Follow {
        let row = follow::follow(pool, TEST_BASE_URL, follower, target)
            .await
            .expect("the follow fixture writes");
        assert!(
            row.ap_id.is_some(),
            "a local follower's row carries a minted activity id"
        );
        row
    }

    /// Records the REMOTE requester `follower`'s follow of the local
    /// `target` and lands the delivered activity's id on it — the shape the
    /// inbound fold stores for a peer's `Follow`, which the answers name.
    async fn remote_request(pool: &PgPool, follower: &User, target: &User, n: &str) -> Follow {
        let mut row = follow::follow(pool, TEST_BASE_URL, follower, target)
            .await
            .expect("the request fixture writes");
        let delivered = format!("https://remote.example/activities/{n}");
        follow::set_ap_id(pool, row.id, &delivered)
            .await
            .expect("the delivered id lands");
        row.ap_id = Some(delivered);
        row
    }

    /// The `{BASE_URL}/activities/{uuid}` shape an answer or a withdrawal
    /// must carry as its own id.
    fn assert_fresh_activity_id(id: &str) {
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "the activity carries this instance's own id shape: {id}"
        );
        let _uuid = Uuid::parse_str(id.rsplit('/').next().unwrap_or_default())
            .expect("the id ends in a uuid");
    }

    /// Following a REMOTE account queues the row's `Follow` straight to the
    /// followee's personal inbox: the stored `ap_id` as the activity id,
    /// the follower as actor, the followee as `object` and as the only
    /// address — nobody else, not even the follower's own followers.
    #[tokio::test]
    async fn following_a_remote_account_delivers_the_stored_follow() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        let row = recorded_follow(&pool, &alice, &fan).await;
        let state = app_state(&pool);

        after_follow_changed(&state, &alice, &fan, Some(&row), true).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the request is announced once");
        assert!(
            publish_jobs(&pool).await.is_empty(),
            "a one-recipient follow ships without a fan-out job"
        );
        assert_eq!(
            jobs[0]["inbox"], "https://remote.example/users/fan/inbox",
            "the followee's personal inbox is the sole target"
        );
        assert_eq!(
            jobs[0]["actor_id"],
            alice.id.to_string(),
            "signed as the follower"
        );
        let body = &jobs[0]["body"];
        assert_eq!(body["type"], "Follow");
        assert_eq!(
            body["id"],
            row.ap_id.expect("a local follower's row carries its id"),
            "the activity id is the stored follows.ap_id"
        );
        assert_eq!(body["actor"], alice.ap_id);
        assert_eq!(body["object"], fan.ap_id);
        assert_eq!(body["to"], json!([fan.ap_id]));
        assert_eq!(body["cc"], json!([]));
        assert_eq!(body["published"], mastodon_time(&row.inserted_at));
    }

    /// Unfollowing delivers the `Undo` that wraps the very `Follow` the row
    /// stands for: the wrapper's own fresh id outside, the stored id inside,
    /// and the same single addressee.
    #[tokio::test]
    async fn unfollowing_a_remote_account_delivers_the_undo() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        let row = recorded_follow(&pool, &alice, &fan).await;
        let stored_id = row
            .ap_id
            .clone()
            .expect("a local follower's row carries its id");
        let _withdrawn = follow::unfollow(&pool, &alice, &fan)
            .await
            .expect("the withdrawal runs");
        let state = app_state(&pool);

        after_follow_changed(&state, &alice, &fan, Some(&row), false).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the withdrawal is announced once");
        assert_eq!(
            jobs[0]["inbox"], "https://remote.example/users/fan/inbox",
            "the withdrawn follow's addressee hears the taking-back"
        );
        let body = &jobs[0]["body"];
        assert_eq!(body["type"], "Undo");
        assert_eq!(body["actor"], alice.ap_id);
        assert_eq!(body["to"], json!([fan.ap_id]));
        assert_eq!(body["cc"], json!([]));
        let id = body["id"].as_str().expect("the undo has an id");
        assert_fresh_activity_id(id);
        assert_ne!(
            id,
            stored_id.as_str(),
            "the wrapper stands under a fresh id"
        );
        assert_eq!(body["object"]["type"], "Follow");
        assert_eq!(
            body["object"]["id"], stored_id,
            "the embedded follow keeps the row's stored id"
        );
        assert_eq!(body["object"]["actor"], alice.ap_id);
        assert_eq!(body["object"]["object"], fan.ap_id);
    }

    /// A local followee, a mirrored follower and a write that left no row
    /// behind are the documented silences: no delivery, no fan-out, nothing.
    #[tokio::test]
    async fn follow_writes_that_are_not_ours_to_announce_publish_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let neighbour = local(&pool, "neighbour").await;
        let fan = remote(&pool, "fan").await;
        let other = remote(&pool, "other").await;
        let open_pair = recorded_follow(&pool, &alice, &neighbour).await;
        let mirrored = follow::follow(&pool, TEST_BASE_URL, &fan, &other)
            .await
            .expect("the mirror's follow writes");
        let state = app_state(&pool);

        after_follow_changed(&state, &alice, &neighbour, Some(&open_pair), true).await;
        after_follow_changed(&state, &alice, &neighbour, Some(&open_pair), false).await;
        after_follow_changed(&state, &fan, &other, Some(&mirrored), true).await;
        after_follow_changed(&state, &alice, &fan, None, false).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "a local pair, a mirror's follow and an empty withdrawal queue nothing"
        );
    }

    /// A follow row with no `ap_id` has no document to stand behind — both
    /// arms fall silent, exactly as the block hook's twin rule does.
    #[tokio::test]
    async fn a_follow_without_a_stored_id_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        let mut row = recorded_follow(&pool, &alice, &fan).await;
        row.ap_id = None;
        let state = app_state(&pool);

        after_follow_changed(&state, &alice, &fan, Some(&row), true).await;
        after_follow_changed(&state, &alice, &fan, Some(&row), false).await;

        assert_eq!(job_count(&pool).await, 0, "an id-less row has no document");
    }

    /// The endpoint's idempotent repeat — a second follow of the open pair —
    /// restates the same activity, and the per-inbox delivery dedup is what
    /// keeps the restated request a single sending.
    #[tokio::test]
    async fn a_repeated_follow_restates_one_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        let row = recorded_follow(&pool, &alice, &fan).await;
        let state = app_state(&pool);

        after_follow_changed(&state, &alice, &fan, Some(&row), true).await;
        after_follow_changed(&state, &alice, &fan, Some(&row), true).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(
            jobs.len(),
            1,
            "the re-send collapses into the live delivery"
        );
        assert_eq!(
            jobs[0]["body"]["id"],
            row.ap_id.expect("a local follower's row carries its id"),
            "and it is the row's own activity, not a re-mint"
        );
    }

    /// Authorizing a REMOTE requester's follow delivers the `Accept` to
    /// their personal inbox alone: the followee acts, the delivered
    /// `Follow` rides inside by its id — the same answer the inbound
    /// handler sends on the spot for an unlocked account.
    #[tokio::test]
    async fn authorizing_a_remote_request_delivers_the_accept() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = local(&pool, "carol").await;
        let bob = remote(&pool, "bob").await;
        let row = remote_request(&pool, &bob, &carol, "fo-ask-1").await;
        let state = app_state(&pool);

        after_follow_request_resolved(&state, &carol, &row, true).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the grant answers once");
        assert_eq!(
            jobs[0]["inbox"], "https://remote.example/users/bob/inbox",
            "the follower's personal inbox is the sole target"
        );
        assert_eq!(
            jobs[0]["actor_id"],
            carol.id.to_string(),
            "signed as the followee"
        );
        let body = &jobs[0]["body"];
        assert_eq!(body["type"], "Accept");
        assert_eq!(body["actor"], carol.ap_id);
        assert_eq!(body["to"], json!([bob.ap_id]));
        assert_fresh_activity_id(body["id"].as_str().expect("the answer has an id"));
        assert_eq!(
            body["object"],
            json!({
                "id": row.ap_id.expect("the delivered id stands"),
                "type": "Follow",
                "actor": bob.ap_id,
                "object": carol.ap_id,
            }),
            "the answer embeds the delivered Follow, the shape peers match on"
        );
    }

    /// Refusing a REMOTE requester is the same document under `Reject`.
    #[tokio::test]
    async fn rejecting_a_remote_request_delivers_the_reject() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = local(&pool, "carol").await;
        let bob = remote(&pool, "bob").await;
        let row = remote_request(&pool, &bob, &carol, "fo-refuse-1").await;
        let state = app_state(&pool);

        after_follow_request_resolved(&state, &carol, &row, false).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the refusal answers once");
        assert_eq!(jobs[0]["inbox"], "https://remote.example/users/bob/inbox");
        let body = &jobs[0]["body"];
        assert_eq!(body["type"], "Reject");
        assert_eq!(body["actor"], carol.ap_id);
        assert_eq!(body["to"], json!([bob.ap_id]));
        assert_eq!(body["object"]["type"], "Follow");
        assert_eq!(
            body["object"]["id"],
            row.ap_id.expect("the delivered id stands"),
            "the refusal answers the same delivered activity"
        );
    }

    /// A LOCAL requester's answer — granted or refused — moves the row and
    /// never a wire: the timeline is the delivery. An id-less request (no
    /// delivered activity to answer) and a resolver who is not one of our
    /// own actors are silent too.
    #[tokio::test]
    async fn follow_request_answers_that_stay_local_deliver_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = local(&pool, "carol").await;
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let local_request = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("the local request writes");
        let idless = follow::follow(&pool, TEST_BASE_URL, &bob, &carol)
            .await
            .expect("the remote request writes");
        assert!(
            idless.ap_id.is_none(),
            "a remote follower's row starts without an id"
        );
        let state = app_state(&pool);

        after_follow_request_resolved(&state, &carol, &local_request, true).await;
        after_follow_request_resolved(&state, &carol, &local_request, false).await;
        after_follow_request_resolved(&state, &carol, &idless, true).await;
        after_follow_request_resolved(&state, &bob, &local_request, true).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "no answer of these four has a remote side to reach"
        );
    }

    /// Dropping a REMOTE follower answers their `Follow` with a `Reject`
    /// at their personal inbox — Pleroma's `User.remove_follower` outcome,
    /// the removed row's stored id embedded.
    #[tokio::test]
    async fn removing_a_remote_follower_delivers_the_reject() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let _request = remote_request(&pool, &bob, &alice, "fo-drop-1").await;
        let removed = follow::remove_follower(&pool, alice.id, bob.id)
            .await
            .expect("the removal runs")
            .expect("the follower was there");
        let state = app_state(&pool);

        after_follower_removed(&state, &alice, &removed).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the dropped follower is answered once");
        assert_eq!(
            jobs[0]["inbox"], "https://remote.example/users/bob/inbox",
            "the follower's personal inbox is the sole target"
        );
        assert_eq!(
            jobs[0]["actor_id"],
            alice.id.to_string(),
            "signed as the followee"
        );
        let body = &jobs[0]["body"];
        assert_eq!(body["type"], "Reject");
        assert_eq!(body["actor"], alice.ap_id);
        assert_eq!(body["to"], json!([bob.ap_id]));
        assert_fresh_activity_id(body["id"].as_str().expect("the answer has an id"));
        assert_eq!(
            body["object"]["id"],
            removed.ap_id.expect("the delivered id stands"),
            "the Reject answers the very Follow the row stood for"
        );
    }

    /// A LOCAL follower removed from the list is settled in the database:
    /// the answer never reaches a wire.
    #[tokio::test]
    async fn removing_a_local_follower_delivers_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let row = follow::follow(&pool, TEST_BASE_URL, &bob, &alice)
            .await
            .expect("the local follow writes");
        let removed = follow::remove_follower(&pool, alice.id, bob.id)
            .await
            .expect("the removal runs")
            .expect("the follower was there");
        assert_eq!(
            row.state, removed.state,
            "the fixture removed the accepted row"
        );
        let state = app_state(&pool);

        after_follower_removed(&state, &alice, &removed).await;

        assert_eq!(job_count(&pool).await, 0, "a local follower hears nothing");
    }

    /// Stages a chat message from `sender` to `recipient` carrying `ap_id`
    /// and returns the two rows the withdrawal hook reads: the SENDER's chat
    /// and the sender's own reference to the message — exactly what the
    /// admin delete hands over after its tombstone. The write reserves an id
    /// for every message, so a `None` here hands back the reference stripped
    /// of its id: the shape that was never announced.
    async fn chat_message(
        pool: &PgPool,
        sender: &User,
        recipient: &User,
        ap_id: Option<&str>,
    ) -> (chat::Chat, ChatMessageReference) {
        let mut tx = pool
            .begin()
            .await
            .expect("the chat fixture opens a transaction");
        let new = NewChatMessage {
            ap_id: ap_id.map(str::to_owned),
            ..NewChatMessage::text("take this back")
        };
        let (_message, mut sender_reference, _other_side) =
            chat::add_message(&mut tx, TEST_BASE_URL, sender, recipient, &new)
                .await
                .expect("the chat fixture writes");
        tx.commit().await.expect("the chat fixture commits");
        if ap_id.is_none() {
            sender_reference.ap_id = None;
        }
        let chat_row = chat::get_owned(pool, sender.id, sender_reference.chat_id)
            .await
            .expect("the chat reads")
            .expect("the sender's chat stands");
        (chat_row, sender_reference)
    }

    /// The `Delete` document one withdrawn local message is announced with:
    /// a fresh activity id under this instance's origin, the sender as
    /// actor, the peer alone in `to`, and the stored object id embedded
    /// under the `ChatMessage` type the receiver's dispatcher routes on.
    #[test]
    fn the_chat_delete_document_names_the_sender_and_the_peer() {
        let config = Config::for_tests();
        let document = chat_delete_json(
            &config,
            Uuid::nil(),
            "https://test.example/users/alice",
            "https://test.example/objects/chat-1",
            "https://remote.example/users/bob",
        );

        assert_eq!(
            document["id"],
            format!("{TEST_BASE_URL}/activities/{}", Uuid::nil()),
            "the document's own id is a fresh activity id"
        );
        assert_eq!(document["type"], "Delete");
        assert_eq!(document["actor"], "https://test.example/users/alice");
        assert_eq!(
            document["to"],
            json!(["https://remote.example/users/bob"]),
            "the chat's other participant is the only address"
        );
        assert_eq!(
            document["object"]["id"],
            "https://test.example/objects/chat-1"
        );
        assert_eq!(document["object"]["type"], "ChatMessage");
        assert!(
            document["@context"].is_array(),
            "the shared json-ld header rides along"
        );
    }

    /// The local-origin gate is exactly the `{BASE_URL}/objects/` prefix:
    /// another instance's id is nobody's withdrawal to announce, and neither
    /// is this instance's own activity namespace.
    #[test]
    fn only_this_instances_object_ids_are_local_origin() {
        let config = Config::for_tests();
        assert!(is_local_object_id(
            &config,
            "https://test.example/objects/chat-1"
        ));
        assert!(
            !is_local_object_id(&config, "https://remote.example/objects/chat-1"),
            "a remote object id is the remote's own to withdraw"
        );
        assert!(
            !is_local_object_id(&config, "https://test.example/activities/chat-1"),
            "the objects prefix is the whole test"
        );
    }

    /// A withdrawn local message of a local↔remote chat queues the one
    /// `Delete` as the sender, addressed to the remote partner alone.
    #[tokio::test]
    async fn withdrawing_a_local_chat_message_queues_a_delete_for_the_remote_partner() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let object_ap_id = format!("{TEST_BASE_URL}/objects/{}", new_id());
        let (chat_row, reference) = chat_message(&pool, &alice, &bob, Some(&object_ap_id)).await;
        let state = app_state(&pool);

        after_chat_message_deleted_by_admin(&state, &chat_row, &reference).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the withdrawal is announced once");
        assert!(jobs[0].1.is_none(), "and it never debounces");
        assert_eq!(
            jobs[0].0["actor_id"],
            alice.id.to_string(),
            "the message's own sender signs it"
        );
        let activity = &jobs[0].0["activity"];
        assert_eq!(activity["type"], "Delete");
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["to"], json!([bob.ap_id]));
        assert_eq!(activity["object"]["id"], object_ap_id);
        assert_eq!(activity["object"]["type"], "ChatMessage");
        assert_fresh_activity_id(activity["id"].as_str().expect("the delete has an id"));
    }

    /// The silences: a message that never carried an id, one whose id is a
    /// remote's, one whose sender is a remote mirror even under a local id,
    /// a local↔local pair, and a switched-off federation queue nothing —
    /// none of them ever announces a withdrawal.
    #[tokio::test]
    async fn withdrawals_that_are_not_ours_to_announce_queue_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;

        let state = app_state(&pool);

        // No id at all: an ordinary local post, never announced.
        let (idless_chat, idless) = chat_message(&pool, &alice, &bob, None).await;
        after_chat_message_deleted_by_admin(&state, &idless_chat, &idless).await;

        // A remote-origin id: its own instance withdraws it.
        let remote_id = "https://remote.example/objects/chat-1";
        let (foreign_chat, foreign) = chat_message(&pool, &alice, &bob, Some(remote_id)).await;
        after_chat_message_deleted_by_admin(&state, &foreign_chat, &foreign).await;

        // A remote sender, even wearing a local-looking id: not our actor.
        let borrowed_id = format!("{TEST_BASE_URL}/objects/{}", new_id());
        let (mirror_chat, mirror_side) =
            chat_message(&pool, &bob, &alice, Some(&borrowed_id)).await;
        after_chat_message_deleted_by_admin(&state, &mirror_chat, &mirror_side).await;

        // A local partner: both copies live in this database.
        let carol = local(&pool, "carol").await;
        let pair_id = format!("{TEST_BASE_URL}/objects/{}", new_id());
        let (local_pair_chat, local_pair) =
            chat_message(&pool, &alice, &carol, Some(&pair_id)).await;
        after_chat_message_deleted_by_admin(&state, &local_pair_chat, &local_pair).await;

        // Federation switched off, even for a real local↔remote pair.
        let quiet_id = format!("{TEST_BASE_URL}/objects/{}", new_id());
        let (off_chat, off_side) = chat_message(&pool, &alice, &bob, Some(&quiet_id)).await;
        let quiet = state_with(&pool, unfederated());
        after_chat_message_deleted_by_admin(&quiet, &off_chat, &off_side).await;

        assert_eq!(job_count(&pool).await, 0, "none of the five queues a job");
    }

    /// Adding a REMOTE account to a local list publishes the `Add`: the
    /// member as `object`, the list's curated inbox as `target`, the member
    /// alone in `to` — and the fan-out reaches their PERSONAL inbox, the
    /// endpoint that names them, nobody else's.
    #[tokio::test]
    async fn adding_a_remote_list_member_publishes_the_add_to_their_inbox() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        let member = remote(&pool, "member").await;
        accepted_follow(&pool, &fan, &alice).await;
        let list_id = new_id();
        let state = app_state(&pool);

        after_list_member_changed(&state, &alice, list_id, &member, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Add");
        assert_eq!(activity["actor"], alice.ap_id, "the owner speaks");
        assert_eq!(activity["object"], member.ap_id, "the member is the object");
        assert_eq!(
            activity["target"],
            format!("{TEST_BASE_URL}/api/v1/lists/{list_id}/inbox"),
            "the curated inbox the member's instance is to post into"
        );
        assert_eq!(activity["to"], json!([member.ap_id]));
        assert_eq!(activity["cc"], json!([]), "and no fan-out beyond them");

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/users/member/inbox"],
            "the member's own inbox, not the shared one, and not the follower's"
        );
    }

    /// Withdrawing a remote member publishes the matching `Remove` under a
    /// fresh activity id — the two writes are two announcements.
    #[tokio::test]
    async fn removing_a_remote_list_member_publishes_the_remove() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let member = remote(&pool, "member").await;
        let list_id = new_id();
        let state = app_state(&pool);

        after_list_member_changed(&state, &alice, list_id, &member, false).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Remove");
        assert_eq!(activity["object"], member.ap_id);
        let id = activity["id"].as_str().expect("the Remove has an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "a fresh own-instance activity id: {id}"
        );
    }

    /// The memberships this instance keeps to itself announce nothing: a
    /// LOCAL member (whose timeline is the delivery), an owner that is a
    /// mirror rather than one of our actors, and a list write while
    /// federation is switched off.
    #[tokio::test]
    async fn list_membership_writes_that_are_not_federated_queue_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let neighbour = local(&pool, "neighbour").await;
        let mirror_owner = remote(&pool, "mirror-owner").await;
        let member = remote(&pool, "member").await;
        let list_id = new_id();
        let state = app_state(&pool);

        after_list_member_changed(&state, &alice, list_id, &neighbour, true).await;
        after_list_member_changed(&state, &mirror_owner, list_id, &member, false).await;
        let quiet = state_with(&pool, unfederated());
        after_list_member_changed(&quiet, &alice, list_id, &member, true).await;

        assert_eq!(job_count(&pool).await, 0, "none of the three queues a job");
    }
}
