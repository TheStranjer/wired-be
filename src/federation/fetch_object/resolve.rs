//! The policy half of the object fetcher: resolve a URI to a stored status,
//! from the local mirror first and a contained remote fetch second.

use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use super::{FetchBudget, FetchError, fetch::fetch_contained};
use crate::{
    config::Config,
    federation::{
        dispatch::FederationError,
        fetch_actor::{get_or_fetch_by_ap_id, is_local_ap_id},
        json,
        note_parser::OBJECT_TYPES,
    },
    models::{
        status::{self, Status},
        user::User,
    },
    services::ingest::{self, IngestOutcome},
    state::AppState,
};

/// Human-facing url path segment of a local status (see
/// [`crate::models::status`]): a thread link naming this instance's
/// `/notice/{uuid}` resolves against the `/objects/{uuid}` the row is
/// stored under.
const NOTICE_PATH: &str = "/notice/";
/// Object id path segment [`resolve_status`] also matches locally.
const OBJECTS_PATH: &str = "/objects/";

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
/// ingested only when its author *is* `actor`, the delivering account.
///
/// A peer cannot put somebody else's post into this instance's database
/// by naming it in a bare-id `Create`. A document authored elsewhere (or
/// not authored at all) resolves to nothing, without a fetch of that
/// other author.
///
/// # Errors
///
/// The same [`FederationError`] arms as [`resolve_status`].
pub async fn resolve_status_as_actor(
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
        .map_err(|err| FederationError::transient("the status uri read failed", &err))?;
    if let Some(row) = stored {
        return Ok(live_or_deleted(row));
    }
    if let Some(alias) = local_object_uri(&state.config, uri) {
        let aliased = status::get_by_uri(&state.db, &alias)
            .await
            .map_err(|err| FederationError::transient("the status object-id read failed", &err))?;
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
