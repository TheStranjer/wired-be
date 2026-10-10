//! The `ActivityPub` federation layer (`docs/ARCHITECTURE.md` decision 3):
//! everything that speaks the wire protocol with other instances.
//!
//! Today this is outbound authentication only, the [`signature`] module that
//! creates the HTTP Signatures (draft-cavage-http-signatures-12) Mastodon
//! and Pleroma expect on every fetch and delivery. Inbound signature
//! verification, activity documents, inboxes and outboxes arrive in later
//! tickets and get their modules beside it.

pub mod signature;
