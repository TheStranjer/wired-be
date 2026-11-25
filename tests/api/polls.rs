//! Poll parameters on `POST /api/v1/statuses` over HTTP: the validation
//! and the storage half of the polls tickets (WBE-93), and the poll's own
//! routes (WBE-106): `GET /api/v1/polls/{id}` answering the [`Poll`]
//! entity for its viewer, and `POST /api/v1/polls/{id}/votes` casting the
//! ballot — tested below after the create-side tests.
//!
//! The read is optionally authenticated (`read:statuses` on a presented
//! user token) and shows the viewer's own `voted`/`own_votes`; the poll's
//! status is the visibility gate, so an unknown, malformed or invisible
//! poll is the shared 404 on both routes. The vote needs
//! `write:statuses`, rides the `statuses_actions` bucket, and refuses
//! with the ticket's pinned 422 bodies, in order: a closed poll "Poll's
//! finished", a second ballot "You have already voted on this poll", the
//! author "Poll's author can't vote", an empty body "Missing parameter:
//! choices", an out-of-range index "Invalid indices", and a second
//! distinct choice on a single-choice poll "Too many choices" — with
//! duplicate indexes ignored. A passing ballot is one transaction
//! writing `poll_votes` and moving every count the entity reports.
//! `hide_totals` lifts at the deadline on this route too.
//!
//! [`Poll`]: wired_be::entities::poll::Poll
//!
//! A create body may carry a `poll` — JSON's `"poll": {"options": [...],
//! "expires_in": …, "multiple": …, "hide_totals": …}` or the form's nested
//! `poll[options][]` / `poll[expires_in]` / `poll[multiple]` /
//! `poll[hide_totals]` keys, which the extractor nests into the same
//! shape. `wired_be::services::create_status::check_body` runs Pleroma's
//! `CommonAPI` rules over it: at least two options, at most
//! `MAX_POLL_OPTIONS` (20 by default), no empty option and none past
//! `MAX_POLL_OPTION_CHARS` (200 by default), `poll[expires_in]` inside the
//! `MIN_POLL_EXPIRATION` to `MAX_POLL_EXPIRATION` window (300 seconds to
//! one year by default), and no non-empty `media_ids` beside a poll. Each
//! refusal is a 422 with the message the service pins; duplicates and a
//! `direct` audience are allowed.
//!
//! An accepted poll is stored: the create writes the `polls` row and its
//! `poll_options` rows on the status's own transaction, and the fresh
//! entity answers the whole `Poll` — the stored row's id, the options in
//! submission order at zero votes, the deadline the posted `expires_in`
//! counts out from now, and the posted flags. `hide_totals` withholds the
//! per-option counts (JSON `null`) while the poll is open; the roll-up
//! columns always render. The rest of the storage contract tested here:
//! a `preview` with a poll checks and answers without inserting, a poll
//! create replayed under one `Idempotency-Key` leaves exactly one status
//! and one poll, and a body with no poll stores no polls row at all.
//! `media_ids` keeps the attachments ticket's meaning: beside a poll it
//! is the 422 `Polls cannot have attachments`, on its own it claims the
//! poster's uploads onto the post — and either way a poll-less body
//! stores no `polls` row.

#![cfg(test)]

/// The accepted create: what a stored poll keeps, renders and counts.
mod create;
/// The create's refusals: every 422 the poll checks pin.
mod create_refusals;
/// `Poll.emojis`: the custom emoji option titles use.
mod emojis;
/// The read route: the entity, the shared 404 and the token gate.
mod read;
/// Shared consts and fixture helpers every submodule of this area draws on.
mod support;

// ————————————————— WBE-106: the poll's own routes —————————————————
//
// These modules drive `GET /api/v1/polls/{id}` and
// `POST /api/v1/polls/{id}/votes` — the read, the ballot, every refusal
// the ticket pins, and the counters a passing vote moves.

/// The passing ballots: the marks a vote leaves and the counts it moves.
mod vote;
/// The vote's refusals, its gates, the hidden totals and the throttle.
mod vote_refusals;
/// Shared consts and helpers for the poll routes' tests.
mod vote_support;
