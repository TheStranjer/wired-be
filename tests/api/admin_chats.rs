//! The admin chat endpoints over HTTP (`/api/v1/pleroma/admin/…`, WBE-202):
//! Pleroma's `AdminAPI.ChatController` beside `list_user_chats` — the user
//! chat listing, the chat show, the message listing and the moderation
//! withdrawal of a single message, all rendered from the ordinary `Chat`
//! and `ChatMessage` entities WITHOUT ownership checks.
//!
//! What the tests pin, per the ticket:
//!
//! - `GET …/users/{nickname}/chats` answers that account's chats newest
//!   activity first with the `Link` header keyed on chat ids (and the
//!   keyset walks them); an unknown nickname is the 404
//!   `{"error":"Not found"}`, and an account with no chats the empty array
//!   with no `Link` header at all;
//! - `GET …/chats/{id}` shows ANY chat by its id — the admin is neither
//!   participant — while a malformed or unknown id is one 404;
//! - `GET …/chats/{id}/messages` answers reference-id-keyed pages newest
//!   first and leaves withdrawn messages out, exactly as the owner's own
//!   client sees the same chat;
//! - `DELETE …/chats/{id}/messages/{message_id}` tombstones the message for
//!   BOTH participants whoever wrote it (both user-side listings go empty),
//!   answers the entity as it stood before the delete, writes the
//!   `chat_message_delete` moderation entry, queues the federation `Delete`
//!   for a local-origin message whose partner is remote and nothing
//!   otherwise, and 404s a second strike at the same message;
//! - every route sits behind `AdminUser` plus its scope: `admin:read:chats`
//!   for the three reads, `admin:write:chats` for the delete, the class
//!   roots standing for both.

#![cfg(test)]

mod support;
