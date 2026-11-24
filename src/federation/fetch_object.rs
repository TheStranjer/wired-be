//! Fetching remote objects: the signed dereference of a document and the
//! thread backfill built on top of it.
//!
//! [`fetch_object_json`] is the wire half: a JSON-asking, size-capped,
//! HTTP-signed `GET` that returns only a document which vouches for the
//! URL it was fetched from, capped at 2 MiB of body and three redirect
//! hops. [`resolve_status`] is the policy half: consult the local
//! `statuses` mirror first — including a status stored under this
//! instance's own `{BASE_URL}/objects/{uuid}` shape of the requested id —
//! and only a document that is a `Note`-family object with a resolvable
//! author is folded, through [`ingest_note`](crate::services::ingest).
//! The reference behaviour is Pleroma's
//! `Pleroma.Object.Fetcher.fetch_and_contain_remote_object_from_id/1` and
//! `Pleroma.Web.ActivityPub.Utils.get_obj_helper`
//! (`lib/pleroma/object/fetcher.ex`,
//! `lib/pleroma/web/activity_pub/utils.ex`); the ticket rules here are
//! authoritative where the two differ.
//!
//! Containment, the same posture this module's sibling
//! [`fetch_actor`](crate::federation::fetch_actor) keeps for actors:
//!
//! - The fetched document's `id` must name the URL it was fetched from.
//!   The object client follows at most `MAX_OBJECT_REDIRECTS` redirects
//!   internally and does not report the final URL, so a document naming
//!   the requested URL itself is accepted directly, and a document naming
//!   an id on the *same origin* is dereferenced exactly once at its own
//!   `id` and accepted only if it names that one — a document that keeps
//!   moving is [`FetchError::Invalid`].
//! - Origin containment (Pleroma's `Containment.contain_origin/1`,
//!   Mastodon's round trip): a document whose `id` names a *different*
//!   host than the URL fetched from vouches for nothing the asking host
//!   vouched for, and is [`FetchError::Invalid`]. An object never moves
//!   between instances the way an actor may.
//!
//! On top of the resolver sits `federator.backfill`: when ingest stores a
//! remote answer to an object this instance does not have, the row keeps
//! the parent id in `in_reply_to_ap_id` and this job walks up the chain —
//! [`resolve_status_at_depth`] for the parent, [`link_backfilled_reply`] to
//! close the link and step `replies_count`. The parent's own job is queued
//! by its ingest at `depth + 1`, so the depth rides in the job args from the
//! first reply on — until the thread is linked or `THREAD_FETCH_DEPTH`
//! ancestors are reached. A parent that turns out to be gone (404 or 410)
//! completes the job quietly: threads legitimately start mid-air.
//!
//! `federator.quote_backfill` does the same for quotes: a remote quote of an
//! object this instance does not have keeps the object's id in `quote_uri`,
//! and the job resolves it ([`resolve_quoted_status`]) and links the quote.
//! The quoted object is ingested without a backfill of its own, so quotes
//! cost one fetch and never chain.
//!
//! Every dereference runs against a per-run budget of
//! [`FetchLimits::max_fetches_per_activity`](crate::config::FetchLimits::max_fetches_per_activity)
//! fetches: one incoming activity (or one backfill job) fans out at most
//! that many object `GET`s, past which the fetch fails instead of
//! following the next reference.
//!
//! [`link_backfilled_reply`]: crate::models::status::link_backfilled_reply

mod backfill;
mod fetch;
mod quote_backfill;
mod resolve;
#[cfg(test)]
mod tests;

use std::{
    error::Error,
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{federation::fetch_actor::FetchError as ActorError, http_client::HttpError};

pub use self::{
    backfill::{BACKFILL_MAX_ATTEMPTS, backfill_key, handle, handle_backfill},
    fetch::fetch_object_json,
    quote_backfill::{
        QUOTE_BACKFILL_MAX_ATTEMPTS, handle_quote, handle_quote_backfill, quote_backfill_key,
    },
    resolve::{
        resolve_quoted_status, resolve_status, resolve_status_as_actor, resolve_status_at_depth,
    },
};

/// The [`JobRegistry`](crate::jobs::JobRegistry) key of the thread-backfill
/// job: one deferred reply link per run, args
/// `{"status_id": "<uuid>", "depth": <n>}`.
pub const KIND_BACKFILL: &str = "federator.backfill";

/// The [`JobRegistry`](crate::jobs::JobRegistry) key of the quote-backfill
/// job: fetches the object an unlinked remote quote names and links the
/// quote to it, args `{"status_id": "<uuid>"}`.
pub const KIND_QUOTE_BACKFILL: &str = "federator.quote_backfill";

/// How fetching a remote object can fail.
#[derive(Debug)]
pub enum FetchError {
    /// The request itself failed or the peer answered a status that is not
    /// one of the object-specific ones: the transport and status failures
    /// of [`crate::http_client`], a body past the cap included.
    Http(HttpError),
    /// The peer answered 410: the object is permanently gone. The resolver
    /// treats it exactly like a tombstone.
    Gone,
    /// The peer answered 404: there is no object behind the id. The
    /// resolver treats it like a thread that legitimately starts here.
    NotFound,
    /// The answer cannot become a status: a non-JSON content type, an
    /// unparsable document, a document that fails containment, or a run
    /// that spent its fetch budget. The string states which.
    Invalid(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(err) => write!(f, "object fetch failed: {err}"),
            Self::Gone => f.write_str("the object is gone"),
            Self::NotFound => f.write_str("the object was not found"),
            Self::Invalid(detail) => f.write_str(detail),
        }
    }
}

impl Error for FetchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(err) => Some(err),
            Self::Gone | Self::NotFound | Self::Invalid(_) => None,
        }
    }
}

/// Any transport or status failure of the shared client is a
/// [`FetchError::Http`], so `?` works throughout the fetch.
impl From<HttpError> for FetchError {
    fn from(err: HttpError) -> Self {
        Self::Http(err)
    }
}

/// The document-fetch steps of this module run on
/// [`fetch_actor`](crate::federation::fetch_actor)'s machinery; the
/// actor-specific refusals of that error vocabulary read as plain invalid
/// documents here.
impl From<ActorError> for FetchError {
    fn from(err: ActorError) -> Self {
        match err {
            ActorError::Http(http) => Self::Http(http),
            ActorError::Gone => Self::Gone,
            ActorError::NotFound => Self::NotFound,
            ActorError::Invalid(detail) => Self::Invalid(detail),
            ActorError::AcctMismatch => {
                Self::Invalid("the fetched document is not the requested account".to_owned())
            }
            ActorError::Webfinger(err) => {
                Self::Invalid(format!("account resolution failed: {err}"))
            }
        }
    }
}

/// The per-run budget of object dereferences: one incoming activity (or
/// one resolver run) spends a unit per document `GET` and refuses the
/// next once they run out. An atomic counter, not a [`std::cell::Cell`],
/// so the futures that carry it stay `Send` (see [`crate::jobs`]).
#[derive(Debug)]
struct FetchBudget {
    /// Dereferences still allowed.
    remaining: AtomicUsize,
}

impl FetchBudget {
    /// A budget of `limit` dereferences.
    const fn new(limit: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(limit),
        }
    }

    /// Takes one dereference: `false` when the budget is spent.
    fn take(&self) -> bool {
        self.remaining
            .try_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .is_ok()
    }
}
