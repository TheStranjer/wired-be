//! The `Delete` of a chat message an admin withdrew: the message's own
//! sender speaks it, the REMOTE other participant alone hears it, and a
//! message this instance never announced — no `ap_id`, or one minted by
//! another origin — is nobody's to take back.
//!
//! The pair's own send and withdraw arms are [`super::chats`]'; the row
//! readers and the publisher hand are the root's.

use super::{chats::chat_delete_json, load_user, queue_activity};
use crate::{
    config::Config,
    models::chat::{self, ChatMessageReference},
    state::AppState,
};

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
        message.message_id,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
