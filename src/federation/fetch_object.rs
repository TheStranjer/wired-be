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
//! [`resolve_status`] for the parent, [`link_backfilled_reply`] to close
//! the link and step `replies_count`, then the parent's own job at
//! `depth + 1` — until the thread is linked or `THREAD_FETCH_DEPTH`
//! ancestors are reached. A parent that turns out to be gone (404 or 410)
//! completes the job quietly: threads legitimately start mid-air.
//!
//! Every dereference runs against a per-run budget of
//! [`FetchLimits::max_fetches_per_activity`](crate::config::FetchLimits::max_fetches_per_activity)
//! fetches: one incoming activity (or one backfill job) fans out at most
//! that many object `GET`s, past which the fetch fails instead of
//! following the next reference.
//!
//! [`link_backfilled_reply`]: crate::models::status::link_backfilled_reply

use std::{
    error::Error,
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
};

use serde_json::{Value, json};
use tracing::debug;
use url::Url;
use uuid::Uuid;

use crate::{
    config::Config,
    federation::{
        dispatch::FederationError,
        fetch_actor::{
            FetchError as ActorError, Signer, fetch_document, fetch_signer, get_or_fetch_by_ap_id,
            is_local_ap_id,
        },
        json,
        note_parser::OBJECT_TYPES,
    },
    http_client::{HttpClient, HttpError, user_agent},
    jobs::{AttemptInfo, HandlerFuture, JobError, QUEUE_BACKGROUND},
    models::{
        job::{self, NewJob},
        status::{self, Status},
        user::User,
    },
    services::ingest::{self, IngestOutcome},
    state::AppState,
};

/// The [`JobRegistry`](crate::jobs::JobRegistry) key of the thread-backfill
/// job: one deferred reply link per run, args
/// `{"status_id": "<uuid>", "depth": <n>}`.
pub const KIND_BACKFILL: &str = "federator.backfill";
/// Cap on one fetched object document: 2 MiB. Notes carry content, tags
/// and attachment lists; anything larger is an attack or a mistake.
const MAX_OBJECT_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Redirects one object `GET` follows. Tighter than the shared client's
/// budget on purpose: an object id that needs more hops to resolve is not
/// resolving (see the module docs).
const MAX_OBJECT_REDIRECTS: usize = 3;
/// The failure budget of a `federator.backfill` job: three tries, then the
/// thread stays as stored — Pleroma's `RemoteFetcherWorker` gives up on a
/// parent the same way.
pub(crate) const BACKFILL_MAX_ATTEMPTS: i32 = 3;
/// The unique-key prefix of a backfill job, followed by the child status'
/// id: at most one live backfill per status, the delivery ledger's shape.
const BACKFILL_KEY_PREFIX: &str = "backfill:";
/// Human-facing url path segment of a local status (see
/// [`crate::models::status`]): a thread link naming this instance's
/// `/notice/{uuid}` resolves against the `/objects/{uuid}` the row is
/// stored under.
const NOTICE_PATH: &str = "/notice/";
/// Object id path segment [`resolve_status`] also matches locally.
const OBJECTS_PATH: &str = "/objects/";

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

/// Dereferences a remote object over HTTP with a fresh fetch budget.
///
/// The `GET` asks for both `ActivityPub` JSON media types, follows at most
/// `MAX_OBJECT_REDIRECTS` redirects, caps the body at 2 MiB and refuses
/// an answer whose content type does not say `json`; with `SIGN_FETCHES`
/// on it carries the instance actor's HTTP Signature, like every other
/// federation fetch (see `fetch_signer`).
///
/// The returned document is contained as the [module docs](self) describe:
/// its `id` names the fetched URL — or, on the same origin, a document
/// fetched once at its own `id` that names itself.
///
/// # Errors
///
/// [`FetchError::Gone`] for a 410, [`FetchError::NotFound`] for a 404,
/// [`FetchError::Http`] for any other transport or status failure
/// (including a body over the cap), and [`FetchError::Invalid`] for a
/// non-JSON content type, a body that is not parseable JSON with a string
/// `id`, a document that fails containment, or a fetch past the budget.
pub async fn fetch_object_json(state: &AppState, url: &str) -> Result<Value, FetchError> {
    let budget = FetchBudget::new(state.config.fetch.limits.max_fetches_per_activity);
    fetch_contained(state, url, &budget).await
}

/// Resolves `uri` to a stored status, fetching the remote object when this
/// instance does not have one.
///
/// The database comes first: a live row answers as stored, and a
/// `{BASE_URL}/notice/{uuid}` form of a local id matches the row stored
/// under the `{BASE_URL}/objects/{uuid}` shape too. An unknown id on this
/// instance's own origin resolves to nothing — its document would be
/// served by this very process. Otherwise the object is dereferenced
/// (with the id and author dereferences of its ingest inside the same
/// fetch budget), must be a `Note`-family object, and is folded through
/// [`ingest_note`](crate::services::ingest) authored by its
/// `attributedTo` account, fetched through
/// [`get_or_fetch_by_ap_id`].
///
/// # Errors
///
/// [`FederationError::Transient`] when the network or the database says
/// *not now*; a retry may well resolve what this pass could not — the
/// transport half of an object fetch and every author-mirror failure ride
/// on that arm.
///
/// [`FederationError::Permanent`] when the peer answers a well-formed
/// refusal (a non-JSON body, a document that fails containment or parsing)
/// or the run spent its fetch budget.
///
/// `Ok(None)` when nothing stands behind `uri`: this instance's own
/// unknown id, a 404 or 410 (the object is gone or never was), a document
/// that is not a `Note`-family object, one with no author, or one the
/// ingest gate drops (tombstoned, deactivated or unsolicited).
pub async fn resolve_status(
    state: &AppState,
    uri: &str,
) -> Result<Option<Status>, FederationError> {
    let budget = FetchBudget::new(state.config.fetch.limits.max_fetches_per_activity);
    resolve_uri(state, uri, None, &budget).await
}

/// [`resolve_status`] for the `Create` handler: the fetched object is
/// ingested only when its author *is* `actor` — the delivering account —
/// so a peer cannot put somebody else's post into this instance's database
/// by naming it in a bare-id `Create`. A document authored elsewhere (or
/// not authored at all) resolves to nothing, without a fetch of that
/// other author.
pub(crate) async fn resolve_status_as_actor(
    state: &AppState,
    uri: &str,
    actor: &User,
) -> Result<Option<Status>, FederationError> {
    let budget = FetchBudget::new(state.config.fetch.limits.max_fetches_per_activity);
    resolve_uri(state, uri, Some(actor), &budget).await
}

/// The whole resolver: local mirror, containment-checked fetch, `Note`
/// family check, authoring, ingest. `as_actor` selects whose post may be
/// folded — the object's own author's mirror, or only the named
/// [`User`]'s (see [`resolve_status_as_actor`]).
async fn resolve_uri(
    state: &AppState,
    uri: &str,
    as_actor: Option<&User>,
    budget: &FetchBudget,
) -> Result<Option<Status>, FederationError> {
    let stored = status::get_by_uri(&state.db, uri)
        .await
        .map_err(|err| transient("the status uri read", &err))?;
    if let Some(row) = stored {
        return Ok(live_or_deleted(row));
    }
    if let Some(alias) = local_object_uri(&state.config, uri) {
        let aliased = status::get_by_uri(&state.db, &alias)
            .await
            .map_err(|err| transient("the status object-id read", &err))?;
        if let Some(row) = aliased {
            return Ok(live_or_deleted(row));
        }
    }
    if is_local_ap_id(&state.config, uri) {
        debug!(
            uri,
            "an object id on this instance's own origin that is not stored resolves to nothing"
        );
        return Ok(None);
    }
    let document = match fetch_contained(state, uri, budget).await {
        Ok(document) => document,
        // Gone and missing answer exactly like a tombstone: the caller has
        // nothing to fold and nothing to retry.
        Err(FetchError::Gone | FetchError::NotFound) => return Ok(None),
        Err(err) => return Err(federation_error(err)),
    };
    if !is_note_family(&document) {
        debug!(uri, "a fetched object that is not a Note-family document");
        return Ok(None);
    }
    let Some(attributed) = attributed_to(&document) else {
        debug!(uri, "a fetched object whose attributedTo names nothing");
        return Ok(None);
    };
    // The as_actor arm refuses before any further fetch: a document the
    // delivering actor did not author is nobody's business but theirs.
    let mirror;
    let author: &User = if let Some(actor) = as_actor {
        if attributed != actor.ap_id {
            debug!(
                uri,
                attributed,
                actor = %actor.ap_id,
                "a fetched object the delivering actor did not author"
            );
            return Ok(None);
        }
        actor
    } else {
        mirror = get_or_fetch_actor(state, &attributed).await?;
        &mirror
    };
    let outcome = ingest::ingest_note(state, author, &document).await?;
    Ok(match outcome {
        IngestOutcome::Created(row) | IngestOutcome::Existing(row) => Some(row),
        IngestOutcome::Ignored(reason) => {
            debug!(
                uri,
                reason, "a fetched object the ingest gate will not fold"
            );
            None
        }
    })
}

/// The stored row's answer to the resolver: a live row as stored, a
/// tombstone as nothing. A deleted row is *not* refetched: its `uri`
/// stays reserved precisely so the tombstone stays the verdict.
fn live_or_deleted(row: Status) -> Option<Status> {
    row.deleted_at.is_none().then_some(row)
}

/// `{base_url}/objects/{uuid}` for a `uri` naming this instance's
/// human-facing `{base_url}/notice/{uuid}` of that id, `None` for every
/// other text. A remote `inReplyTo` may carry the url rather than the
/// object id; both forms name the same local row.
fn local_object_uri(config: &Config, uri: &str) -> Option<String> {
    let rest = uri
        .strip_prefix(&config.base_url)?
        .strip_prefix(NOTICE_PATH)?;
    if Uuid::parse_str(rest).is_err() {
        return None;
    }
    Some(format!("{}{OBJECTS_PATH}{rest}", config.base_url))
}

/// Dereferences `url` under `budget` and returns the contained document
/// (see the [module docs](self)): the `id` must name the fetched URL, or
/// a URL on the same origin that is dereferenced once more and names
/// itself.
async fn fetch_contained(
    state: &AppState,
    url: &str,
    budget: &FetchBudget,
) -> Result<Value, FetchError> {
    let http = object_client(&state.config);
    let signer = fetch_signer(state).await?;
    let document = fetch_bounded(&http, url, signer.as_ref(), budget).await?;
    let claimed = declared_id(&document)?;
    if claimed == url {
        return Ok(document);
    }
    if !same_authority(url, claimed) {
        return Err(FetchError::Invalid(format!(
            "the object fetched from `{url}` names `{claimed}`, a different origin"
        )));
    }
    // Same origin but a different id (the peer redirected, or the id the
    // URL serves differs from the document's own): follow the claim
    // exactly once and accept only a document that names itself.
    let refetched = fetch_bounded(&http, claimed, signer.as_ref(), budget).await?;
    let refetched_id = declared_id(&refetched)?;
    if refetched_id != claimed {
        return Err(FetchError::Invalid(format!(
            "the object `{claimed}` identifies itself as `{refetched_id}`"
        )));
    }
    Ok(refetched)
}

/// One budgeted document `GET`: spends a dereference and runs the shared
/// signed federation fetch against the object body cap.
async fn fetch_bounded(
    http: &HttpClient,
    url: &str,
    signer: Option<&Signer>,
    budget: &FetchBudget,
) -> Result<Value, FetchError> {
    if !budget.take() {
        return Err(FetchError::Invalid(format!(
            "the per-activity object fetch budget is spent (`{url}` not fetched)"
        )));
    }
    fetch_document(http, url, signer, MAX_OBJECT_BODY_BYTES)
        .await
        .map_err(FetchError::from)
}

/// The client the object fetches run on: the instance's outbound identity
/// and SSRF posture exactly as the application's shared client takes from
/// [`Config`], with the tighter redirect budget the module docs state.
fn object_client(config: &Config) -> HttpClient {
    HttpClient::with_redirect_limit(
        &user_agent(&config.base_url, config.contact_email.as_deref()),
        config.fetch.http_allow_private_hosts,
        MAX_OBJECT_REDIRECTS,
    )
}

/// The document's self-declared `id`: every containment rule compares it
/// against the URL the document was fetched from.
fn declared_id(document: &Value) -> Result<&str, FetchError> {
    document
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| FetchError::Invalid("the object document carries no string `id`".to_owned()))
}

/// Whether two absolute URLs name the same origin: the scheme-less
/// authority (`host:port`) compared on the parsed URL, the same measure
/// [`is_local_ap_id`] keeps. An unparsable side matches nothing.
fn same_authority(left: &str, right: &str) -> bool {
    let Ok(parsed_left) = Url::parse(left) else {
        return false;
    };
    Url::parse(right).is_ok_and(|parsed_right| parsed_left.authority() == parsed_right.authority())
}

/// Whether a fetched document is of the `Note` family the status handlers
/// fold — the object types [`crate::federation::note_parser`] accepts and
/// nothing else.
fn is_note_family(document: &Value) -> bool {
    document
        .get("type")
        .and_then(json::first_type)
        .is_some_and(|kind| OBJECT_TYPES.contains(&kind.as_str()))
}

/// The first `attributedTo` reference of a document (the exact shape
/// [`crate::federation::handlers::create`] compares for embedded
/// documents): the author the resolver folds the document as — or refuses
/// to fold it for.
fn attributed_to(document: &Value) -> Option<String> {
    document
        .get("attributedTo")
        .map(json::as_array)
        .and_then(|entries| entries.iter().find_map(json::value_or_id))
}

/// The author mirror of an instance-initiated resolve. Any actor-fetch
/// failure reads as *not now*: the mirror may exist and the document
/// re-serve later, and the job's own attempt budget bounds the retries —
/// distinguishing a peer's permanent refusal from its outage from here
/// would mean parsing error text.
async fn get_or_fetch_actor(state: &AppState, ap_id: &str) -> Result<User, FederationError> {
    get_or_fetch_by_ap_id(state, ap_id)
        .await
        .map_err(|err| FederationError::Transient(err.to_string()))
}

/// The [`HandlerFn`](crate::jobs::HandlerFn) `crate::serve` binds to
/// [`KIND_BACKFILL`].
pub fn handle(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { handle_backfill(state, args).await })
}

/// The `federator.backfill` handler: closes one deferred reply link —
/// resolve the parent the status answers, link the child to it, and queue
/// the parent's own unresolved parent at `depth + 1`.
///
/// `args` is the object [`crate::services::ingest`] or this handler
/// enqueued: `{"status_id": "<uuid>", "depth": <n>}`; a missing or broken
/// `depth` reads as the first walk.
///
/// # Errors
///
/// [`JobError::Discard`] for args no enqueue should have built or for a
/// parent the peer refuses as unparseable; [`JobError::Retry`] for a
/// database or network failure that may pass on a later attempt. A parent
/// that is gone (`Ok(None)` from [`resolve_status`]) or a walk that reaches
/// `THREAD_FETCH_DEPTH` is a quiet completion.
pub async fn handle_backfill(state: AppState, args: Value) -> Result<(), JobError> {
    let Some(raw_id) = args.get("status_id").and_then(Value::as_str) else {
        return Err(JobError::Discard(
            "unusable backfill args: no status id".to_owned(),
        ));
    };
    let Ok(status_id) = Uuid::parse_str(raw_id) else {
        return Err(JobError::Discard(format!(
            "unusable backfill args: `{raw_id}` is not a status id"
        )));
    };
    let depth = args
        .get("depth")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    backfill_one(&state, status_id, depth).await
}

/// One backfill walk: the parent the child answers (`None` — complete —
/// when the child is gone, the link is already closed by a previous pass
/// whose parent vanished, or the deferred parent never resolves to a
/// foldable status), then the walk continues up the chain.
async fn backfill_one(state: &AppState, child_id: Uuid, depth: i64) -> Result<(), JobError> {
    let child = status::get_by_id(&state.db, child_id)
        .await
        .map_err(|err| retry("the status read", &err))?;
    let Some(child) = child else {
        debug!(%child_id, "the status a backfill was queued for is gone");
        return Ok(());
    };
    let parent = match child.in_reply_to_id {
        Some(parent_id) => status::get_by_id(&state.db, parent_id)
            .await
            .map_err(|err| retry("the parent read", &err))?,
        None => resolve_deferred_parent(state, &child, depth).await?,
    };
    match parent {
        Some(parent) => continue_chain(state, &parent, depth).await,
        None => Ok(()),
    }
}

/// The deferred arm of [`backfill_one`]: a child whose parent link is an
/// id only. Past [`FetchLimits::thread_fetch_depth`] ancestors the walk stops
/// quietly; otherwise [`resolve_status`] answers the parent and
/// [`link_backfilled_reply`] closes the link and steps the parent's
/// `replies_count` — idempotent, so a retry of a half-finished pass links
/// nothing twice.
///
/// [`link_backfilled_reply`]: crate::models::status::link_backfilled_reply
/// [`FetchLimits::thread_fetch_depth`]: crate::config::FetchLimits::thread_fetch_depth
async fn resolve_deferred_parent(
    state: &AppState,
    child: &Status,
    depth: i64,
) -> Result<Option<Status>, JobError> {
    let limit = i64::try_from(state.config.fetch.limits.thread_fetch_depth).unwrap_or(i64::MAX);
    if depth >= limit {
        debug!(
            status = %child.id,
            depth, limit, "the thread backfill stops at its depth limit"
        );
        return Ok(None);
    }
    let Some(parent_uri) = child.in_reply_to_ap_id.as_deref() else {
        return Ok(None);
    };
    let parent = match resolve_status(state, parent_uri).await {
        Ok(Some(parent)) => parent,
        Ok(None) => {
            debug!(
                status = %child.id,
                parent = %parent_uri,
                "the answered object is gone or unfoldable; the thread stays as stored"
            );
            return Ok(None);
        }
        Err(FederationError::Transient(detail)) => return Err(JobError::Retry(detail)),
        Err(FederationError::Permanent(detail)) => return Err(JobError::Discard(detail)),
    };
    let linked = status::link_backfilled_reply(&state.db, child.id, &parent)
        .await
        .map_err(|err| retry("the reply link write", &err))?;
    if linked {
        debug!(status = %child.id, parent = %parent.id, "a backfilled thread link closed");
    }
    Ok(Some(parent))
}

/// The chain arm: when the parent *itself* answers an unstored object, its
/// own backfill runs at `depth + 1`. Ingest queued that job at depth `0`
/// the moment the parent's row was created (see
/// [`crate::services::ingest`]) — this walk's depth bookkeeping wins, so
/// the pending pass is cancelled and re-queued one deeper, which is what
/// makes the limit bind on a chain fetched end to end. One step from the
/// limit, the cancelled pass is not re-queued and the walk ends.
async fn continue_chain(state: &AppState, parent: &Status, depth: i64) -> Result<(), JobError> {
    if parent.in_reply_to_ap_id.is_none() {
        return Ok(());
    }
    let unique_key = backfill_key(parent.id);
    // Cancel parent's own queued pass unconditionally when it carries a
    // deferred link: this walk owns the depth bookkeeping from here, so at
    // the limit the pending pass is dropped rather than left to restart the
    // walk uncounted.
    if let Err(err) = job::cancel_pending(&state.db, &unique_key).await {
        debug!(%err, parent = %parent.id, "the pending backfill cancel failed; queueing anyway");
    }
    let limit = i64::try_from(state.config.fetch.limits.thread_fetch_depth).unwrap_or(i64::MAX);
    if depth + 1 >= limit {
        debug!(
            parent = %parent.id,
            depth, limit, "the thread backfill stops at its depth limit"
        );
        return Ok(());
    }
    let args = json!({ "status_id": parent.id.to_string(), "depth": depth + 1 });
    let new_job = NewJob {
        queue: QUEUE_BACKGROUND,
        kind: KIND_BACKFILL,
        args,
        run_at: None,
        max_attempts: Some(BACKFILL_MAX_ATTEMPTS),
        unique_key: Some(&unique_key),
    };
    job::enqueue(&state.db, new_job)
        .await
        .map(drop)
        .map_err(|err| {
            // The child's link is already committed; a lost deeper job costs
            // the chain one pass, and this attempt reports it for retry — the
            // link write reruns as the no-op it is.
            retry("the backfill enqueue", &err)
        })
}

/// The dedupe key of the backfill job for the status `status_id`: one
/// live job per deferred reply, shared by the ingest enqueue and the
/// chain walk.
pub(crate) fn backfill_key(status_id: Uuid) -> String {
    format!("{BACKFILL_KEY_PREFIX}{status_id}")
}

/// The resolver's error vocabulary for an object-fetch failure: a broken
/// transport is a *not now* the queue can ride out; a refusal of the
/// document — malformed, unconstrained by containment, or past the run's
/// fetch budget — stands on its own, and no retry changes it.
fn federation_error(err: FetchError) -> FederationError {
    match err {
        FetchError::Http(http) => {
            FederationError::Transient(format!("object fetch failed: {http}"))
        }
        FetchError::Gone | FetchError::NotFound => FederationError::Permanent(err.to_string()),
        FetchError::Invalid(detail) => FederationError::Permanent(detail),
    }
}

/// Shapes a database failure into the retryable arm of the dispatcher's
/// error vocabulary, naming the read or write that failed.
fn transient(what: &str, err: &impl fmt::Display) -> FederationError {
    FederationError::Transient(format!("{what} failed: {err}"))
}

/// Shapes a database failure into the job's retry arm, naming the read or
/// write that failed.
fn retry(what: &str, err: &impl fmt::Display) -> JobError {
    JobError::Retry(format!("{what} failed: {err}"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::{
        Router,
        extract::{Path, State},
        http::{HeaderMap, StatusCode, header},
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use sqlx::PgPool;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        federation::fetch_actor::ACTOR_ACCEPT,
        http_client::HttpClient,
        models::{
            status::NewStatus,
            user::{self, Flag, NewUser},
        },
        uploads::UploaderBackend,
    };

    /// `User-Agent` for the test clients.
    const TEST_UA: &str = "Wired (https://test.example)";
    /// The content type a federation peer answers an object document with.
    const OBJECT_CONTENT_TYPE: &str = "application/activity+json";
    /// The public addressing every fixture note carries.
    const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
    /// The actor every fixture note is authored by, under the peer's base.
    const ALICE: &str = "alice";

    /// The state under test: `pool` on the default test config — whose
    /// `HTTP_ALLOW_PRIVATE_HOSTS` lets the fetch client reach the mocks.
    fn test_state(pool: PgPool) -> AppState {
        state_with_config(pool, Config::for_tests())
    }

    /// The state under test on `config` instead of the stock test one.
    fn state_with_config(pool: PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        let hub = crate::streaming::hub::Hub::new(&pool);
        AppState {
            db: pool,
            hub,
            config: Arc::clone(&config),
            http_client: HttpClient::new_for_tests(TEST_UA),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// Binds an unused loopback port.
    async fn bind() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").await.unwrap()
    }

    /// The `http://{addr}` origin a listener answers on.
    fn origin(listener: &TcpListener) -> String {
        format!("http://{}", listener.local_addr().unwrap())
    }

    /// Serves `app` on `listener` until the test aborts the task.
    fn spawn(listener: TcpListener, app: Router) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test server stopped: {err}");
            }
        })
    }

    /// The state every mock-peer handler runs with: its own origin, the
    /// request count, the last `Accept` seen, and the fixture's thread
    /// shape (object name → answered object name).
    #[derive(Clone)]
    struct Peer {
        /// The `http://{addr}` origin of this mock server.
        base: String,
        /// Requests this peer answered, across all its routes.
        hits: Arc<AtomicUsize>,
        /// The `Accept` header of the most recent request, when recorded.
        last_accept: Arc<Mutex<Option<String>>>,
        /// Which fixture objects answer which other fixture object.
        parents: Arc<Mutex<HashMap<String, String>>>,
    }

    /// A mock peer at `base`: nothing fetched yet, no thread shape.
    fn peer_at(base: &str) -> Peer {
        Peer {
            base: base.to_owned(),
            hits: Arc::new(AtomicUsize::new(0)),
            last_accept: Arc::new(Mutex::new(None)),
            parents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A minimal actor document for `id`, the way a `Mastodon` peer serves
    /// one: everything [`parse_actor`] requires is present.
    ///
    /// [`parse_actor`]: crate::federation::remote_actor::parse_actor
    fn actor_document(id: &str) -> Value {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Person",
            "id": id,
            "preferredUsername": ALICE,
            "name": "First",
            "inbox": format!("{id}/inbox"),
            "publicKey": {
                "id": format!("{id}#main-key"),
                "owner": id,
                "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
            },
        })
    }

    /// The fixture note named `name` on `peer`: authored by its `alice`,
    /// public, and answering whatever the peer's thread shape says it does.
    fn note_document(peer: &Peer, name: &str) -> Value {
        let parent = peer.parents.lock().unwrap().get(name).cloned();
        let mut document = json!({
            "id": format!("{}/objects/{name}", peer.base),
            "type": "Note",
            "attributedTo": format!("{}/users/{ALICE}", peer.base),
            "to": [PUBLIC],
            "content": format!("<p>object {name}</p>"),
            "published": "2024-03-01T10:00:00.000Z",
        });
        if let Some(answered) = parent {
            document["inReplyTo"] = json!(format!("{}/objects/{answered}", peer.base));
        }
        document
    }

    /// A body labelled [`OBJECT_CONTENT_TYPE`].
    fn json_response(document: &Value) -> Response {
        (
            [(header::CONTENT_TYPE, OBJECT_CONTENT_TYPE)],
            document.to_string(),
        )
            .into_response()
    }

    /// The peer's standard surface: alice's actor document and any fixture
    /// note by name.
    fn peer_app(peer: Peer) -> Router {
        Router::new()
            .route("/users/{name}", get(serve_actor))
            .route("/objects/{name}", get(serve_object))
            .route("/hop/{n}", get(serve_hop))
            .with_state(peer)
    }

    /// Counts the hit and answers alice's self-identifying actor document.
    async fn serve_actor(State(peer): State<Peer>) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        json_response(&actor_document(&format!("{}/users/{ALICE}", peer.base)))
    }

    /// Counts the hit, records the `Accept`, and answers the fixture note
    /// named by the path — which carries the peer's thread shape.
    async fn serve_object(
        State(peer): State<Peer>,
        Path(name): Path<String>,
        headers: HeaderMap,
    ) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        if let Some(accept) = headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
        {
            let _previous = peer.last_accept.lock().unwrap().replace(accept.to_owned());
        }
        json_response(&note_document(&peer, &name))
    }

    /// Counts the hit; `/hop/0` lands on the fixture note `landed` and
    /// every other hop redirects one closer to it.
    async fn serve_hop(State(peer): State<Peer>, Path(n): Path<u32>) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return json_response(&note_document(&peer, "landed"));
        }
        (
            StatusCode::FOUND,
            [(header::LOCATION, format!("{}/hop/{}", peer.base, n - 1))],
        )
            .into_response()
    }

    /// A one-route server for one fixed document at one exact path.
    fn fixed_app(path: &str, document: Value) -> Router {
        let exact = path.to_owned();
        Router::new()
            .route(&exact, get(serve_fixed))
            .with_state(document)
    }

    /// [`fixed_app`]'s handler.
    async fn serve_fixed(State(document): State<Value>) -> Response {
        json_response(&document)
    }

    /// One fixed document plus the peer counter its deliveries run up.
    #[derive(Clone)]
    struct ServedDoc {
        /// The peer's request counter.
        hits: Arc<AtomicUsize>,
        /// The document the route answers with.
        document: Value,
    }

    /// A [`fixed_app`] whose deliveries the peer counts.
    fn counted_fixed_app(peer: &Peer, path: &str, document: Value) -> Router {
        let exact = path.to_owned();
        Router::new()
            .route(&exact, get(serve_counted_fixed))
            .with_state(ServedDoc {
                hits: Arc::clone(&peer.hits),
                document,
            })
    }

    /// [`counted_fixed_app`]'s handler.
    async fn serve_counted_fixed(State(served): State<ServedDoc>) -> Response {
        let _hits = served.hits.fetch_add(1, Ordering::SeqCst);
        json_response(&served.document)
    }

    /// The object url of fixture `name` on a peer answering at `base`.
    fn object_url(base: &str, name: &str) -> String {
        format!("{base}/objects/{name}")
    }

    /// Counts the `statuses` rows stored under `uri`.
    async fn rows_for_uri(pool: &PgPool, uri: &str) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
            .bind(uri)
            .fetch_one(pool)
            .await
            .expect("the row count reads")
    }

    /// The live `federator.backfill` job queued for `status_id`, if any:
    /// its args and its attempt budget.
    async fn pending_backfill(pool: &PgPool, status_id: Uuid) -> Option<(Value, i32)> {
        sqlx::query_as::<_, (Value, i32)>(
            "SELECT args, max_attempts FROM jobs
              WHERE kind = $1 AND unique_key = $2 AND state IN ('available', 'retryable')",
        )
        .bind(KIND_BACKFILL)
        .bind(backfill_key(status_id))
        .fetch_optional(pool)
        .await
        .expect("the job read")
    }

    /// The stored mirror of `{name}@peer`, inserted straight: the authors
    /// the direct-ingest fixtures are written under.
    async fn mirror_user(pool: &PgPool, name: &str, base: &str) -> User {
        let host = base.strip_prefix("http://").unwrap_or(base).to_owned();
        user::insert(
            pool,
            &NewUser {
                nickname: format!("{name}@{host}"),
                ap_id: format!("{base}/users/{name}"),
                local: Flag::new(false),
                domain: Some(host),
                followers_url: Some(format!("{base}/users/{name}/followers")),
                ..NewUser::default()
            },
        )
        .await
        .expect("the mirror fixture inserts")
    }

    /// The stored mirror of `alice@peer`.
    async fn alice_mirror(pool: &PgPool, base: &str) -> User {
        mirror_user(pool, ALICE, base).await
    }

    /// Answers `body` labelled the way an HTML page is, to test the
    /// content-type rule.
    fn html_response(body: &'static str) -> Response {
        ([(header::CONTENT_TYPE, "text/html")], body).into_response()
    }

    /// A peer answering `/gone` with 410, `/missing` with 404 and `/flaky`
    /// with 500, plus `/html` (wrong media type) and `/broken` (claimed
    /// JSON that is not).
    fn refusal_app(peer: &Peer) -> Router {
        Router::new()
            .route("/gone", get(serve_gone).with_state(Arc::clone(&peer.hits)))
            .route(
                "/missing",
                get(serve_missing).with_state(Arc::clone(&peer.hits)),
            )
            .route(
                "/flaky",
                get(serve_flaky).with_state(Arc::clone(&peer.hits)),
            )
            .route("/html", get(serve_html).with_state(Arc::clone(&peer.hits)))
            .route(
                "/broken",
                get(serve_broken).with_state(Arc::clone(&peer.hits)),
            )
    }

    /// [`refusal_app`]'s status arms: each counts its hit.
    async fn serve_gone(State(hits): State<Arc<AtomicUsize>>) -> StatusCode {
        let _hits = hits.fetch_add(1, Ordering::SeqCst);
        StatusCode::GONE
    }

    /// [`refusal_app`]'s 404 arm.
    async fn serve_missing(State(hits): State<Arc<AtomicUsize>>) -> StatusCode {
        let _hits = hits.fetch_add(1, Ordering::SeqCst);
        StatusCode::NOT_FOUND
    }

    /// [`refusal_app`]'s 500 arm.
    async fn serve_flaky(State(hits): State<Arc<AtomicUsize>>) -> StatusCode {
        let _hits = hits.fetch_add(1, Ordering::SeqCst);
        StatusCode::INTERNAL_SERVER_ERROR
    }

    /// [`refusal_app`]'s HTML arm.
    async fn serve_html(State(hits): State<Arc<AtomicUsize>>) -> Response {
        let _hits = hits.fetch_add(1, Ordering::SeqCst);
        html_response("<html>not activity</html>")
    }

    /// [`refusal_app`]'s unparsable-body arm.
    async fn serve_broken(State(hits): State<Arc<AtomicUsize>>) -> Response {
        let _hits = hits.fetch_add(1, Ordering::SeqCst);
        (
            [(header::CONTENT_TYPE, OBJECT_CONTENT_TYPE)],
            "this is not json",
        )
            .into_response()
    }

    /// A peer whose `/objects/moved` serves the fixture note `real`: the
    /// moved-document arm of containment, where the refetch at the declared
    /// id succeeds because that document names itself.
    fn moved_app(peer: &Peer) -> Router {
        Router::new()
            .route("/objects/moved", get(serve_moved).with_state(peer.clone()))
            .merge(peer_app(peer.clone()))
    }

    /// [`moved_app`]'s handler: `real`'s document, served under `moved`.
    async fn serve_moved(State(peer): State<Peer>) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        json_response(&note_document(&peer, "real"))
    }

    #[tokio::test]
    async fn a_self_identifying_object_is_returned_verbatim() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state(db.pool().clone());
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let server = spawn(listener, peer_app(peer.clone()));

        let document = fetch_object_json(&state, &object_url(&base, "one"))
            .await
            .expect("the fixture note dereferences");

        assert_eq!(
            document["id"],
            json!(object_url(&base, "one")),
            "the document names the URL it was fetched from"
        );
        assert_eq!(document["type"], json!("Note"), "the body is the fixture");
        assert_eq!(
            peer.hits.load(Ordering::SeqCst),
            1,
            "a self-naming object costs exactly one GET"
        );
        assert_eq!(
            peer.last_accept.lock().unwrap().as_deref(),
            Some(ACTOR_ACCEPT),
            "the fetch asks with both ActivityPub JSON media types"
        );
        server.abort();
    }

    #[tokio::test]
    async fn gone_missing_and_flaky_answer_their_own_errors() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state(db.pool().clone());
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let server = spawn(listener, refusal_app(&peer));

        let gone = fetch_object_json(&state, &format!("{base}/gone"))
            .await
            .expect_err("410 is not a document");
        assert!(matches!(gone, FetchError::Gone), "410 maps to Gone: {gone}");
        let missing = fetch_object_json(&state, &format!("{base}/missing"))
            .await
            .expect_err("404 is not a document");
        assert!(
            matches!(missing, FetchError::NotFound),
            "404 maps to NotFound: {missing}"
        );
        let flaky = fetch_object_json(&state, &format!("{base}/flaky"))
            .await
            .expect_err("500 is not a document");
        assert!(
            matches!(flaky, FetchError::Http(HttpError::Status(500))),
            "an unhandled status maps to Http: {flaky}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn unusable_bodies_are_invalid() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state(db.pool().clone());
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let app = refusal_app(&peer).merge(fixed_app("/noid", json!({"type": "Note"})));
        let server = spawn(listener, app);

        let html = fetch_object_json(&state, &format!("{base}/html"))
            .await
            .expect_err("an HTML body is not a document");
        assert!(
            matches!(&html, FetchError::Invalid(detail) if detail.contains("content type")),
            "a wrong media type is Invalid: {html}"
        );
        let broken = fetch_object_json(&state, &format!("{base}/broken"))
            .await
            .expect_err("unparsable JSON is not a document");
        assert!(
            matches!(&broken, FetchError::Invalid(detail) if detail.contains("parseable")),
            "a broken body is Invalid: {broken}"
        );
        let noid = fetch_object_json(&state, &format!("{base}/noid"))
            .await
            .expect_err("a document without an id cannot be contained");
        assert!(
            matches!(&noid, FetchError::Invalid(detail) if detail.contains("id")),
            "a document with no string id is Invalid: {noid}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_body_past_the_two_megabyte_cap_is_refused() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state(db.pool().clone());
        let listener = bind().await;
        let base = origin(&listener);
        let url = object_url(&base, "huge");
        let document = json!({
            "id": url,
            "type": "Note",
            "content": "x".repeat(MAX_OBJECT_BODY_BYTES + 4096),
        });
        let server = spawn(listener, fixed_app("/objects/huge", document));

        let err = fetch_object_json(&state, &url)
            .await
            .expect_err("the cap must cut the body off");

        assert!(
            matches!(err, FetchError::Http(HttpError::TooLarge)),
            "over the cap is an Http(TooLarge) failure: {err}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_moved_object_is_reanchored_at_its_own_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state(db.pool().clone());
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let server = spawn(listener, moved_app(&peer));

        let document = fetch_object_json(&state, &object_url(&base, "moved"))
            .await
            .expect("a same-origin move is followed once");

        assert_eq!(
            document["id"],
            json!(object_url(&base, "real")),
            "the accepted document names the URL its own id was fetched from"
        );
        assert_eq!(
            peer.hits.load(Ordering::SeqCst),
            2,
            "the claim costs exactly one refetch"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_document_that_keeps_moving_is_invalid() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state(db.pool().clone());
        let listener = bind().await;
        let base = origin(&listener);
        let app = fixed_app("/a", json!({"id": format!("{base}/b"), "type": "Note"})).merge(
            fixed_app("/b", json!({"id": format!("{base}/a"), "type": "Note"})),
        );
        let server = spawn(listener, app);

        let err = fetch_object_json(&state, &format!("{base}/a"))
            .await
            .expect_err("a document that moves again is not accepted");

        assert!(
            matches!(&err, FetchError::Invalid(detail) if detail.contains("identifies itself")),
            "the second mismatch names the moving document: {err}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_cross_origin_id_is_refused_without_a_refetch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state(db.pool().clone());
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let foreign = json!({
            "id": "http://foreign.invalid/objects/stolen",
            "type": "Note",
        });
        let app = refusal_app(&peer).merge(counted_fixed_app(&peer, "/foreign", foreign));
        let server = spawn(listener, app);

        let err = fetch_object_json(&state, &format!("{base}/foreign"))
            .await
            .expect_err("origin containment refuses a document naming another host");

        assert_eq!(
            peer.hits.load(Ordering::SeqCst),
            1,
            "exactly the first GET ran; the foreign id was never fetched"
        );
        assert!(
            matches!(&err, FetchError::Invalid(detail) if detail.contains("different origin")),
            "a foreign id is Invalid on the spot: {err}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn redirect_chains_are_bounded_by_three_hops() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state(db.pool().clone());
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let server = spawn(listener, peer_app(peer.clone()));

        let document = fetch_object_json(&state, &format!("{base}/hop/2"))
            .await
            .expect("a two-hop chain is followed");
        assert_eq!(
            document["id"],
            json!(object_url(&base, "landed")),
            "the chain lands on the self-naming note"
        );

        let err = fetch_object_json(&state, &format!("{base}/hop/9"))
            .await
            .expect_err("a chain longer than the cap never resolves");
        assert!(
            matches!(err, FetchError::Http(_)),
            "past the redirect cap the client's failure rides on Http: {err}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn the_fetch_budget_stops_a_fanning_run() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let mut config = Config::for_tests();
        config.fetch.limits.max_fetches_per_activity = 1;
        let state = state_with_config(db.pool().clone(), config);
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let server = spawn(listener, moved_app(&peer));

        let err = fetch_object_json(&state, &object_url(&base, "moved"))
            .await
            .expect_err("the refetch the moved document wants costs a second dereference");

        assert!(
            matches!(&err, FetchError::Invalid(detail) if detail.contains("budget")),
            "a run past its budget is refused: {err}"
        );
        assert_eq!(
            peer.hits.load(Ordering::SeqCst),
            1,
            "the budget stopped the run before the second GET left"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_stored_uri_answers_from_the_database_without_a_fetch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool.clone());
        // No mock runs at all: `remote.invalid` cannot answer, so any
        // network attempt here would fail the resolve instead of proving
        // the database answered first.
        let alice = alice_mirror(&pool, "https://remote.invalid").await;
        let uri = object_url("https://remote.invalid", "quiet");
        let mut document = json!({
            "id": uri,
            "type": "Note",
            "attributedTo": alice.ap_id,
            "to": [PUBLIC],
            "content": "<p>quiet</p>",
        });
        document["published"] = json!("2024-03-01T10:00:00.000Z");
        let IngestOutcome::Created(row) = ingest::ingest_note(&state, &alice, &document)
            .await
            .expect("the fixture note ingests")
        else {
            panic!("a first-seen note is created");
        };

        let resolved = resolve_status(&state, &uri)
            .await
            .expect("the stored row resolves without the network");

        assert_eq!(
            resolved.map(|stored| stored.id),
            Some(row.id),
            "the mirror of the stored object answers as stored"
        );
    }

    #[tokio::test]
    async fn a_notice_url_resolves_to_the_local_object_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool.clone());
        let carol = user::insert(
            &pool,
            &NewUser {
                nickname: "carol".to_owned(),
                ap_id: format!("{TEST_BASE_URL}/users/carol"),
                local: Flag::new(true),
                ..NewUser::default()
            },
        )
        .await
        .expect("the local author inserts");
        let row = status::insert(
            &pool,
            TEST_BASE_URL,
            &NewStatus::local(&carol, "a local root"),
        )
        .await
        .expect("the local status inserts");
        let notice = row
            .url
            .clone()
            .expect("a local status carries its notice url");

        let resolved = resolve_status(&state, &notice)
            .await
            .expect("the alias resolves");

        assert_eq!(
            resolved.map(|stored| stored.id),
            Some(row.id),
            "the `/notice/…` form matches the row stored under `/objects/…`"
        );
    }

    #[tokio::test]
    async fn a_local_id_this_database_lacks_resolves_to_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool.clone());
        let missing = format!("{TEST_BASE_URL}/objects/{}", Uuid::now_v7());

        // No mock: an unknown id on this instance's own origin is answered
        // without the network, so a fetch attempt would surface as an error.
        let resolved = resolve_status(&state, &missing)
            .await
            .expect("the local miss is not a failure");

        assert_eq!(
            resolved, None,
            "this instance serves its own ids from its database"
        );
    }

    #[tokio::test]
    async fn a_tombstoned_row_resolves_to_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool.clone());
        let alice = alice_mirror(&pool, "https://remote.invalid").await;
        let uri = object_url("https://remote.invalid", "deleted");
        let document = json!({
            "id": uri,
            "type": "Note",
            "attributedTo": alice.ap_id,
            "to": [PUBLIC],
            "content": "<p>short lived</p>",
        });
        let IngestOutcome::Created(row) = ingest::ingest_note(&state, &alice, &document)
            .await
            .expect("the fixture ingests")
        else {
            panic!("a first-seen note is created");
        };
        let _tombstone = status::soft_delete(&pool, row.id)
            .await
            .expect("the delete tombstones");

        let resolved = resolve_status(&state, &uri)
            .await
            .expect("the tombstone answers");

        assert_eq!(
            resolved, None,
            "a deleted post resolves to nothing and is never refetched"
        );
    }

    #[tokio::test]
    async fn a_fetched_note_is_ingested_exactly_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool.clone());
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let server = spawn(listener, peer_app(peer.clone()));
        let uri = object_url(&base, "one");

        let first = resolve_status(&state, &uri)
            .await
            .expect("the fixture note resolves");
        let hits_after_first = peer.hits.load(Ordering::SeqCst);
        let second = resolve_status(&state, &uri)
            .await
            .expect("the repeat resolve is not a failure");

        let Some(row) = first else {
            panic!("a fetchable note with a fetchable author becomes a status");
        };
        assert_eq!(row.uri, uri, "the object id is the stored uri");
        assert!(!row.local, "the row is a mirror");
        assert_eq!(
            second.map(|stored| stored.id),
            Some(row.id),
            "the second resolve restates the same row"
        );
        assert_eq!(
            peer.hits.load(Ordering::SeqCst),
            hits_after_first,
            "the second resolve touched nothing: the row answers from the database"
        );
        assert_eq!(
            rows_for_uri(&pool, &uri).await,
            1,
            "one status for the two resolves"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_document_outside_the_note_family_resolves_to_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool.clone());
        let listener = bind().await;
        let base = origin(&listener);
        let url = format!("{base}/place");
        let document = json!({
            "id": url,
            "type": "Place",
            "attributedTo": format!("{base}/users/{ALICE}"),
            "content": "<p>a location, not a post</p>",
        });
        let server = spawn(listener, fixed_app("/place", document));

        let resolved = resolve_status(&state, &url)
            .await
            .expect("a non-note is an answer, not a failure");

        assert_eq!(resolved, None, "only the Note family becomes a status");
        server.abort();
    }

    #[tokio::test]
    async fn a_gone_or_missing_object_resolves_to_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool.clone());
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let server = spawn(listener, refusal_app(&peer));

        let gone = resolve_status(&state, &format!("{base}/gone"))
            .await
            .expect("a 410 completes the resolve");
        let missing = resolve_status(&state, &format!("{base}/missing"))
            .await
            .expect("a 404 completes the resolve");
        let flaky = resolve_status(&state, &format!("{base}/flaky")).await;

        assert_eq!(gone, None, "gone is nothing, retryable or not");
        assert_eq!(missing, None, "missing is nothing");
        assert!(
            matches!(flaky, Err(FederationError::Transient(_))),
            "a 500 says not now: {flaky:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_foreign_document_is_not_folded_for_the_delivering_actor() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool.clone());
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let server = spawn(listener, peer_app(peer.clone()));
        // `mallory` delivers a `Create` naming alice's note: the fetched
        // document names its own author, not the deliverer.
        let mallory = mirror_user(&pool, "mallory", &base).await;

        let resolved = resolve_status_as_actor(&state, &object_url(&base, "one"), &mallory)
            .await
            .expect("the refusal is an answer, not a failure");

        assert_eq!(
            resolved, None,
            "a peer may only create its own posts by reference"
        );
        assert_eq!(
            peer.hits.load(Ordering::SeqCst),
            1,
            "the refusal lands after the object GET and before any author fetch"
        );
        assert_eq!(
            rows_for_uri(&pool, &object_url(&base, "one")).await,
            0,
            "nothing is stored under the foreign author's object"
        );
        server.abort();
    }

    /// The args of a `federator.backfill` job for `status_id` at `depth`.
    fn backfill_args(status_id: Uuid, depth: i64) -> Value {
        json!({ "status_id": status_id.to_string(), "depth": depth })
    }

    /// A peer whose thread shape is `head → … → tail`: each named fixture
    /// note answers the next one, and the last roots its own thread.
    fn chain_peer(base: &str, names: &[&str]) -> Peer {
        let peer = peer_at(base);
        {
            let mut parents = peer.parents.lock().unwrap();
            for (name, parent) in names.iter().copied().zip(names.iter().copied().skip(1)) {
                let _inserted = parents.insert(name.to_owned(), parent.to_owned());
            }
        }
        peer
    }

    #[tokio::test]
    async fn the_backfill_job_walks_a_thread_and_queues_the_next_link() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool.clone());
        let listener = bind().await;
        let base = origin(&listener);
        // child → p1 → p2: the walk closes child→p1, storing p1 (itself
        // deferred), and queues p1's own pass one deeper — not restarted.
        let peer = chain_peer(&base, &["child", "p1", "p2"]);
        let alice = alice_mirror(&pool, &base).await;
        let server = spawn(listener, peer_app(peer.clone()));

        let IngestOutcome::Created(child) =
            ingest::ingest_note(&state, &alice, &note_document(&peer, "child"))
                .await
                .expect("the child note ingests")
        else {
            panic!("a first-seen note is created");
        };
        assert_eq!(
            child.in_reply_to_ap_id.as_deref(),
            Some(object_url(&base, "p1").as_str()),
            "the parent is kept as an unresolved id"
        );
        assert_eq!(child.in_reply_to_id, None, "and nothing is linked yet");
        let (args, max_attempts) = pending_backfill(&pool, child.id)
            .await
            .expect("the ingest queued a backfill");
        assert_eq!(
            args["depth"],
            json!(0),
            "the first walk starts at depth zero"
        );
        assert_eq!(
            max_attempts, BACKFILL_MAX_ATTEMPTS,
            "the queued job carries the backfill retry budget"
        );

        handle_backfill(state.clone(), backfill_args(child.id, 0))
            .await
            .expect("the first walk links the child to its parent");

        let stored_parent = status::get_by_uri(&pool, &object_url(&base, "p1"))
            .await
            .expect("the parent read")
            .expect("the parent was fetched and stored");
        let linked = status::get_by_id(&pool, child.id)
            .await
            .expect("the child read")
            .expect("the child is stored");
        assert_eq!(
            linked.in_reply_to_id,
            Some(stored_parent.id),
            "the deferred id became a real link"
        );
        assert_eq!(linked.in_reply_to_ap_id, None, "the deferred id cleared");
        assert_eq!(
            linked.in_reply_to_account_id,
            Some(stored_parent.user_id),
            "the parent's author linked too"
        );
        let parent = status::get_by_id(&pool, stored_parent.id)
            .await
            .expect("the parent read")
            .expect("the parent is stored");
        assert_eq!(parent.replies_count, 1, "the parent gained its first reply");
        assert!(
            parent.in_reply_to_ap_id.is_some(),
            "the parent itself answers an unstored object"
        );
        let (next_args, _) = pending_backfill(&pool, parent.id)
            .await
            .expect("the walk queued the parent's own pass");
        assert_eq!(
            next_args["depth"],
            json!(1),
            "the chain continued one deeper, not restarted at zero"
        );
        server.abort();
    }

    #[tokio::test]
    async fn the_backfill_walk_stops_at_the_depth_limit() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut config = Config::for_tests();
        config.fetch.limits.thread_fetch_depth = 2;
        let state = state_with_config(pool.clone(), config);
        let listener = bind().await;
        let base = origin(&listener);
        // child → p1 → p2 with the limit at two: the walk closes child→p1
        // (the pass at depth 0) and p1→p2 (the pass at depth 1), and p2
        // inherits no further queue — its own pending pass is retired.
        let peer = chain_peer(&base, &["child", "p1", "p2", "p3"]);
        let alice = alice_mirror(&pool, &base).await;
        let server = spawn(listener, peer_app(peer.clone()));
        let IngestOutcome::Created(child) =
            ingest::ingest_note(&state, &alice, &note_document(&peer, "child"))
                .await
                .expect("the child note ingests")
        else {
            panic!("a first-seen note is created");
        };

        handle_backfill(state.clone(), backfill_args(child.id, 0))
            .await
            .expect("the first walk completes");
        let p1 = status::get_by_uri(&pool, &object_url(&base, "p1"))
            .await
            .expect("the parent read")
            .expect("the first parent stored");
        handle_backfill(state.clone(), backfill_args(p1.id, 1))
            .await
            .expect("the second walk completes at the limit");

        let p2 = status::get_by_uri(&pool, &object_url(&base, "p2"))
            .await
            .expect("the grandparent read")
            .expect("the second parent stored within the limit");
        assert_eq!(
            p2.in_reply_to_ap_id.as_deref(),
            Some(object_url(&base, "p3").as_str())
        );
        assert!(
            pending_backfill(&pool, p2.id).await.is_none(),
            "at the limit the walk cancels the queued pass instead of continuing"
        );
        assert_eq!(
            rows_for_uri(&pool, &object_url(&base, "p3")).await,
            0,
            "the ancestor past the limit was never fetched"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_gone_parent_completes_the_walk_in_silence() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool.clone());
        let listener = bind().await;
        let base = origin(&listener);
        let alice = alice_mirror(&pool, &base).await;
        let peer = peer_at(&base);
        let mut child = note_document(&peer, "child");
        child["inReplyTo"] = json!(format!("{base}/gone"));
        let IngestOutcome::Created(row) = ingest::ingest_note(&state, &alice, &child)
            .await
            .expect("the child ingests")
        else {
            panic!("a first-seen note is created");
        };
        let server = spawn(listener, refusal_app(&peer));

        handle_backfill(state, backfill_args(row.id, 0))
            .await
            .expect("a 410 parent is not an error the queue should retry");

        let still_deferred = status::get_by_id(&pool, row.id)
            .await
            .expect("the child read")
            .expect("the child is stored");
        assert_eq!(
            still_deferred.in_reply_to_ap_id.as_deref(),
            Some(format!("{base}/gone").as_str()),
            "the link stays deferred: the parent may legitimately be missing"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_flaky_parent_retries_the_walk() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool.clone());
        let listener = bind().await;
        let base = origin(&listener);
        let alice = alice_mirror(&pool, &base).await;
        let peer = peer_at(&base);
        let mut child = note_document(&peer, "child");
        child["inReplyTo"] = json!(format!("{base}/flaky"));
        let IngestOutcome::Created(row) = ingest::ingest_note(&state, &alice, &child)
            .await
            .expect("the child ingests")
        else {
            panic!("a first-seen note is created");
        };
        let server = spawn(listener, refusal_app(&peer));

        let err = handle_backfill(state, backfill_args(row.id, 0))
            .await
            .expect_err("a 500 parent is retryable");

        assert!(
            matches!(err, JobError::Retry(_)),
            "the queue runs the walk again: {err}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn the_walk_is_idempotent_across_repeats() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool.clone());
        let listener = bind().await;
        let base = origin(&listener);
        let peer = chain_peer(&base, &["child", "p1"]);
        let alice = alice_mirror(&pool, &base).await;
        let server = spawn(listener, peer_app(peer.clone()));
        let IngestOutcome::Created(child) =
            ingest::ingest_note(&state, &alice, &note_document(&peer, "child"))
                .await
                .expect("the child ingests")
        else {
            panic!("a first-seen note is created");
        };
        handle_backfill(state.clone(), backfill_args(child.id, 0))
            .await
            .expect("the first walk links");
        let hits = peer.hits.load(Ordering::SeqCst);

        handle_backfill(state, backfill_args(child.id, 0))
            .await
            .expect("the repeat walk is not a failure");

        let stored_parent = status::get_by_uri(&pool, &object_url(&base, "p1"))
            .await
            .expect("the parent read")
            .expect("the parent stored");
        let parent = status::get_by_id(&pool, stored_parent.id)
            .await
            .expect("the parent read")
            .expect("the parent stored");
        assert_eq!(
            parent.replies_count, 1,
            "the counter stepped once, not twice"
        );
        assert_eq!(
            peer.hits.load(Ordering::SeqCst),
            hits,
            "the linked child resolves from the database, touching no network"
        );
        server.abort();
    }

    #[tokio::test]
    async fn unusable_backfill_args_are_discarded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool);

        let missing = handle_backfill(state.clone(), json!({"depth": 0}))
            .await
            .expect_err("args with no status id are unusable");
        let broken = handle_backfill(state, json!({"status_id": "not-a-uuid", "depth": 0}))
            .await
            .expect_err("a non-uuid id is unusable");

        assert!(
            matches!(missing, JobError::Discard(_)),
            "no id discards: {missing}"
        );
        assert!(
            matches!(broken, JobError::Discard(_)),
            "a broken id discards: {broken}"
        );
    }
}
