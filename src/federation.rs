//! The `ActivityPub` federation layer (`docs/ARCHITECTURE.md` decision 3):
//! everything that speaks the wire protocol with other instances.
//!
//! Today this is the actor document ([`actor`]) and its inbound counterpart,
//! the remote-actor parser and mirror ([`remote_actor`]), the outbound
//! [`webfinger_client`] that resolves an `acct:` handle to an actor URL,
//! [`fetch_actor`], which dereferences remote actor documents over HTTP and
//! keeps the user mirrors fresh, plus the [`signature`] module holding the
//! HTTP Signatures (draft-cavage-http-signatures-12) Mastodon and Pleroma
//! expect on every fetch and delivery, both directions:
//! [`signature::sign_request`] signs outbound requests, [`signature::verify`]
//! authenticates inbound ones.
//! [`json`] holds the shared JSON-LD shape normalizers those readers use.
//! Activity documents, inboxes and outboxes arrive in later tickets and get
//! their modules beside them.

pub mod actor;
pub mod fetch_actor;
pub mod json;
pub mod remote_actor;
pub mod signature;
pub mod webfinger_client;
