//! The chat arms the pair's own writes drive: the `Create{ChatMessage}` a
//! local account's message to a REMOTE counterpart is, and the `Delete` of
//! its reserved object id when the sender withdraws the message.
//!
//! A chat message to a local recipient never travels over HTTP, and a
//! recipient whose `accepts_chat_messages` flag is down is not delivered to;
//! the moderator's tombstone of a message is [`super::chat_admin_deletes`]'
//! arm. The row readers and the publisher hand are the root's.

use serde_json::{Value, json};
use tracing::warn;
use uuid::Uuid;

use super::queue_activity;
use crate::{
    config::Config,
    federation::note,
    models::{
        chat::ChatMessageReference,
        media_attachment::{self, MediaAttachment},
        user::User,
    },
    state::AppState,
    util::time::mastodon_time,
};

/// One chat message as the REMOTE counterpart must see it: the stored
/// view ([`ChatMessageReference`]'s message columns), its reserved object
/// id, and the two actor ends of the pair. Grouped so the two builders
/// below stay inside the parameter budget, the way [`Ballot`] does for the
/// poll answers.
///
/// [`Ballot`]: super::polls::Ballot
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
/// and the same one-address line as the `Create` it takes back. The sender's
/// own withdrawal and the admin's tombstone both build it here, so the same
/// message always names the same `Delete`.
pub(super) fn chat_delete_json(
    config: &Config,
    message_id: Uuid,
    actor_ap_id: &str,
    object_ap_id: &str,
    recipient_ap_id: &str,
) -> Value {
    json!({
        "@context": note::json_ld_context(config),
        "id": format!("{}/activities/delete-{message_id}", config.base_url),
        "type": "Delete",
        "actor": actor_ap_id,
        "to": [recipient_ap_id],
        "cc": [],
        "object": { "id": object_ap_id, "type": "ChatMessage" },
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
    let document = chat_delete_json(
        &state.config,
        message.message_id,
        &user.ap_id,
        ap_id,
        &other.ap_id,
    );
    queue_activity(state, user, &document, "chat message withdrawal").await;
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        models::chat::{self, NewChatMessage},
        services::federation_out::{
            after_chat_message_deleted_by_admin,
            test_support::{
                app_state, job_count, local, publish_jobs, remote, state_with, unfederated,
            },
        },
        util::id::new_id,
    };

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
        assert_eq!(
            activity["id"],
            format!("{TEST_BASE_URL}/activities/delete-{}", reference.message_id),
            "the admin's delete carries the deterministic id the sender's own does"
        );
        assert_eq!(activity["cc"], json!([]));
    }

    /// The `Delete` document for one message id is the same on every call
    /// (a deterministic `delete-{message id}` id, no fresh uuid) and names
    /// the peer in `to` with an empty `cc`.
    #[test]
    fn the_chat_delete_document_is_stable_for_a_message_id() {
        let config = Config::for_tests();
        let message_id = new_id();
        let build = || {
            chat_delete_json(
                &config,
                message_id,
                "https://test.example/users/alice",
                "https://test.example/objects/chat-1",
                "https://remote.example/users/bob",
            )
        };

        let document = build();
        assert_eq!(
            document,
            build(),
            "the same message builds the same document"
        );
        assert_eq!(
            document["id"],
            format!("{TEST_BASE_URL}/activities/delete-{message_id}")
        );
        assert_eq!(document["type"], "Delete");
        assert_eq!(document["actor"], "https://test.example/users/alice");
        assert_eq!(document["to"], json!(["https://remote.example/users/bob"]));
        assert_eq!(
            document["cc"],
            json!([]),
            "the cc line is present and empty"
        );
        assert_eq!(
            document["object"],
            json!({ "id": "https://test.example/objects/chat-1", "type": "ChatMessage" })
        );
        assert!(
            document["@context"].is_array(),
            "the shared json-ld header rides along"
        );
        assert_ne!(
            document["id"],
            chat_delete_json(&config, new_id(), "a", "b", "c")["id"],
            "another message names another activity"
        );
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
}
