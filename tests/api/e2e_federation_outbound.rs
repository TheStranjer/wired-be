//! End-to-end federation of outbound content (WBE-224 scenario 4): signed,
//! correctly addressed deliveries, dedupe and retries over real HTTP.
//!
//! Every scenario builds a real world through this instance's public API —
//! a local author with keys, a mock actor that followed her through the
//! inbox path — performs the local write over HTTP, drains the queues with
//! [`common::run_all_jobs`] and reads the facts off the [`remote::MockPeer`]'s
//! captured deliveries: the inbox path the bytes arrived at, the `Signature`
//! / `Digest` / `Host` / `Date` headers they arrived with, the verification
//! of that signature against the local actor's own published key, and the
//! activity JSON's `to` / `cc` / `actor` / `object` lines. Addressing is
//! asserted positively from the capture, never off job counts alone;
//! nothing waits on a clock — a retry scenario backdates the job row and
//! drains again rather than sleeping.
//!
//! What the scenarios pin:
//!
//! - the `Create`, `Update` and `Delete` of a status, the `Announce` of a
//!   boost and its `Undo`, the `Like` of a favourite and its `Undo`, and
//!   the `EmojiReact` of a reaction and its `Undo` — each a signed delivery
//!   with the visibility-specific `to`/`cc` split for public, unlisted,
//!   followers-only (`private`) and `direct` posts, and with `bto`/`bcc`
//!   never on the wire;
//! - a `local`-visibility post, edit and delete that deliver nothing at all,
//!   and a direct post that reaches only the mentioned remote's personal
//!   inbox — no shared-inbox copy, no follower fan-out;
//! - shared-inbox dedupe: two followers on one host cost one delivery per
//!   activity, keyed `deliver:{activity id}:{inbox}`, and a restated
//!   reaction while its delivery is still live never sends twice;
//! - the profile `Update` behind `PATCH /api/v1/accounts/update_credentials`
//!   with the freshly rendered actor document (display name, note, avatar,
//!   profile fields), and its per-account debounce key;
//! - the `Move` and moved-document `Update` behind
//!   `POST /api/v1/accounts/move`, the forwarded `Flag` behind
//!   `POST /api/v1/accounts/{id}/flag` spoken by the instance actor and
//!   filed into the admin queue, and the `Add` / `Remove` behind a
//!   list-membership write, `target` the list's curated inbox and the
//!   member's personal inbox the only recipient;
//! - a `500` retried on the queue's own backoff and a `503` at its
//!   `Retry-After` moment, both resending the exact same signed bytes; a
//!   `410 Gone` discarded for good, its host marked unreachable so later
//!   posts skip the address entirely; and a peer that answers `401` like a
//!   follower whose key no longer verifies — refused once, retried, then
//!   discarded, the valid signature it was sent with still in the capture.
//!
//! The per-endpoint entity rules belong to the area files; the queue-level
//! fan-out computations to `status_federation.rs` and the handshake round
//! trips to `e2e_federation_follow.rs`; this module owns the content bytes
//! that actually left the process.
//!
//! The layout: one submodule per scenario family — `notes`,
//! `interactions`, `profile_events`, `lists` and `retries` — with
//! [`support`] holding the consts, fixtures and drivers every journey
//! rides and [`expectations`] the delivery assertion toolkit.
//!
//! [`common::run_all_jobs`]: crate::common::run_all_jobs

#![cfg(test)]

/// The delivery assertion toolkit: what one captured delivery must look
/// like, the addressing rows of the visibility matrix, the counts that find
/// one activity in the mock's capture and the lifecycle asserts.
mod expectations;
/// The boost, the favourite and the emoji reaction with their withdrawals,
/// and the shared-inbox dedupe two followers on one host collapse into.
mod interactions;
/// List membership: the `Add` and `Remove` a curated list announces of a
/// remote member, delivered to the member's personal inbox alone.
mod lists;
/// The note lifecycle and the visibility matrix: `Create`, `Update` and
/// `Delete` over public, unlisted, followers-only, direct and `local`.
mod notes;
/// The profile events: the actor-document `Update` and its debounce, the
/// `Move` and its re-read actor, and the forwarded `Flag`.
mod profile_events;
/// The retry ladder: the `500` and the `503` that resend the same signed
/// bytes, the `410` that drops the address for good, and the refused
/// signature retried once before the discard.
mod retries;
/// Shared consts, the fixtures and HTTP drivers, and the queue readers
/// every scenario rides.
mod support;
