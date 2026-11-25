//! The shared loopback mock federation peer (WBE-224 slice 1): one real HTTP
//! server a federation test points this instance at.
//!
//! Both halves of a round trip are real: the instance fetches this peer's
//! actors and delivers to its inboxes over the wire, and the peer posts signed
//! activities back through the instance's own inbox routes. A [`MockPeer`]
//! serves:
//!
//! - `GET /users/{nick}` — a self-identifying `Person` actor with a *real* RSA
//!   public key, a personal inbox under its own id and the peer-wide shared
//!   inbox in `endpoints`, so a delivery addressed to either lands in the
//!   capture and an inbound signature verifies against the key the document
//!   names ([`MockPeer::deliver`] signs with the matching private half);
//! - `GET /users/{nick}/outbox` — the actor's `OrderedCollection` root and,
//!   under `?page=true`, the pages of activity ids a test listed with
//!   [`MockPeer::publish`], walked with `next` / `prev` cursors;
//! - `GET /relay` — a `Service` actor (the shape relays are served as) whose
//!   inbox *is* the shared one, which is what lets a test watch the
//!   publisher's per-inbox deduplication;
//! - `GET /objects/{name}` — a public `Note` authored by the peer's own
//!   `author` account, the object a relay re-broadcasts, or whatever shaped
//!   document [`MockPeer::serve_note`] put at that name — the id this
//!   instance dereferences when it backfills a thread or resolves a status
//!   by `uri`;
//! - `GET /key` — a standalone `Key` object carrying the same
//!   `publicKeyPem` the actor documents embed: the mirror a fetcher reads
//!   when a `keyId` names the key rather than an actor;
//! - `GET /.well-known/webfinger` — a JRD echoing the asked `acct:` resource
//!   and naming that nickname's actor, so `acct:` handles resolve here;
//! - `POST /inbox` and `POST /users/{nick}/inbox` — both record the exact
//!   bytes, headers and path they were sent to and answer `202 Accepted`,
//!   or the next answer the test scripted with [`MockPeer::script_inbox`]
//!   / [`MockPeer::script_inbox_permanently`] (a `500`, a `503` with a
//!   `Retry-After`, a `410` that never lifts — whatever the scenario is
//!   making the delivery worker see);
//! - [`MockPeer::rotate_keys`] swaps the key pair every actor document
//!   publishes from then on, so a scenario can stand the peer in a key
//!   rotation and compare what this instance still holds against what is
//!   served now.
//!
//! Sending is [`MockPeer::deliver`], which signs as one of the peer's actors
//! with [`wired_be::federation::signature::sign_request`] and rides any path
//! the caller names through the test router in-process; [`MockPeer::activity`]
//! builds the documents with ids on the peer's own host, which is what the
//! dispatcher's containment check asks of them, and
//! [`MockPeer::note_document`] shapes the `Note` they wrap — the addressing,
//! `attachment`, `tag`, `sensitive` / `summary`, a `Question`'s `oneOf`.
//! Reading is
//! [`MockPeer::received`], [`MockPeer::delivered`] and
//! [`MockPeer::delivery_count`], and a [`Delivery`] proves its own authenticity
//! through [`Delivery::signature_verifies`] and [`Delivery::digest_matches`].
//!
//! [`wired_be::federation::signature::sign_request`]: wired_be::federation::signature::sign_request

#![cfg(test)]

mod support;

pub use self::support::{
    Delivery, INBOX, KEY_OWNER, MockPeer, PUBLIC, app_and_state, keyed_instance_actor,
    keyed_local_user, keyed_local_user_with, local_keys,
};
