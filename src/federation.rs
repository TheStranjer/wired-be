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
//! authenticates inbound ones — the `GET` side of that authentication is
//! [`authorized_fetch`], the gate `AUTHORIZED_FETCH` puts in front of the
//! `ActivityPub` read routes.
//! [`json`] holds the shared JSON-LD shape normalizers those readers use.
//! [`delivery`] is the way back out: the `federator.deliver` job that signs
//! an activity as its actor and POSTs it to a remote inbox, retrying the
//! answers that may yet succeed. [`inbox_auth`] is its inbound counterpart:
//! the `SignedActivity` extractor that authenticates a peer's inbox `POST`
//! against its actor's key before any handler sees the activity, and
//! [`dispatch`] is what the inbox routes do with it once authenticated —
//! the `federator.incoming` job handler, a documented stub until the
//! dispatcher ticket fills it in. Activity documents, inboxes and outboxes
//! arrive in later tickets and get their modules beside them.

pub mod actor;
pub mod authorized_fetch;
pub mod delivery;
pub mod dispatch;
pub mod fetch_actor;
pub mod inbox_auth;
pub mod json;
pub mod remote_actor;
pub mod signature;
pub mod webfinger_client;
