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
//! - [`after_account_deleted`]: a `Delete` of the actor, run as the first
//!   act of the `users.delete` sweep (before it clears the relationship
//!   rows the fan-out reads), delivered to the union of the account's
//!   remote followers' and remote followees' inboxes plus the accepted
//!   relays' (both sides learned something from the actor, both must learn
//!   it is gone), then in a second publish to every other known remote
//!   inbox;
//! - [`after_account_disabled`]: nothing — deactivation deliberately
//!   federates no activity, so the hook is the documented landing point for
//!   a ticket that ever changes that answer;
//! - [`after_report_created`]: the `Flag` a forwarded report owes a REMOTE
//!   target's instance — the instance actor speaks it, the reported account
//!   and its cited status uris are the object and the reporter's comment is
//!   the content, delivered to the target's inbox alone (Mastodon's
//!   forwarded-report convention). A local target or a filing that asked
//!   for no forwarding stays with our own moderators;
//! - [`after_scrobble_created`]: the `Listen` (wrapping an `Audio` object)
//!   a fresh scrobble is, addressed by the scrobble's own visibility and
//!   delivered to the listener's remote followers; a `direct` scrobble
//!   addresses nobody and sends nothing;
//! - [`after_poll_vote`]: the answers a ballot on a REMOTE status's poll
//!   becomes — one `Create` per chosen option, its object a bodyless `Note`
//!   naming the option title and answering the question's uri (the shape
//!   [`crate::federation::handlers::vote`] folds on the way in), addressed
//!   to the poll's author alone and delivered to their inbox. A poll on a
//!   local status sends no answer;
//! - [`after_local_poll_changed`] / [`after_local_poll_closed`]: the
//!   `Update` of a LOCAL poll's `Question`, queued as a
//!   `polls.publish_update` job naming the status, three minutes ahead
//!   after a vote (local or federated) and at once when the poll closes,
//!   under the key `poll-update:{status id}`. The document is rendered when
//!   the job runs ([`handle_publish_update`]), so it carries the tally as
//!   of then;
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
//! The children hold the arms — `blocks`, `boosts`, `chat_admin_deletes`,
//! `chats`, `collections`, `follow_requests`, `follows`, `likes`, `moves`,
//! `notes`, `polls`, `profile`, `reactions`, `reports`, `scrobbles`, `statuses` and
//! `withdrawals` — while this root keeps the plumbing they share:
//! `StatusAction` and `status_action`, the row readers `load_status` and
//! `load_user`, the two publisher hands
//! `queue_activity` and `publish_only_to`, and the `withdrawal` builder. The
//! fixtures every child's tests build on stand in the test-only
//! `test_support` child.
//!
//! [`activities`]: crate::federation::activities

use serde_json::Value;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    federation::{
        activities::{AnsweredActivity, undo_json},
        publisher::{self, PublishOpts},
    },
    models::{
        activitypub_actor,
        status::{self, Status, Visibility},
        user::{self, User},
    },
    state::AppState,
    util::id::new_id,
};

mod audience;
mod blocks;
mod boosts;
mod chat_admin_deletes;
mod chats;
mod collections;
mod follow_requests;
mod follows;
mod likes;
mod moves;
mod notes;
mod polls;
mod profile;
mod quotes;
mod reactions;
mod reports;
mod scrobbles;
mod statuses;
#[cfg(test)]
mod test_support;
mod withdrawals;

pub use blocks::after_block_changed;
pub use boosts::after_reblog_changed;
pub use chat_admin_deletes::after_chat_message_deleted_by_admin;
pub use chats::{after_chat_message_created, after_chat_message_deleted};
pub use collections::after_pin_changed;
pub use follow_requests::{after_follow_request_resolved, after_follower_removed};
pub use follows::{FollowAnswer, accept_follow, after_follow_changed, reject_follow};
pub use likes::after_favourite_changed;
pub use moves::after_account_moved;
pub use polls::{
    KIND_PUBLISH_UPDATE, after_local_poll_changed, after_local_poll_closed, after_poll_vote,
    handle_publish_update,
};
pub use profile::{after_account_deleted, after_account_disabled, after_profile_updated};
pub use quotes::{QuoteRequestAnswer, answer_quote_request};
pub use reactions::after_reaction_changed;
pub use reports::after_report_created;
pub use scrobbles::after_scrobble_created;
pub use statuses::{after_status_created, after_status_edited};
pub use withdrawals::after_status_deleted;

/// The local actor, the acted-on status and its author: the three rows
/// every interaction document is built from.
#[derive(Debug)]
struct StatusAction {
    /// The local account whose write is announced.
    actor: User,
    /// The followers collection id on the actor's `activitypub_actors` row,
    /// for the documents that address the actor's followers.
    actor_followers_url: Option<String>,
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
///
/// [`note_publisher`]: notes::note_publisher
/// [`note_withdrawal`]: withdrawals::note_withdrawal
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
    let actor_followers_url = match activitypub_actor::get(&state.db, actor.id).await {
        Ok(row) => row.and_then(|row| row.followers_url),
        Err(err) => {
            warn!(%err, %actor_id, "federation hook could not read an actor's ActivityPub row");
            return None;
        }
    };
    Some(StatusAction {
        public: status.visibility == Visibility::Public,
        actor,
        actor_followers_url,
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

/// The addresses of a local actor's collections, as `activitypub_actors`
/// stores them.
#[derive(Debug)]
struct ActorCollections {
    /// The followers collection a document names in `cc`.
    followers: String,
    /// The featured (pinned) collection a pin document names as `target`.
    featured: String,
}

/// Reads the collection addresses of `actor` from its `activitypub_actors`
/// row, answering `None` for a database that will not answer (a federation
/// miss, never a failure).
///
/// An actor with no row, or a row carrying no address, gets the built
/// address of that collection, as an unset column always did.
async fn load_collections(state: &AppState, actor: &User) -> Option<ActorCollections> {
    match activitypub_actor::get(&state.db, actor.id).await {
        Ok(row) => Some(ActorCollections {
            followers: activitypub_actor::followers_address(row.as_ref(), &actor.ap_id),
            featured: activitypub_actor::featured_address(row.as_ref(), &actor.ap_id),
        }),
        Err(err) => {
            warn!(%err, id = %actor.id, "federation hook could not read an actor row");
            None
        }
    }
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
    use super::test_support::{
        filed, filing, job_count, local, local_status, publish_jobs, remote,
        remote_ballot_and_chat, remote_status, state_with, unfederated,
    };
    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        models::{
            block,
            emoji_reaction::{self, NewReaction},
            favourite, follow, pinned_status,
        },
    };

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
        after_account_deleted(&state, &alice)
            .await
            .expect("the Delete federation runs");
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
}
