//! Tests for the `ActivityPub` collection routes of
//! `src/api/activitypub.rs`, built by `src/federation/collections.rs`:
//! `GET /users/{nickname}/outbox` (and its `?page=true` keyset page),
//! `GET /users/{nickname}/followers` and `/following` (same two-level
//! structure, page size 10, keyed on the follow row id),
//! `GET /users/{nickname}/collections/featured`, and
//! `GET /objects/{uuid}/replies` with its page.
//!
//! The routes negotiate like the actor route (`ActivityPub` `Accept` gets
//! the document, a browser leaves with the frontend redirect, everyone
//! else gets the 404, every answer varies on `Accept`), apply the
//! `AUTHORIZED_FETCH` gate, and answer unknown and remote accounts like
//! the actor route's 404 and deactivated ones like its 410. The pure
//! document shapes are pinned where they are built, in
//! `src/federation/collections.rs`; what these tests own is the route:
//! the stored rows behind the items, the visibility and hide rules over
//! real data, the cursors' walk, the status codes and the headers.

#![cfg(test)]

mod follows_featured;
mod gates;
mod outbox;
mod replies;
mod support;
