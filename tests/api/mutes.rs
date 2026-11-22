//! Integration tests for the mutes endpoints (the mutes ticket):
//! `POST /api/v1/accounts/{id}/mute` stores the pair with Mastodon's two
//! parameters and answers the caller's relationship with `muting: true` and
//! `muting_notifications` as the request left them; `…/unmute` lifts the mute
//! idempotently; muting yourself is the 422 the ticket pins and any other
//! account miss the shared 404; a mute whose `duration` has run out is absent
//! from the flags and from the listing before its row is even swept; re-muting
//! a pair rewrites its options and keeps its row. `GET /api/v1/mutes` answers
//! the caller's muted accounts as `Account` entities, newest mute first,
//! keyset-paginated over the *mute row* id with Pleroma's `Link` header and
//! Mastodon's 80-row cap, each entry embedding the caller's relationship and
//! carrying the extra top-level `mute_expires_at` key — a Mastodon time, or
//! `null` for an indefinite mute. The `mutes.expire` job deletes the lapsed
//! rows the reads already ignore. Every route half of the auth and scope
//! pipelines is pinned: the listing with `read:mutes`, the two writes with
//! `write:mutes | follow`.

#![cfg(test)]

mod support;
