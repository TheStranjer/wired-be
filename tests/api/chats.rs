//! The chat API over HTTP (`/api/v1/pleroma/chats`, `/api/v2/pleroma/chats`,
//! WBE-146; its read-marker and pinning half, WBE-148; and its messages,
//! WBE-147).
//!
//! The tests pin what the three tickets promise. On the write side:
//! `POST …/chats/{id}/read` marks every reference of the caller's chat up to
//! `last_read_id` read — up to and including, idempotently, leaving anything
//! newer unread — and answers the `Chat` entity with the new `unread` count;
//! without the marker the request is the 400
//! `{"error":"Missing parameter: last_read_id"}`, and a present non-id
//! marker its `Invalid parameter` sibling. `POST …/messages/{message_id}/read`
//! marks one reference and answers the `ChatMessage` entity; a reference of
//! another chat is not there. `POST …/pin` and `…/unpin` toggle `pinned` and
//! move what the lists' `pinned` filter (both mounted below: `GET
//! /api/v1/pleroma/chats` and `GET /api/v2/pleroma/chats`) sees.
//!
//! The message routes (WBE-147) pin their own set: `GET …/messages` answers
//! `ChatMessage` entities newest first, a 20-default page under `MAX_LIMIT`,
//! with the `Link` header keyed on reference ids and the keyset cursors
//! walking them; `POST …/messages` takes `content` and/or `media_id` (the
//! sender's own unclaimed image/video/audio row, attached to the message),
//! refusing neither-key bodies, oversize content (against the
//! `MAX_CHAT_MESSAGE_CHARS` setting, default and lowered) and unattachable
//! media each with its own 400, honoring `Idempotency-Key` within the hour,
//! and landing the message unread in the recipient's chat;
//! `DELETE …/messages/{message_id}` answers the deleted entity and splits as
//! the model does — the recipient loses their copy, the sender's delete
//! withdraws the message from both sides.
//!
//! On the read side: `POST …/by-account-id/{id}` gets or creates the
//! caller's chat with that account, 200 and idempotent, and refuses the
//! self-chat with a 422, a recipient whose `accepts_chat_messages` flag is
//! down with the 403 naming the flag, and a malformed, unknown or
//! deactivated account with the one 404; the lists answer `Chat` entities
//! newest activity first under the `Link` header keyed on chat ids, with
//! the `pinned` and `with_muted` filters, v1's 30-row page cap against v2's
//! larger one, and keyset walks by `max_id`/`min_id`/`since_id` over the
//! chat ids; `GET …/chats/{id}` answers the chat to its owner alone.
//! Foreign, unknown and malformed ids are one 404 everywhere, and every
//! route demands its scope: `read:chats` to look, `write:chats` to change
//! (the plain `read` and `write` roots cover both sub-scopes).

#![cfg(test)]

/// The block gate of the create and post routes, and the gates' order.
mod chat_blocks;
/// The chat create and show routes and their gates.
mod chat_create_show;
/// The chat lists, v1 and v2: order, filters, caps and keyset walks.
mod chat_lists;
/// The scope gates of the reading and creating routes.
mod chat_scope_gates;
/// The message delete routes and the message routes' gates.
mod messages_delete_gates;
/// The message list, the unread arrival and the idempotency replay.
mod messages_list;
/// The message post: media, empty bodies, the length limit.
mod messages_post;
/// The message-route fixtures and asserts the message children share.
mod messages_support;
/// The pin pair, the owner-only reads and the write gates.
mod pins;
/// The whole-chat and single-message read markers.
mod read_markers;
/// Shared consts and fixture helpers every submodule of this area draws on.
mod support;
