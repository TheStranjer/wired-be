//! The incoming activity dispatcher: the `federator.incoming` job handler
//! an inbox route enqueues for every accepted `ActivityPub` `POST`, plus
//! the daily prune of the dedupe ledger it keeps.
//!
//! The reference is Pleroma's `Pleroma.Web.Federator.incoming_ap_doc/1`
//! (`lib/pleroma/web/federator.ex`) feeding the `Transmogrifier` pipeline
//! (`lib/pleroma/web/activity_pub/transmogrifier.ex`, `handle_incoming`)
//! and its policy stage (`lib/pleroma/web/activity_pub/pipeline.ex`): a
//! received activity is normalized, checked for containment in its
//! signer, deduplicated, run through the instance's filters, and routed
//! by type to the code that folds it into the database. Here those stages
//! are [`handle_incoming`], in order:
//!
//! 1. normalize the document into an [`ActivityView`] through the [`json`]
//!    helpers, then load the signer — the [`crate::federation::inbox_auth`]
//!    extractor authenticated it, and the inbox route recorded its
//!    `users.id` in the job args. An authenticated exchange proves the
//!    signer's host alive, so `note_signer_host_reachable` first-sights
//!    it in the `instances` ledger and clears any unreachable mark there;
//!    that bookkeeping only ever logs a failure, never changes the outcome;
//! 2. containment: the activity's `actor` must be the signer, its `id`
//!    must live on the actor's host, and an embedded `object` carrying its
//!    own `id` must too — unless the activity type is one whose object is
//!    by nature someone else's content (`FOREIGN_OBJECT_TYPES`);
//! 3. the dedupe ledger: one `INSERT … ON CONFLICT DO NOTHING` row of
//!    `received_activities` claims the activity under its [`dedupe_key`]
//!    (the id, plus an object digest for an `Update`); a lost claim means the
//!    activity was seen before and the job completes. A retryable failure
//!    afterwards deletes the row again, so the retry is not swallowed;
//! 4. the policy hooks: [`mrf::filter_incoming`] (the Pleroma pipeline's
//!    MRF stage) and the instance-policy `reject` domain list; rejected
//!    activities are dropped with a debug log;
//! 5. dispatch through the [`Handlers`] registry, keyed by activity type;
//!    unknown types are ignored, logged at debug. Later tickets add one
//!    line each to [`Handlers::default`]. `Create`, `Update` and `Delete`
//!    route one level deeper, by the type of the *object* they carry,
//!    through their own object-type tables; a `Create` of a type that
//!    carries two different kinds of thing — an ordinary post and, say, one
//!    option of a poll vote, both `Note`s — runs its type's pre-checks
//!    ([`Handlers::register_create_prechecked`]) before its table entry, and a
//!    `Delete` whose object no entry of that kind claims falls back to the
//!    one route that can tell a bare id naming an account from one naming a
//!    status;
//! 6. `Undo` routes through its own table by the *inner* activity's type:
//!    an embedded inner object says so itself, a bare id is looked up
//!    among the stored remote activities ([`crate::models::activity`]),
//!    and the Undo's actor must be the actor of what it undoes;
//! 7. errors: [`FederationError::Transient`] (a database or network
//!    failure that may pass on a retry) becomes [`JobError::Retry`],
//!    anything else a logged [`JobError::Discard`]. Handler calls run
//!    inside a `tracing` span carrying the activity id and type — never
//!    its body.
//!
//! The args also carry the `inbox_owner` of a personal-inbox delivery; it
//! rides on [`ActivityView::inbox_owner`] so a `Create` can silently mention
//! the owner of a bcc-addressed post. The sibling
//! [`KIND_CLEANUP_RECEIVED`] job prunes ledger rows older than 14 days;
//! the periodic scheduler enqueues it daily.
//!
//! [`json`]: crate::federation::json

use std::sync::OnceLock;

use serde_json::Value;
use tracing::{Instrument as _, debug};

use crate::{
    federation::mrf,
    jobs::{AttemptInfo, HandlerFuture, JobError},
    state::AppState,
};

mod containment;
mod error;
mod handlers;
mod identity;
mod ledger;
mod pipeline;
mod reachability;
mod routing;
#[cfg(test)]
mod test_support;
mod types;

pub use error::FederationError;
pub use handlers::Handlers;
pub use identity::dedupe_key;
pub use types::{ActivityView, CreatePreCheck, HandlerFn, ObjectRef, Outcome, UndoFn};

use ledger::prune_ledger;
use pipeline::{extract_incoming, load_signer, run_pipeline};
use reachability::note_signer_host_reachable;
use types::MrfFn;

/// The job kind of one received `ActivityPub` activity, on
/// [`QUEUE_FEDERATOR_INCOMING`](crate::jobs::QUEUE_FEDERATOR_INCOMING):
/// Pleroma's `:incoming_ap_doc`, renamed to this project's
/// `federator.incoming` (decision 11).
pub const KIND_INCOMING: &str = "federator.incoming";

/// The job kind of the daily prune of the received-activity ledger, run
/// from the [`periodic_jobs`](crate::jobs::scheduler::periodic_jobs)
/// registry on [`QUEUE_BACKGROUND`](crate::jobs::QUEUE_BACKGROUND).
pub const KIND_CLEANUP_RECEIVED: &str = "federation.cleanup_received";

/// The [`KIND_INCOMING`] handler: the pipeline the [module
/// docs](self) lay out, run against the process-wide [`Handlers`]
/// registry and the real [`mrf::filter_incoming`] policy stage.
///
/// `args` is the object the inbox route wrote:
/// `{"activity": <the received JSON>, "signer_id": <users.id of the
/// authenticated actor>, "inbox_owner": <users.id of the local owner, or
/// null for the shared inboxes>}`. `inbox_owner` is carried on the
/// [`ActivityView`] for the handlers that need it.
///
/// # Errors
///
/// [`JobError::Discard`] for arguments no inbox route should have built,
/// for a delivery whose signer is gone, and for any handler failure that
/// is not a [`FederationError::Transient`]; that transient arm —
/// [`JobError::Retry`] — is the only way this handler asks for another
/// attempt. Containment and policy drops are not errors: the activity is
/// discarded and the job completes.
pub async fn handle_incoming(state: AppState, args: Value) -> Result<(), JobError> {
    dispatch_activity(&state, args, default_handlers(), mrf::filter_incoming_boxed).await
}

/// The [`HandlerFn`](crate::jobs::HandlerFn) [`crate::serve`] binds to
/// [`KIND_INCOMING`]: the attempt info asks for nothing the dispatcher
/// does not get from the ledger itself.
pub fn handle(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { handle_incoming(state, args).await })
}

/// The [`crate::jobs::HandlerFn`] [`crate::serve`] binds to
/// [`KIND_CLEANUP_RECEIVED`]: one sweep of the ledger rows older than
/// 14 days (`RECEIVED_MAX_AGE`).
///
/// A failed sweep is a [`JobError::Retry`] — the next period's row runs
/// it again regardless, so a missed pass only lets old rows live a
/// little longer.
pub fn handle_cleanup_received(
    state: AppState,
    _attempt: AttemptInfo,
    _args: Value,
) -> HandlerFuture {
    Box::pin(async move { prune_ledger(&state.db).await })
}

/// The process-wide handler registry [`handle_incoming`] routes through,
/// built once from [`Handlers::default`].
fn default_handlers() -> &'static Handlers {
    static REGISTRY: OnceLock<Handlers> = OnceLock::new();
    REGISTRY.get_or_init(Handlers::default)
}

/// Runs one job delivery through the whole pipeline. Factored out of
/// [`handle_incoming`] (which hands it the live registry and the real MRF
/// stage) so the tests can drive it with a fake registry and a rejecting
/// filter.
async fn dispatch_activity(
    state: &AppState,
    args: Value,
    handlers: &Handlers,
    mrf_filter: MrfFn,
) -> Result<(), JobError> {
    let incoming = extract_incoming(&args)?;
    let Some(mut view) = ActivityView::parse(&incoming.activity) else {
        debug!("discarding a delivery whose id, type, actor or object cannot be normalized");
        return Ok(());
    };
    view.inbox_owner = incoming.inbox_owner;
    let span = tracing::debug_span!("incoming_activity", ap_id = %view.id, kind = %view.kind);
    async move {
        let signer = load_signer(state, incoming.signer_id).await?;
        note_signer_host_reachable(state, &signer).await;
        run_pipeline(state, view, signer, handlers, mrf_filter).await
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        test_support::{
            FIRST_ATTEMPT, app_state, follow_activity, incoming_args, ledger_rows, signer,
        },
        *,
    };
    use crate::jobs::{QUEUE_BACKGROUND, QUEUE_FEDERATOR_INCOMING, scheduler::periodic_jobs};

    #[test]
    fn the_incoming_kind_is_named_as_the_ticket_fixed() {
        assert_eq!(KIND_INCOMING, "federator.incoming", "the job kind");
        assert_eq!(
            KIND_CLEANUP_RECEIVED, "federation.cleanup_received",
            "the ledger prune kind"
        );
        assert_eq!(
            QUEUE_FEDERATOR_INCOMING, "federator_incoming",
            "and the queue the routes enqueue the incoming job on"
        );
    }

    #[test]
    fn both_kinds_are_registered_with_their_handlers() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_INCOMING).is_some(),
            "the incoming kind is registered"
        );
        assert!(
            registry.get(KIND_CLEANUP_RECEIVED).is_some(),
            "the ledger prune kind is registered"
        );
    }

    #[test]
    fn the_prune_is_scheduled_daily_on_the_background_queue() {
        let job = periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_CLEANUP_RECEIVED)
            .expect("the ledger prune is a periodic job");
        assert_eq!(job.queue, QUEUE_BACKGROUND, "slow housekeeping");
        assert_eq!(job.every, std::time::Duration::from_hours(24), "daily");
    }

    /// The registered job handler drives the same pipeline
    /// [`handle_incoming`] does: over the live registry a real `Follow` of
    /// an account this instance does not store — a delivery the follow
    /// handler claims and then ignores — runs containment, claims its
    /// ledger row, and completes.
    #[tokio::test]
    async fn the_job_handler_drives_the_pipeline() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let activity = follow_activity("handler-1");

        let outcome = handle(
            app_state(&pool),
            FIRST_ATTEMPT,
            incoming_args(&activity, &alice),
        )
        .await;

        assert_eq!(outcome, Ok(()), "the dispatcher drains real deliveries");
        assert_eq!(
            ledger_rows(&pool, "https://peer.example/activities/handler-1").await,
            1,
            "and claimed the ledger row on the way"
        );
    }
}
