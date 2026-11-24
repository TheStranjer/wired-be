//! Emoji reactions over HTTP (`/api/v1/pleroma/statuses/{id}/reactions`,
//! WBE-94): reacting, withdrawing a reaction, and reading them back.
//!
//! The two writes are Pleroma's `create` and `delete`: a user token with
//! `write:statuses`, the status resolved through the shared visibility gate (a
//! malformed, unknown, deleted or invisible id is the one 404), and the
//! percent-decoded `{emoji}` segment either one unicode emoji grapheme or a
//! `:shortcode:` of a local, enabled custom emoji — anything else is the 400
//! `{"error":"Invalid emoji"}`. Both answer the reacted-to `Status` entity as
//! the caller sees it, whose `pleroma.emoji_reactions` carries the roll-up: one
//! entry per emoji, the whole count, the viewer's own `me`, the custom emoji's
//! image URL and at most five reactor ids. Both writes are idempotent, so a
//! repeat `PUT` is one reaction and a `DELETE` of a reaction that is not there
//! is the unchanged status with 200.
//!
//! The two reads are Pleroma's `index`, with and without the emoji filter:
//! optional auth (`read:statuses` demanded of a *presented* user token), one
//! object per emoji — `{name, count, me, url, accounts}` — ordered by each
//! emoji's first reaction and then by the emoji text, and every reactor as a
//! full `Account` entity. Reactions of accounts the viewer blocked are hidden,
//! and so are those of accounts they muted unless they sent `with_muted`; an
//! emoji left with no visible reactor leaves the array, and a deactivated
//! reactor is dropped from its emoji's list and count, Pleroma's `fetch_users`
//! doing the same.

#![cfg(test)]

mod reads;
mod reads_visibility;
mod remote;
mod support;
mod writes;
