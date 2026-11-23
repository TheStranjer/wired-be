//! Fetching remote actors: the HTTP dereference of an actor document and
//! the get-or-fetch mirrors built on top of it.
//!
//! [`fetch_actor_json`] is the wire half: a JSON-asking, size-capped,
//! optionally signed `GET` that returns only a document which vouches for
//! the URL it was fetched from. The get-or-fetch functions are the policy
//! half: consult the local `users` mirror first (decision 7), refresh a
//! remote mirror no more than once a day, keep serving the stale mirror
//! when a peer misbehaves, and resolve `acct` handles through
//! [`WebFinger`](crate::federation::webfinger_client) when the database
//! knows nothing. The reference behaviour is Pleroma's
//! `Pleroma.Object.Fetcher.fetch_and_contain_remote_object_from_id/1`
//! (`lib/pleroma/object/fetcher.ex`) and `Pleroma.User`'s
//! `get_or_fetch_by_ap_id/1`, `get_or_fetch_by_nickname/1` and the 24-hour
//! `needs_update?/1` window (`lib/pleroma/user.ex`); the ticket rules here
//! are authoritative where the two differ.
//!
//! Two containment rules keep a hostile host from putting words in another
//! instance's mouth (Mastodon's round trip, Pleroma's
//! `Containment.contain_origin_from_id/2`):
//!
//! - A fetched document's `id` must name the URL that actually served it.
//!   The shared [`HttpClient`](crate::http_client::HttpClient) follows
//!   redirects and reports the final URL: a redirect to another host is
//!   [`FetchError::Invalid`], and a document naming anything but the final
//!   URL is dereferenced exactly once at its own `id` — without a redirect
//!   allowed — and accepted only if it names *that* URL. A moved document
//!   is re-anchored to its canonical id, a document that keeps moving is
//!   [`FetchError::Invalid`].
//! - A handle resolved through `WebFinger` must be vouched for by the actor
//!   itself: the fetched document's `preferredUsername@host` has to equal
//!   the requested `acct`, or the lookup fails as
//!   [`FetchError::AcctMismatch`].

use chrono::Duration;

pub mod containment;
mod error;
pub mod fetch;
pub mod lookup;
pub mod signer;
#[cfg(test)]
mod test_support;

pub use error::FetchError;
pub use fetch::fetch_actor_json;
pub use lookup::{get_or_fetch_by_acct, get_or_fetch_by_ap_id, refresh_user};
pub use signer::Signer;

pub(crate) use self::{
    containment::{ensure_not_redirected, ensure_same_host, names_served_url},
    fetch::fetch_document,
    lookup::is_local_ap_id,
    signer::fetch_signer,
};

/// Cap on one fetched actor document: 1 MiB. Actor documents are profiles;
/// anything larger is an attack or a mistake.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// The `Accept` header every federation `GET` asks with: both
/// `ActivityPub` JSON media types, the `ld+json` one pinned to the
/// `ActivityStreams` profile (Mastodon and Pleroma serve on either
/// spelling). Shared with the object fetcher (see
/// [`crate::federation::fetch_object`]), which asks the same way.
pub(crate) const ACTOR_ACCEPT: &str = "application/activity+json, application/ld+json; \
                            profile=\"https://www.w3.org/ns/activitystreams\"";
/// How long a stored remote mirror answers without a refetch: Pleroma's
/// `needs_update?/1` 24-hour window.
const REFRESH_AFTER: Duration = Duration::hours(24);
/// Status of an actor that does not exist.
const STATUS_NOT_FOUND: u16 = 404;
/// Status of an actor that is permanently gone (Pleroma's and Mastodon's
/// tombstone answer; the one refresh failure that must not hide behind a
/// stale mirror).
const STATUS_GONE: u16 = 410;
/// Substring every JSON-family media type the peers answer with shares.
const JSON_MARKER: &str = "json";
