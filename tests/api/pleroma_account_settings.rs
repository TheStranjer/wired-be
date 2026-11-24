//! Account utilities (`/api/pleroma/aliases`, `/api/pleroma/move_account`
//! from WBE-137; `/api/pleroma/change_password`, `/api/pleroma/change_email`,
//! `/api/pleroma/delete_account`, `/api/pleroma/disable_account` from
//! WBE-136) over the real router, plus the Mastodon twin of the move,
//! `POST /api/v1/accounts/move` — same gates in the same order, same
//! `Move`/`Update` federation, answered with the caller's `Relationship`
//! entity and proper 4xx refusals.
//!
//! The alias tests walk the full add → list → remove lifecycle and pin
//! the render rules (handle for a mirrored actor, raw URL otherwise,
//! append-if-absent); the move tests pin Pleroma's gate order (password,
//! target resolution, `alsoKnownAs` precondition, 30-day cooldown), the
//! follower transfer (local accepted followers move, opted-out and
//! remote ones stay), the counters that move with it, the entity's
//! nested non-recursive `moved` render, the `Move` activity the completed
//! move queues for the federation fan-out (a refusal queues nothing), and
//! every route's `AuthUser` + scope pipeline. The credential tests pin the
//! 200-with-error shape of every password/validation refusal, which tokens
//! survive which action, the email-confirmation branch and its queued
//! letter, and the delete endpoint's config gate, `deleted_at` mark and
//! enqueued `users.delete` sweep (the sweep itself is exercised
//! handler-direct in `src/jobs/deletion.rs`).

#![cfg(test)]

mod aliases;
mod credentials;
mod departures;
mod mastodon_move;
mod move_follows;
mod moves;
mod refetch;
mod support;
