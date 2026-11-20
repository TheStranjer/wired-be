//! Tests for the `ActivityPub` routes: `GET /users/{nickname}`, the actor
//! document, the JSON-LD schema document at
//! `GET /schemas/litepub-0.1.jsonld`, the three inbox `POST`s, and the
//! client-to-server four — `GET /users/{nickname}/inbox`,
//! `POST /users/{nickname}/outbox`, `GET /api/ap/whoami` and
//! `POST /api/ap/upload_media`.
//!
//! An `ActivityPub` client (an `Accept` naming `application/activity+json`
//! or `application/ld+json`) gets Pleroma's `user.json` document as
//! `application/activity+json; charset=utf-8`; a browser (`Accept` naming
//! `text/html`, no `ActivityPub` type) gets Pleroma's 302 to the frontend
//! profile URL; anyone else gets 404 `{"error":"Not found"}`. An unknown
//! or remote account gets the same 404 to an `ActivityPub` ask, and a
//! deactivated local account gets 410 Gone. Every answer says it varies
//! on `Accept`.
//!
//! The inboxes (`POST /inbox`, `POST /users/{nickname}/inbox` and
//! `POST /internal/fetch/inbox`) accept a signed delivery with 202 and an
//! empty body and queue exactly one `federator.incoming` job for it —
//! once per activity id however often it is redelivered — while an unknown,
//! remote or deactivated owner is the 404, an activity without the
//! required fields is the 400, an unsigned delivery the extractor's 401,
//! and every delivery a `FEDERATION_ENABLED`-off 403.
//!
//! The renderer's corner cases are covered where they live, in
//! `src/federation/actor.rs`; what these tests own is the route: the
//! negotiation, the headers, the status codes and the document of a stored
//! user end to end. The signature codec is covered in
//! `src/federation/signature/`; what these tests own here is that an
//! authenticated delivery becomes a queue row.
//!
//! The area is one module per surface: [`actors`] for the schema and actor
//! documents, [`inbox`] for the three inbox `POST`s, the four
//! `c2s_*` files for the client-to-server routes, and [`support`] for the
//! consts and helpers they share.
//!
//! [`actors`]: actors
//! [`inbox`]: inbox
//! [`support`]: support

#![cfg(test)]

/// The JSON-LD schema document and the actor documents served at
/// `GET /users/{nickname}`: the whole `user.json` shape, its key set and
/// variations, the `Accept` negotiation and the 404 and 410 answers.
mod actors;

/// The client-to-server inbox read (`GET /users/{nickname}/inbox`): its
/// gates, the collection root and the paged mentions.
mod c2s_inbox;

/// The client-to-server outbox gates, the `Create` of a note and the
/// `Delete` of the caller's own post.
mod c2s_outbox_create;

/// The client-to-server outbox `Follow` and its `Undo`s.
mod c2s_outbox_follows;

/// The client-to-server outbox `Like`, `Announce` and `Block` and their
/// `Undo`s.
mod c2s_outbox_interactions;

/// The client-to-server `GET /api/ap/whoami` and `POST /api/ap/upload_media`.
mod c2s_whoami_upload;

/// The three inbox `POST`s: signed deliveries queued as
/// `federator.incoming` jobs, and the gates that refuse them.
mod inbox;

/// The consts and helpers every submodule of this area shares.
mod support;
