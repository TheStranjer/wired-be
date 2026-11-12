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
//! the `federator.incoming` job handler: normalization, containment, the
//! dedupe ledger, the policy stage, and the registry of per-type activity
//! handlers. [`mrf`] is that handler's policy pipeline: the
//! accept-everything filter chain the MRF tickets will fill in.
//! [`note_parser`] is the dispatcher's pure
//! front end for objects: the reader that turns a received `Note`-family object into
//! the parts of a status. [`note`] is the way back out for statuses: the
//! pure builder of the `Note`/`Question` objects a local post federates as
//! and of the `Create`, `Announce` and `Tombstone` documents around it,
//! plus the JSON-LD schema they reference. [`activities`] is the same
//! pure build for the facts that are not statuses: the `Like`,
//! `EmojiReact`, `Follow`, `Block`, `Undo`, `Accept` and `Reject`
//! documents minted at `{BASE_URL}/activities/{uuid}`, ahead of the
//! route that will serve them. Activity collections, inboxes and
//! outboxes arrive in later tickets and get their modules beside them.

pub mod activities;
pub mod actor;
pub mod authorized_fetch;
pub mod delivery;
pub mod dispatch;
pub mod fetch_actor;
pub mod inbox_auth;
pub mod json;
pub mod mrf;
pub mod note;
pub mod note_parser;
pub mod remote_actor;
pub mod signature;
pub mod webfinger_client;
