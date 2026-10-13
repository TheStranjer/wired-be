//! The `ActivityPub` federation layer (`docs/ARCHITECTURE.md` decision 3):
//! everything that speaks the wire protocol with other instances.
//!
//! Today this is the [`signature`] module: the HTTP Signatures
//! (draft-cavage-http-signatures-12) Mastodon and Pleroma expect on every
//! fetch and delivery, both directions — [`signature::sign_request`] signs
//! outbound requests, [`signature::verify`] authenticates inbound ones.
//! Activity documents, inboxes and outboxes arrive in later tickets and get
//! their modules beside it.

pub mod signature;
