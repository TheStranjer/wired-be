//! Scheduled statuses over HTTP (WBE-99, WBE-107): the parked posts, from the
//! arm that parks them to the four routes that manage them.
//!
//! The first half is the `scheduled_at` arm of `POST /api/v1/statuses`: a
//! future instant parks the whole create instead of writing a status.
//!
//! The arm runs the create's own body checks first — the empty text, the
//! unknown scope, the `expires_in` floor and the shared reply 404 are the
//! same answers they are on a plain post — and then the schedule's own
//! gates: `scheduled_at` must sit at least `MIN_SCHEDULED_SECS` (300 by
//! default, the refusal naming the configured minutes) ahead of now, the
//! account may hold at most `MAX_SCHEDULED_PER_USER` pending rows and at
//! most `MAX_SCHEDULED_PER_USER_PER_DAY` within the UTC day of the fire,
//! and every `media_ids` entry must be the poster's own upload no status
//! has claimed and no pending schedule reserves — unknown, foreign,
//! attached and already-reserved ids all one 422 `Media not found`.
//!
//! What passes becomes exactly one `scheduled_statuses` row — the
//! normalised request as its params jsonb (trimmed text, resolved
//! defaults, deduplicated id strings, and the `idempotency` and
//! `application_id` echo keys) — plus one
//! `scheduled_statuses.publish` job on the `scheduled` queue due at the
//! submitted instant keyed `sched:<id>`. Nothing else happens: no
//! `statuses` row, no expiration job, no claimed idempotency key, and no
//! `media_attachments` column moves — the upload is reserved purely by
//! living in the pending row's params. The answer is 200 with the
//! `ScheduledStatus` entity (the params echoed as `text`, the
//! `scheduled_at` and `poll` keys pinned to `null`), its
//! `media_attachments` read back in the order the body submitted them.
//! Form and query bodies schedule the same post as JSON, the schedule
//! ignores an `Idempotency-Key` beyond echoing it, and the gate is
//! `write:statuses` like the create itself (401 anonymous, 403 without
//! the scope).
//!
//! The second half is `/api/v1/scheduled_statuses`: the listing, one
//! schedule, the move and the cancel, over the rows the arm above parks.
//! The listing is the caller's own rows keyed and ordered on the id —
//! newest created first, the due time nowhere in it, as Pleroma's
//! `Pagination.fetch_paginated` leaves them — 20 rows by default and 40 at
//! the most, with the `Link` header walking from both ends of the page. A
//! move takes `scheduled_at` (JSON, form or query alike), owes the create's
//! window for it, leaves the parked request exactly as it was stored, and
//! carries its fire along: one waiting publish job, due at the new moment
//! and a fresh row for it. A cancel takes the row and that job with it and
//! answers `{}`. Somebody else's schedule, an id the table never held and
//! id text that is no id are one 404 `Record not found` on all three, and
//! the gate is Pleroma's pair: `read:statuses` to look, `write:statuses` to
//! move and cancel, 401 anonymous.
//!
//! The area is split below: `support` and `routes_support` hold the consts
//! and fixture helpers of the two halves, the four test files hold the
//! tests themselves.

#![cfg(test)]

/// The listing and the single schedule: order, cursors, page size and the
/// shared 404.
mod listing_show;
/// The move and the cancel: the row and its fire, and refusals that leave
/// both alone.
mod move_cancel;
/// Ownership and the scope pair: whose schedule a token may reach at all.
mod route_gates;
/// Shared consts and fixture helpers for the `/api/v1/scheduled_statuses`
/// route submodules.
mod routes_support;
/// The scheduling arm's own answers: the entity, the window, the create's
/// refusals, the gate and the body shapes.
mod schedule_create;
/// The scheduling arm's limits: the total cap, the per-day cap and the
/// media reservation.
mod schedule_limits;
/// Shared consts and fixture helpers for the scheduling-arm submodules.
mod support;
