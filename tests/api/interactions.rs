//! Inbound interactions over federation (WBE-177): the dispatcher's own
//! entry point [`handle_incoming`] folding a peer's `Like`, `EmojiReact`
//! and `Announce` — and their `Undo`s — onto this instance's rows
//! (`src/federation/handlers/interactions.rs`).
//!
//! Every test delivers the fixture activity through the real dispatcher —
//! the same job the inbox routes enqueue, run in-process with the signer's
//! stored mirror as its own row — and reads the answer back from the
//! database and from the status and notification endpoints, exactly what a
//! local author would see. What this pins, per the ticket:
//!
//! - a delivered like folds a `favourites` row keyed by the activity id,
//!   the author's counter, and exactly one telling; a restated pair marks
//!   nothing twice, and the `Undo` — by bare id or wrapped inner — takes
//!   row, count and telling back;
//! - a reaction lands with the activity's own `Emoji` tag url, content the
//!   local rules refuse stores nothing, and one emoji's undo removes only
//!   its own row;
//! - an announce is a reblog row whose uri is the activity id, whose
//!   visibility the activity's addressing states, and which folds onto the
//!   original when it names a boost; a restated pair is ignored and the
//!   withdrawn id stays reserved;
//! - a target that is not visible to its actor — a stranger's private or
//!   direct post, a non-public boost — folds nothing, and an `Undo` signed
//!   by another actor withdraws nothing;
//! - an unknown remote object is fetched and ingested before the fold, a
//!   gone answer folds nothing, and a `Like` carrying one emoji is the
//!   Misskey reaction in disguise, while `Dislike` folds nothing at all.
//!
//! [`handle_incoming`]: wired_be::federation::dispatch::handle_incoming

#![cfg(test)]

mod support;
