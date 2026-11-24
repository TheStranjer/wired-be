//! End-to-end federation of inbound content (WBE-224 scenario 3 and the chat
//! half of scenario 7): a real, signed peer delivery read back through this
//! instance's public API.
//!
//! Every scenario here runs the round trip the [`remote::MockPeer`] exists
//! for: a mock actor delivers a signed activity to this instance's shared
//! inbox over the in-process router (which authenticates it and mirrors the
//! actor under its loopback host, decision 7), [`common::run_all_jobs`] folds
//! the `federator.incoming` job plus anything it queues (the `Update`,
//! backfill, delivery and notification work), and the result is read back
//! through the client API — the timelines, `GET /api/v1/statuses/{id}`,
//! `accounts/{id}/statuses`, `context`, `polls/{id}`, the reactions and chat
//! endpoints, and `GET /api/v1/notifications` — never off a raw row dump. A
//! delivery that folded into the wrong status, the wrong audience or the
//! wrong place therefore cannot pass.
//!
//! What the scenarios pin, per the ticket:
//!
//! - a remote `Create Note` at each visibility, each read as the mock's actor
//!   mirror under its loopback host: `public` in `timelines/public` and the
//!   author's `accounts/{id}/statuses`, `unlisted` out of the public timeline
//!   but on the profile and in a follower's home, `private` only to an
//!   approved follower, and `direct` only in the recipient's conversations
//!   with nothing leaked to a timeline;
//! - the rich content of one delivered note — an `attachment` (the remote
//!   media shape with `url`/`preview_url`), a `Hashtag` (the status in
//!   `timelines/tag/{name}`), a `Mention` of a local user (a `mention` telling
//!   and visibility), a `sensitive`/`summary` content warning, a `Question`
//!   poll the viewer votes back on, and a custom `Emoji` that carries through
//!   the reaction path;
//! - an `Update` of the same object uri editing the status and leaving the
//!   client's `history`, and a restatement changing nothing further;
//! - a `Delete` taking the note down through the client API and stepping its
//!   parent's counter, with a late `Create` of the same uri ignored and the
//!   author's post count unmoved;
//! - `Like` / `Undo{Like}`, `EmojiReact` / `Undo{EmojiReact}` and `Announce`
//!   / `Undo{Announce}` moving the counters, the `reactions` read and the
//!   public-timeline boost and raising or clearing the matching telling;
//! - a delivered reply backfilling its missing parent over real HTTP and a
//!   parent the peer 404s degrading to the reply standing alone;
//! - a remote ballot marking a local poll's option and telling its author,
//!   idempotent when restated; and
//! - a delivered `Create{ChatMessage}` opening the pair's chat with exactly
//!   one `pleroma:chat_mention`, its sender's `Delete` withdrawing both and a
//!   stranger's leaving them standing.
//!
//! The per-type dispatcher corners belong to `ingest.rs`, `note_edits.rs`,
//! `poll_votes.rs`, `emoji_reactions.rs`, `reblogs.rs`, `favourites.rs`,
//! `notification_generation.rs` and `chat_federation.rs`; this module owns
//! the real-HTTP, real-signature, public-API view of the same folds.
//!
//! [`common::run_all_jobs`]: crate::common::run_all_jobs

#![cfg(test)]

mod edits_deletes;
mod interactions;
mod listens;
mod notes;
mod support;
mod threads_polls_chats;
