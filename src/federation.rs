//! The `ActivityPub` federation layer (`docs/ARCHITECTURE.md` decision 3):
//! everything that speaks the wire protocol with other instances.
//!
//! Today this is the actor document ([`actor`]) and its inbound counterpart,
//! the remote-actor parser and mirror ([`remote_actor`]), the outbound
//! [`webfinger_client`] that resolves an `acct:` handle to an actor URL,
//! [`fetch_actor`], which dereferences remote actor documents over HTTP and
//! keeps the user mirrors fresh, [`fetch_object`], its object counterpart:
//! the signed dereference of a remote `Note`-family document, the resolver
//! that turns such a fetch into a stored status, and the
//! `federator.backfill` job that walks a fetched reply's ancestry to close
//! deferred thread links, plus the [`signature`] module holding the
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
//! handlers, whose code lives beside it in [`handlers`]: [`handlers::actor`]
//! folds the inbound `Update` and `Delete` of a remote account, and the
//! create/note tickets add their files to that module's registry as they
//! arrive. [`mrf`] is that handler's policy pipeline: the filter chain
//! of [`MrfPolicy`](mrf::MrfPolicy) policies configured by the `mrf`
//! instance setting — the no-op stage and [`mrf::simple`], the domain
//! lists that reject, quarantine and strip by origin host — with further
//! MRF policy tickets registering one name each. Actor documents run
//! through that chain's other stage, [`filter_actor`](mrf::filter_actor),
//! before they are mirrored.
//! [`note_parser`] is the dispatcher's pure
//! front end for objects: the reader that turns a received `Note`-family object into
//! the parts of a status. [`note`] is the way back out for statuses: the
//! pure builder of the `Note`/`Question` objects a local post federates as
//! and of the `Create`, `Announce` and `Tombstone` documents around it,
//! plus the JSON-LD schema they reference. [`activities`] is the same
//! pure build for the facts that are not statuses: the `Like`,
//! `EmojiReact`, `Follow`, `Block`, `Undo`, `Accept` and `Reject`
//! documents minted at `{BASE_URL}/activities/{uuid}`, served beside the
//! status `Create`s and `Announce`s by the activity reads of
//! [`crate::api::activitypub`]. [`collections`] builds the
//! activity collections the actor document names — outbox, followers,
//! following, featured and an object's replies — roots and keyset pages
//! alike. [`publisher`] is the fan-out in front of delivery: it turns an
//! activity's addressing — the actor's followers collection expanded to
//! its remote followers included — into the set of remote inboxes to
//! deliver to and queues one [`delivery`] job each, and its
//! `federator.publish` job kind lets API handlers hand that work to the
//! queue instead of waiting on it. The personal inbox `POST`s arrived with [`inbox_auth`] and
//! [`dispatch`]; `GET /users/{nickname}/inbox`, the client read of a
//! personal inbox, belongs to a later ticket.

pub mod activities;
pub mod actor;
pub mod authorized_fetch;
pub mod collections;
pub mod delivery;
pub mod dispatch;
pub mod fetch_actor;
pub mod fetch_object;
pub mod handlers;
pub mod inbox_auth;
pub mod json;
pub mod mrf;
pub mod note;
pub mod note_parser;
pub mod policy;
pub mod publisher;
pub mod remote_actor;
pub mod signature;
pub mod vocab;
pub mod webfinger_client;
