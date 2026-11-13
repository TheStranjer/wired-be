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
//!    `users.id` in the job args;
//! 2. containment: the activity's `actor` must be the signer, its `id`
//!    must live on the actor's host, and an embedded `object` carrying its
//!    own `id` must too — unless the activity type is one whose object is
//!    by nature someone else's content (`FOREIGN_OBJECT_TYPES`);
//! 3. the dedupe ledger: one `INSERT … ON CONFLICT DO NOTHING` row of
//!    `received_activities` claims the activity; a lost claim means the
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
//! stays unread until the handler tickets need it. The sibling
//! [`KIND_CLEANUP_RECEIVED`] job prunes ledger rows older than 14 days;
//! the periodic scheduler enqueues it daily.

use std::{collections::HashMap, error::Error, fmt, sync::OnceLock};

use chrono::TimeDelta;
use futures::future::BoxFuture;
use serde_json::Value;
use sqlx::PgPool;
use tracing::{Instrument as _, debug, warn};
use uuid::Uuid;

use crate::{
    federation::{json, mrf},
    jobs::{AttemptInfo, HandlerFuture, JobError},
    models::{activity, instance_policy_domain, user, user::User},
    state::AppState,
};

/// The job kind of one received `ActivityPub` activity, on
/// [`QUEUE_FEDERATOR_INCOMING`](crate::jobs::QUEUE_FEDERATOR_INCOMING):
/// Pleroma's `:incoming_ap_doc`, renamed to this project's
/// `federator.incoming` (decision 11).
pub const KIND_INCOMING: &str = "federator.incoming";

/// The job kind of the daily prune of the received-activity ledger, run
/// from the [`periodic_jobs`](crate::jobs::scheduler::periodic_jobs)
/// registry on [`QUEUE_BACKGROUND`](crate::jobs::QUEUE_BACKGROUND).
pub const KIND_CLEANUP_RECEIVED: &str = "federation.cleanup_received";

/// How long a `received_activities` row lives: 14 days. A peer that
/// redelivers the same activity after that horizon gets it processed
/// again, which is the point — a two-week-old activity is long past any
/// inbox retry chain, and the ledger only has to be exact while one can
/// still be running.
const RECEIVED_MAX_AGE: TimeDelta = TimeDelta::days(14);

/// The `ActivityStreams` type of withdrawal activities, the one type the
/// dispatcher does not route by its own name but by the type of the
/// activity it undoes.
const UNDO_TYPE: &str = "Undo";

/// The `ActivityStreams` type of editing activities: like `Undo`, routed
/// one level deeper than its own name — by the type of the object it
/// carries, through [`Handlers::update_by_object_type`].
const UPDATE_TYPE: &str = "Update";

/// The `ActivityStreams` type of deletion activities, the other
/// object-type arm of [`route`], keyed through
/// [`Handlers::delete_by_object_type`] with [`Handlers::delete_fallback`]
/// behind it.
const DELETE_TYPE: &str = "Delete";

/// The `ActivityStreams` type of announcement activities: the third
/// object-type arm of [`route`], keyed through
/// [`Handlers::create_by_object_type`] — the code that folds a created
/// `Note` and the code that will fold a created `ChatMessage` have nothing
/// in common beyond the `Create` around them, so the object's own type
/// routes.
const CREATE_TYPE: &str = "Create";

/// Activity types whose `object` is by design a reference to content that
/// lives somewhere else — someone else's note, actor, or custom emoji —
/// so the containment rule of the embedded object's host does not apply
/// to them. Pleroma's `Transmogrifier` and `ObjectValidators` treat
/// exactly these as the activities that legitimately speak about foreign
/// objects.
const FOREIGN_OBJECT_TYPES: [&str; 10] = [
    "Announce",
    "Like",
    "EmojiReact",
    "Follow",
    "Block",
    "Flag",
    "Undo",
    "Delete",
    "Add",
    "Remove",
];

/// The ledger claim: one row per activity id, and a lost claim is the
/// `rows_affected = 0` that says "already seen".
const INSERT_RECEIVED_SQL: &str =
    "INSERT INTO received_activities (ap_id) VALUES ($1) ON CONFLICT DO NOTHING";
/// Releasing a claim so the next attempt of the same job can run the
/// activity again.
const DELETE_RECEIVED_SQL: &str = "DELETE FROM received_activities WHERE ap_id = $1";
/// The daily prune of the ledger rows past [`RECEIVED_MAX_AGE`].
const PRUNE_RECEIVED_SQL: &str = "DELETE FROM received_activities WHERE inserted_at < now() - $1";

/// Everything the dispatcher can do with a received activity, as the
/// per-type handlers report it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The handler folded the activity; the job completes.
    Handled,
    /// The handler saw nothing to do; the reason is static — recorded in
    /// the debug log, the job completes the same.
    Ignored(&'static str),
}

/// The `object` of a normalized activity.
#[derive(Clone, Debug)]
pub enum ObjectRef {
    /// A reference to an object kept elsewhere: a bare id string, or an
    /// object/array shape whose first reference ([`json::value_or_id`])
    /// wins.
    Id(String),
    /// A full embedded document, kept verbatim for the handlers that fold
    /// objects (and inspectable through [`ObjectRef::id`] for the id it
    /// carries).
    Embedded(Value),
}

impl ObjectRef {
    /// Normalizes one `object` field value: an object is embedded, an
    /// array recurses into its first element (the one-or-many rule), and
    /// anything else must carry a reference to be an [`ObjectRef::Id`].
    /// `None` — for a missing, null, or reference-less object — means the
    /// activity describes nothing the dispatcher could route.
    #[must_use]
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Object(_) => Some(Self::Embedded(value.clone())),
            Value::Array(values) => values.first().and_then(Self::from_value),
            Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {
                json::value_or_id(value).map(Self::Id)
            }
        }
    }

    /// The id this object names: the string of a reference, or the `id`
    /// field of an embedded document (an embedded document may carry none,
    /// which is why this answers an `Option`).
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Id(id) => Some(id.as_str()),
            Self::Embedded(object) => object.get("id").and_then(Value::as_str),
        }
    }

    /// The embedded document, for the handlers that fold objects.
    #[must_use]
    pub const fn embedded(&self) -> Option<&Value> {
        match self {
            Self::Embedded(object) => Some(object),
            Self::Id(_) => None,
        }
    }
}

/// A received activity normalized through the [`json`] helpers: every
/// field the wire protocol may send as a string, an object, or an array
/// arrives here in exactly one shape.
///
/// A type handler therefore never branches on JSON cardinality.
#[derive(Clone, Debug)]
pub struct ActivityView {
    /// The activity's `ActivityPub` id — the dedupe key and the subject
    /// of `Delete`/`Undo` chains.
    pub id: String,
    /// The activity type: the first string of a `type` field that may
    /// itself be an array.
    pub kind: String,
    /// The acting actor's id — a string, an actor object's `id`, or the
    /// first reference of an array, normalized to the id alone.
    pub actor: String,
    /// The activity's object: a reference or an embedded document.
    pub object: ObjectRef,
    /// The `target` of `Add`/`Remove`/`Move` activities, normalized to a
    /// reference; `None` when the activity carries none.
    pub target: Option<String>,
    /// The `to` addressing, every element normalized to a reference.
    pub to: Vec<String>,
    /// The `cc` addressing, every element normalized to a reference.
    pub cc: Vec<String>,
    /// The document as received, for handlers that need fields beyond the
    /// normalized five and for the MRF's re-normalization.
    pub raw: Value,
}

impl ActivityView {
    /// Normalizes a received document, `None` when it is not a dispatch-
    /// able activity: an `id` string, a first `type` string, an `actor`
    /// reference and an `object` are the minimum the dispatcher's own
    /// steps (containment, dedupe, routing) all need.
    #[must_use]
    pub(crate) fn parse(raw: &Value) -> Option<Self> {
        let id = raw.get("id").and_then(Value::as_str)?;
        let kind = raw.get("type").and_then(json::first_type)?;
        let actor = raw.get("actor").and_then(json::value_or_id)?;
        let object = raw.get("object").and_then(ObjectRef::from_value)?;
        Some(Self {
            id: id.to_owned(),
            kind,
            actor,
            object,
            target: raw.get("target").and_then(json::value_or_id),
            to: raw.get("to").map_or_else(Vec::new, json::references),
            cc: raw.get("cc").map_or_else(Vec::new, json::references),
            raw: raw.clone(),
        })
    }
}

/// What a per-type handler does with a normalized activity: folds it,
/// ignores it, or fails with a [`FederationError`] the dispatcher maps
/// onto the job.
pub type HandlerFn =
    fn(AppState, ActivityView, User) -> BoxFuture<'static, Result<Outcome, FederationError>>;

/// What an `Undo` handler does with an `Undo` whose inner activity has
/// already been resolved to its type and checked against its actor: the
/// same contract as a [`HandlerFn`], registered per inner type.
pub type UndoFn = HandlerFn;

/// What a `Create` pre-check decides about an embedded object: `true` claims
/// the activity for the handler registered beside the guard, `false` leaves
/// it to the object type's own entry.
///
/// A guard that must consult the database takes the cheap `AppState` by
/// value and copies whatever it needs out of the document before its first
/// `await`, exactly like [`mrf::filter_incoming_boxed`] — that is why the
/// document arrives borrowed while the future may not be.
///
/// [`mrf::filter_incoming_boxed`]: crate::federation::mrf::filter_incoming_boxed
pub type CreatePreCheck = fn(AppState, &Value) -> BoxFuture<'static, bool>;

/// One guarded `Create` route: the object type whose entry it runs ahead of,
/// the guard, and the handler the guard hands a claimed activity to.
#[derive(Clone, Copy, Debug)]
struct CreateGuard {
    /// The declared object `type` this guard is consulted for — the same key
    /// [`Handlers::register_create`] binds.
    object_type: &'static str,
    /// Whether the delivered document is the specialised shape.
    check: CreatePreCheck,
    /// The handler a claimed document routes to.
    handler: HandlerFn,
}

/// The policy stage of the pipeline, as a function pointer so the
/// dispatcher's test seam can hand it a rejecting filter.
type MrfFn = fn(AppState, &ActivityView) -> BoxFuture<'static, mrf::MrfDecision>;

/// The type-to-handler tables the dispatcher routes through.
///
/// [`Handlers::default`] is the registry the live dispatcher runs: the
/// composition of every handler module in
/// [`crate::federation::handlers`]. A ticket that teaches the instance a
/// new activity type adds its routes to its own handler module, and nothing
/// here. [`Handlers::new`] is the seam the tests build fake registries with.
#[derive(Debug)]
pub struct Handlers {
    /// Routes keyed by the activity's own type.
    by_type: HashMap<&'static str, HandlerFn>,
    /// `Undo` routes, keyed by the inner activity's type.
    undo_by_type: HashMap<&'static str, UndoFn>,
    /// `Update` routes, keyed by the type of the *object* the activity
    /// carries: updating a `Person` and updating a `Note` are different
    /// code, and only the object's own type says which one a delivery
    /// needs.
    update_by_object_type: HashMap<&'static str, HandlerFn>,
    /// `Delete` routes, keyed the same way, for the peers that send an
    /// embedded document.
    delete_by_object_type: HashMap<&'static str, HandlerFn>,
    /// `Create` routes, keyed the same way: which code folds a delivered
    /// object depends on the object's own `type`, so a `Create` of a
    /// `Note` and the later `Create` of a `ChatMessage` register one
    /// entry each beside each other.
    create_by_object_type: HashMap<&'static str, HandlerFn>,
    /// The `Create` route for an object that is only a reference: an
    /// embedded document declares a `type` to route on, a bare id declares
    /// nothing, and exactly one handler can take the whole shape — the one
    /// that dereferences the id and folds what it answers (see
    /// [`crate::federation::fetch_object`]).
    create_by_reference: Option<HandlerFn>,
    /// `Create` pre-checks, in registration order: a delivered object of a
    /// keyed type is put to each of their guards before that type's entry in
    /// [`Handlers::create_by_object_type`] folds it, which is how one `type`
    /// that carries two different kinds of thing — a `Note` that is a post
    /// and a `Note` that is one option of a poll vote — reaches two
    /// different handlers.
    create_prechecks: Vec<CreateGuard>,
    /// The `Delete` route for an object [`Handlers::delete_by_object_type`]
    /// does not claim — a bare id, or an embedded document of a type nobody
    /// registered. An id is the only thing that can tell a peer deleting
    /// its own account from one deleting a status, so that is where this
    /// resolution belongs.
    delete_fallback: Option<HandlerFn>,
}

/// The live registry, built by the handler modules themselves.
impl Default for Handlers {
    fn default() -> Self {
        crate::federation::handlers::registry()
    }
}

impl Handlers {
    /// An empty registry: dispatching through it ignores everything. The
    /// tests' seam; [`Handlers::default`] is the one the worker runs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_type: HashMap::new(),
            undo_by_type: HashMap::new(),
            update_by_object_type: HashMap::new(),
            delete_by_object_type: HashMap::new(),
            create_by_object_type: HashMap::new(),
            create_by_reference: None,
            create_prechecks: Vec::new(),
            delete_fallback: None,
        }
    }

    /// Binds one activity type to its handler.
    pub fn register(&mut self, kind: &'static str, handler: HandlerFn) {
        let _previous = self.by_type.insert(kind, handler);
    }

    /// Binds the `Undo` of one inner activity type to its handler.
    pub fn register_undo(&mut self, inner_type: &'static str, handler: UndoFn) {
        let _previous = self.undo_by_type.insert(inner_type, handler);
    }

    /// Binds one `Update`d object type to its handler, filling the
    /// registry's `update_by_object_type` table.
    pub fn register_update(&mut self, object_type: &'static str, handler: HandlerFn) {
        let _previous = self.update_by_object_type.insert(object_type, handler);
    }

    /// Binds one `Delete`d object type to its handler, filling the
    /// registry's `delete_by_object_type` table.
    pub fn register_delete(&mut self, object_type: &'static str, handler: HandlerFn) {
        let _previous = self.delete_by_object_type.insert(object_type, handler);
    }

    /// Binds one `Create`d object type to its handler, filling the
    /// registry's `create_by_object_type` table.
    pub fn register_create(&mut self, object_type: &'static str, handler: HandlerFn) {
        let _previous = self.create_by_object_type.insert(object_type, handler);
    }

    /// Sets the handler a `Create` whose object is only a reference goes
    /// to — the [`register_delete_fallback`] of the `Create` route, for
    /// the code that dereferences the id instead of ignoring it.
    ///
    /// [`register_delete_fallback`]: Handlers::register_delete_fallback
    pub fn register_create_reference(&mut self, handler: HandlerFn) {
        self.create_by_reference = Some(handler);
    }

    /// Binds a guarded `Create` route: `check` puts the embedded document of
    /// a `Create`d `object_type` to its shape test, and a claimed
    /// activity goes to `handler` instead of to the type's entry in
    /// [`Handlers::register_create`].
    ///
    /// This is the "before" half of a shared object `type`: a poll vote
    /// arrives as a plain `Create` of a `Note` and only a read of the
    /// document (and, through its `inReplyTo`, of the database) tells it
    /// apart from the post it superficially is, so the note ticket's
    /// unguarded entry stays the fallback and the specialised ticket runs its
    /// guard first. Guards of one type run in registration order, and the
    /// first that claims the document ends the walk.
    pub fn register_create_prechecked(
        &mut self,
        object_type: &'static str,
        check: CreatePreCheck,
        handler: HandlerFn,
    ) {
        self.create_prechecks.push(CreateGuard {
            object_type,
            check,
            handler,
        });
    }

    /// The guarded route that claims this `Create`, `None` when no guard of
    /// `object_type` does: [`route_create`] runs the walk through here so the
    /// unguarded type table is consulted only once nothing claimed the
    /// document.
    async fn claimed_create(
        &self,
        object_type: &str,
        state: &AppState,
        object: &Value,
    ) -> Option<HandlerFn> {
        for guard in &self.create_prechecks {
            if guard.object_type != object_type {
                continue;
            }
            if (guard.check)(state.clone(), object).await {
                return Some(guard.handler);
            }
        }
        None
    }

    /// Whether the `Create` sub-registry claims `object_type` — the read
    /// the handler modules' own tests use to check what they registered
    /// without reaching into the table.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn handles_create(&self, object_type: &str) -> bool {
        self.create_by_object_type.contains_key(object_type)
    }

    /// Whether the registry takes a `Create` whose object is only a
    /// reference — the [`handles_create`](Self::handles_create) question
    /// asked of the `create_by_reference` route.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn takes_create_reference(&self) -> bool {
        self.create_by_reference.is_some()
    }

    /// Whether the `Update` sub-registry claims `object_type` — the
    /// [`handles_create`](Self::handles_create) question for the edit
    /// routes, asked by the handler modules that register them.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn handles_update(&self, object_type: &str) -> bool {
        self.update_by_object_type.contains_key(object_type)
    }

    /// Whether a `Create` of `object_type` is put to a pre-check before it
    /// reaches the object type's own handler — the [`handles_create`]
    /// question asked of the guarded half of the sub-registry.
    ///
    /// [`handles_create`]: Self::handles_create
    #[cfg(test)]
    #[must_use]
    pub(crate) fn pre_checks_create(&self, object_type: &str) -> bool {
        self.create_prechecks
            .iter()
            .any(|guard| guard.object_type == object_type)
    }

    /// Whether the `Delete` sub-registry claims `object_type` — the
    /// [`handles_create`](Self::handles_create) question one level deeper,
    /// for the handler modules that register both arms of a type.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn handles_delete(&self, object_type: &str) -> bool {
        self.delete_by_object_type.contains_key(object_type)
    }

    /// Sets the fallback the `Delete` route reaches when no entry of the
    /// `delete_by_object_type` table claims the delivered object.
    pub fn register_delete_fallback(&mut self, handler: HandlerFn) {
        self.delete_fallback = Some(handler);
    }
}

/// Every way an activity can fail on the way in, as the handlers and the
/// dispatcher's own stages report it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FederationError {
    /// A database or network failure: the work may succeed on a later
    /// attempt, so the dispatcher turns it into a [`JobError::Retry`] and
    /// releases the dedupe row for the retry to find.
    Transient(String),
    /// A failure no retry can fix — a malformed document, an operation
    /// the peer got wrong: the dispatcher logs it and discards the job.
    Permanent(String),
}

impl fmt::Display for FederationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transient(detail) => write!(f, "transient federation failure: {detail}"),
            Self::Permanent(detail) => write!(f, "permanent federation failure: {detail}"),
        }
    }
}

impl Error for FederationError {}

/// The [`KIND_INCOMING`] handler: the pipeline the [module
/// docs](self) lay out, run against the process-wide [`Handlers`]
/// registry and the real [`mrf::filter_incoming`] policy stage.
///
/// `args` is the object the inbox route wrote:
/// `{"activity": <the received JSON>, "signer_id": <users.id of the
/// authenticated actor>, "inbox_owner": <users.id of the local owner, or
/// null for the shared inboxes>}`. `inbox_owner` is carried along unread
/// until the handler tickets need it.
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
    let Some(view) = ActivityView::parse(&incoming.activity) else {
        debug!("discarding a delivery whose id, type, actor or object cannot be normalized");
        return Ok(());
    };
    let span = tracing::debug_span!("incoming_activity", ap_id = %view.id, kind = %view.kind);
    async move {
        let signer = load_signer(state, incoming.signer_id).await?;
        run_pipeline(state, view, signer, handlers, mrf_filter).await
    }
    .instrument(span)
    .await
}

/// The activity document and the signer's id, exactly as the inbox route
/// wrote them.
struct Incoming {
    /// The received `ActivityPub` document.
    activity: Value,
    /// The `users.id` of the actor whose key authenticated the delivery.
    signer_id: Uuid,
}

/// Reads [`Incoming`] out of the job args; an args object that does not
/// describe a delivery no inbox route enqueued is discarded, not retried,
/// because no retry can make it one.
fn extract_incoming(args: &Value) -> Result<Incoming, JobError> {
    let discard = |why: &str| JobError::Discard(format!("unusable {KIND_INCOMING} args: {why}"));
    let activity = args.get("activity").ok_or_else(|| discard("no activity"))?;
    let signer_id = args
        .get("signer_id")
        .and_then(Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok())
        .ok_or_else(|| discard("no signer id"))?;
    Ok(Incoming {
        activity: activity.clone(),
        signer_id,
    })
}

/// Loads the signer the [`SignedActivity`] extractor authenticated. A
/// gone row means the account was deleted after the delivery was queued:
/// nothing to process it for any more.
///
/// [`SignedActivity`]: crate::federation::inbox_auth::SignedActivity
async fn load_signer(state: &AppState, id: Uuid) -> Result<User, JobError> {
    user::get_by_id(&state.db, id)
        .await
        .map_err(|err| JobError::Retry(format!("could not read the signer: {err}")))?
        .ok_or_else(|| JobError::Discard("the signer account is gone".to_owned()))
}

/// Containment, the ledger claim, and everything after them.
async fn run_pipeline(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
    mrf_filter: MrfFn,
) -> Result<(), JobError> {
    if let Some(reason) = containment_failure(&view, &signer) {
        debug!(reason = %reason, "the activity fails containment; discarding");
        return Ok(());
    }
    if !claim_ledger_row(state, &view.id).await? {
        debug!("this activity has already been received; nothing to do");
        return Ok(());
    }
    let outcome = process(state, &view, signer, handlers, mrf_filter).await;
    if let Err(err) = &outcome
        && is_retryable(err)
    {
        release_ledger_row(state, &view.id).await;
    }
    outcome
}

/// Where the activity could not belong to the actor that sent it — the
/// re-check of what the inbox already enforced, in the one place that
/// processes activities without the HTTP layer's guarantees. `None` means
/// it passes.
fn containment_failure(view: &ActivityView, signer: &User) -> Option<String> {
    if view.actor != signer.ap_id {
        return Some(format!(
            "the activity actor {} is not the signer {}",
            view.actor, signer.ap_id
        ));
    }
    let Some(actor_host) = json::http_host(&view.actor) else {
        return Some(format!("the actor id {} is not an http(s) url", view.actor));
    };
    let id_host = json::http_host(&view.id);
    if id_host.as_deref() != Some(actor_host.as_str()) {
        return Some(format!(
            "the activity id host {} differs from the actor host {actor_host}",
            id_host.as_deref().unwrap_or("<none>")
        ));
    }
    object_containment_failure(view, &actor_host)
}

/// The embedded-object arm of containment: an object carried inside the
/// activity that names its own `id` must live on the actor's host, unless
/// the activity type is one that legitimately speaks about someone
/// else's content ([`FOREIGN_OBJECT_TYPES`]).
fn object_containment_failure(view: &ActivityView, actor_host: &str) -> Option<String> {
    let ObjectRef::Embedded(object) = &view.object else {
        return None;
    };
    let object_id = object.get("id").and_then(Value::as_str)?;
    let Some(object_host) = json::http_host(object_id) else {
        return Some(format!(
            "the embedded object id {object_id} is not an http(s) url"
        ));
    };
    if object_host == actor_host || FOREIGN_OBJECT_TYPES.contains(&view.kind.as_str()) {
        return None;
    }
    Some(format!(
        "the embedded object host {object_host} differs from the actor host {actor_host}"
    ))
}

/// Claims the ledger row: `true` when this is the first sight of the id.
/// The claim itself failing is the database being unreachable — the one
/// failure mode that must retry before anything is decided about the
/// activity.
async fn claim_ledger_row(state: &AppState, ap_id: &str) -> Result<bool, JobError> {
    let written = sqlx::query(INSERT_RECEIVED_SQL)
        .bind(ap_id)
        .execute(&state.db)
        .await
        .map_err(|err| JobError::Retry(format!("the received-activity claim failed: {err}")))?;
    Ok(written.rows_affected() > 0)
}

/// Releases a claim whose processing will be retried, so the next attempt
/// finds the door open. A release that fails leaves the row to the daily
/// prune — one swallowed retry inside two weeks is the cost.
async fn release_ledger_row(state: &AppState, ap_id: &str) {
    if let Err(err) = sqlx::query(DELETE_RECEIVED_SQL)
        .bind(ap_id)
        .execute(&state.db)
        .await
    {
        warn!(ap_id, %err, "could not release the ledger row of a retrying activity");
    }
}

/// Whether the worker will run the job again, which is exactly when the
/// ledger row must not stand in its way.
const fn is_retryable(err: &JobError) -> bool {
    matches!(err, JobError::Retry(_) | JobError::RetryAfter(_, _))
}

/// The policy hooks and the registry dispatch, after the ledger claim.
async fn process(
    state: &AppState,
    view: &ActivityView,
    signer: User,
    handlers: &Handlers,
    mrf_filter: MrfFn,
) -> Result<(), JobError> {
    let accepted = match mrf_filter(state.clone(), view).await {
        mrf::MrfDecision::Reject(reason) => {
            debug!(reason = %reason, "the policy pipeline rejected the activity");
            return Ok(());
        }
        mrf::MrfDecision::Accept(value) => value,
    };
    let Some(processed) = ActivityView::parse(&accepted) else {
        debug!("the policy pipeline accepted a document that does not parse; dropping");
        return Ok(());
    };
    if policy_rejects(state, &processed).await? {
        debug!("instance policy rejects the actor's domain");
        return Ok(());
    }
    route(state, processed, signer, handlers).await
}

/// The instance-policy arm of the policy hooks: a signer host on the
/// `reject` list. The inbox refuses these deliveries at the door; this is
/// the same check inside the queue, where the [MRF
/// stage](mrf::filter_incoming) may have rewritten the actor.
async fn policy_rejects(state: &AppState, view: &ActivityView) -> Result<bool, JobError> {
    // Containment already proved the actor an http(s) url with a host.
    let Some(host) = json::http_host(&view.actor) else {
        return Ok(false);
    };
    instance_policy_domain::is_rejected(&state.db, &host)
        .await
        .map_err(|err| JobError::Retry(format!("the instance policy read failed: {err}")))
}

/// The registry dispatch: `Undo` routes by its inner activity's type,
/// `Update` and `Delete` by the type of the object they carry, and every
/// other type by its own name; an unregistered type is ignored.
async fn route(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
) -> Result<(), JobError> {
    match view.kind.as_str() {
        UNDO_TYPE => route_undo(state, view, signer, handlers).await,
        UPDATE_TYPE => route_update(state, view, signer, handlers).await,
        DELETE_TYPE => route_delete(state, view, signer, handlers).await,
        CREATE_TYPE => route_create(state, view, signer, handlers).await,
        _ => route_by_type(state, view, signer, handlers).await,
    }
}

/// The plain arm of the dispatch: the activity's own type, or nothing.
async fn route_by_type(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
) -> Result<(), JobError> {
    match handlers.by_type.get(view.kind.as_str()) {
        None => {
            debug!(kind = %view.kind, "no handler for this activity type; ignoring");
            Ok(())
        }
        Some(handler) => call_handler(*handler, state, view, signer).await,
    }
}

/// The `Update` arm: the embedded object's own type decides which code
/// folds it, because an updated actor and an updated status have nothing in
/// common beyond the activity around them. An object that is a bare
/// reference declares no type and is ignored, and so is any type no ticket
/// has registered a route for yet.
async fn route_update(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
) -> Result<(), JobError> {
    let declared = object_type(&view);
    let handler = declared
        .as_deref()
        .and_then(|kind| handlers.update_by_object_type.get(kind))
        .copied();
    if let Some(handler) = handler {
        call_handler(handler, state, view, signer).await
    } else {
        debug!(
            object_type = declared.as_deref().unwrap_or("<reference>"),
            "no Update handler for this object type; ignoring"
        );
        Ok(())
    }
}

/// The `Delete` arm: an embedded document whose type a sub-registry entry
/// claims goes to that entry; everything else — a bare id, a `Tombstone`, a
/// type nobody registered — goes to [`Handlers::delete_fallback`], which
/// resolves the id and decides whether it names an account. With no
/// fallback registered the activity is ignored.
async fn route_delete(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
) -> Result<(), JobError> {
    let declared = object_type(&view);
    let handler = declared
        .as_deref()
        .and_then(|kind| handlers.delete_by_object_type.get(kind))
        .or(handlers.delete_fallback.as_ref())
        .copied();
    if let Some(handler) = handler {
        call_handler(handler, state, view, signer).await
    } else {
        debug!(
            object_type = declared.as_deref().unwrap_or("<reference>"),
            "no Delete handler for this object; ignoring"
        );
        Ok(())
    }
}

/// The type an activity's object declares, `None` when the object is a
/// bare reference: the key both object-type sub-registries route on.
fn object_type(view: &ActivityView) -> Option<String> {
    view.object
        .embedded()
        .and_then(|object| object.get("type"))
        .and_then(json::first_type)
}

/// The `Create` arm: the embedded object's own type decides which code
/// folds it — a `Note` becomes a status, a later ticket's `ChatMessage`
/// becomes a chat message — through [`Handlers::create_by_object_type`].
/// A type that carries more than one kind of thing is put to its
/// [pre-checks](Handlers::register_create_prechecked) first, in registration
/// order, and a claimed document leaves through the guard's own handler.
/// An object that is a bare reference declares no type; it goes to
/// [`Handlers::create_by_reference`], whose handler dereferences the id and
/// folds what answers — with no reference handler registered such a
/// `Create` is ignored, as is any type no ticket has routed yet.
async fn route_create(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
) -> Result<(), JobError> {
    if view.object.embedded().is_none() {
        if let Some(handler) = handlers.create_by_reference {
            return call_handler(handler, state, view, signer).await;
        }
        debug!("no Create handler takes a bare object reference; ignoring");
        return Ok(());
    }
    let declared = object_type(&view);
    let claimed = match (declared.as_deref(), view.object.embedded()) {
        (Some(kind), Some(object)) => handlers.claimed_create(kind, state, object).await,
        (Some(_), None) | (None, _) => None,
    };
    if let Some(handler) = claimed {
        return call_handler(handler, state, view, signer).await;
    }
    let handler = declared
        .as_deref()
        .and_then(|kind| handlers.create_by_object_type.get(kind))
        .copied();
    if let Some(handler) = handler {
        call_handler(handler, state, view, signer).await
    } else {
        debug!(
            object_type = declared.as_deref().unwrap_or("<untyped>"),
            "no Create handler for this object type; ignoring"
        );
        Ok(())
    }
}

/// The `Undo` arm: resolve what is being undone, refuse an `Undo` whose
/// actor is not the actor of the activity it undoes, then route by the
/// inner type through the `Undo` table.
async fn route_undo(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
) -> Result<(), JobError> {
    let Some((inner_type, inner_actor)) = undo_inner(state, &view).await? else {
        return Ok(());
    };
    if inner_actor != view.actor {
        debug!(
            inner_actor = %inner_actor,
            "an Undo whose actor is not the actor of what it undoes; discarding"
        );
        return Ok(());
    }
    match handlers.undo_by_type.get(inner_type.as_str()) {
        None => {
            debug!(inner_type = %inner_type, "no Undo handler for the inner activity type; ignoring");
            Ok(())
        }
        Some(handler) => call_handler(*handler, state, view, signer).await,
    }
}

/// The inner activity of an `Undo` as `(type, actor id)`: an embedded
/// object states both itself; an object that is only an id is looked up
/// among the stored remote activities. `None` (after the debug log that
/// says why) is an `Undo` of nothing this instance can route.
async fn undo_inner(
    state: &AppState,
    view: &ActivityView,
) -> Result<Option<(String, String)>, JobError> {
    match &view.object {
        ObjectRef::Embedded(object) => {
            let Some(kind) = object.get("type").and_then(json::first_type) else {
                debug!("an Undo of an embedded object without a type; ignoring");
                return Ok(None);
            };
            let actor = object
                .get("actor")
                .and_then(json::value_or_id)
                .unwrap_or_else(|| view.actor.clone());
            Ok(Some((kind, actor)))
        }
        ObjectRef::Id(id) => {
            let stored = activity::find_by_ap_id(&state.db, id)
                .await
                .map_err(|err| JobError::Retry(format!("the undo lookup failed: {err}")))?;
            match stored {
                None => {
                    debug!(inner = %id, "an Undo of an activity this instance never stored; ignoring");
                    Ok(None)
                }
                Some((kind, actor)) => Ok(Some((kind.to_owned(), actor))),
            }
        }
    }
}

/// Runs one handler inside the activity's span and maps its failure onto
/// the job's treatment.
async fn call_handler(
    handler: HandlerFn,
    state: &AppState,
    view: ActivityView,
    signer: User,
) -> Result<(), JobError> {
    match handler(state.clone(), view, signer).await {
        Ok(Outcome::Handled) => {
            debug!("the activity was handled");
            Ok(())
        }
        Ok(Outcome::Ignored(reason)) => {
            debug!(reason, "the handler ignored the activity");
            Ok(())
        }
        Err(err) => Err(job_error_from(err)),
    }
}

/// Maps a handler's failure onto the job's treatment, per the ticket's
/// step 7: transient is a retry, everything else a logged discard.
fn job_error_from(err: FederationError) -> JobError {
    match err {
        FederationError::Transient(detail) => JobError::Retry(detail),
        FederationError::Permanent(detail) => {
            warn!(%detail, "discarding an activity its handler refused");
            JobError::Discard(detail)
        }
    }
}

/// One prune pass: the ledger rows older than [`RECEIVED_MAX_AGE`].
async fn prune_ledger(db: &PgPool) -> Result<(), JobError> {
    match sqlx::query(PRUNE_RECEIVED_SQL)
        .bind(RECEIVED_MAX_AGE)
        .execute(db)
        .await
    {
        Ok(deleted) => {
            let count = deleted.rows_affected();
            if count > 0 {
                debug!(count, "pruned received-activity ledger rows");
            }
            Ok(())
        }
        Err(err) => Err(JobError::Retry(format!(
            "received-activity ledger prune failed: {err}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::Config,
        federation::mrf::MrfDecision,
        http_client::HttpClient,
        jobs::{QUEUE_BACKGROUND, QUEUE_FEDERATOR_INCOMING, scheduler::periodic_jobs},
        models::user::{Flag, NewUser, insert, local_ap_id},
        uploads::UploaderBackend,
    };

    /// Every `(label, activity id)` a fake handler ran for. Handlers are
    /// `fn` pointers that cannot capture a counter, so the fakes record
    /// here; tests count their own unique activity ids and never collide
    /// with the suite's concurrent runs.
    static SEEN: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

    /// Records a fake handler run.
    fn record(label: &str, view: &ActivityView) {
        let mut seen = SEEN.lock().expect("the test lock");
        seen.push((label.to_owned(), view.id.clone()));
    }

    /// How often the handler labelled `label` ran for the activity `ap_id`.
    fn seen(label: &str, ap_id: &str) -> usize {
        SEEN.lock()
            .expect("the test lock")
            .iter()
            .filter(|(seen_label, seen_id)| seen_label == label && seen_id == ap_id)
            .count()
    }

    /// Test state over `pool`.
    fn app_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// Test state over a pool that cannot answer: the database being
    /// unreachable, as the handlers see it.
    async fn broken_state() -> AppState {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        pool.close().await;
        app_state(&pool)
    }

    /// The signer fixture: a mirror of `alice` on `peer.example`.
    async fn signer(pool: &PgPool) -> User {
        insert(
            pool,
            &NewUser {
                nickname: "alice@peer.example".to_owned(),
                ap_id: "https://peer.example/users/alice".to_owned(),
                local: Flag::new(false),
                domain: Some("peer.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// The local account fixture, for the rows a remote actor's activity
    /// points at.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id("https://test.example", nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// The args shape an inbox route writes.
    fn args_with(activity: &Value, signer_id: Uuid) -> Value {
        json!({"activity": activity, "signer_id": signer_id, "inbox_owner": null})
    }

    /// The args of a delivery authenticated by `who`.
    fn incoming_args(activity: &Value, who: &User) -> Value {
        args_with(activity, who.id)
    }

    /// A `Follow` of the local `bob` from `alice@peer.example`, with the
    /// activity id `n`.
    fn follow_activity(n: &str) -> Value {
        json!({
            "id": format!("https://peer.example/activities/{n}"),
            "type": "Follow",
            "actor": "https://peer.example/users/alice",
            "object": "https://test.example/users/bob",
        })
    }

    /// An `Undo` by alice of the activity id `what`, named by bare id.
    fn undo_activity(n: &str, what: &str) -> Value {
        json!({
            "id": format!("https://peer.example/activities/{n}"),
            "type": "Undo",
            "actor": "https://peer.example/users/alice",
            "object": what,
        })
    }

    /// The registry the dispatch tests run: fake handlers on every type
    /// they deliver. `Create` routes by its object's type through the
    /// sub-registry, like the live one does.
    fn test_handlers() -> Handlers {
        let mut handlers = Handlers::new();
        handlers.register("Follow", ok_handler);
        handlers.register_create("Note", ok_handler);
        handlers.register("Announce", ok_handler);
        handlers.register("Like", transient_handler);
        handlers.register("Block", permanent_handler);
        handlers.register_undo("Follow", undo_follow_handler);
        handlers.register_undo("Announce", undo_announce_handler);
        handlers
    }

    /// A fake handler that records the run and handles.
    fn ok_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("ok", &view);
            Ok(Outcome::Handled)
        })
    }

    /// A fake handler that fails the way an unreachable database looks.
    fn transient_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("transient", &view);
            Err(FederationError::Transient("injected".to_owned()))
        })
    }

    /// A fake handler that fails the way a refused operation looks.
    fn permanent_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("permanent", &view);
            Err(FederationError::Permanent("injected".to_owned()))
        })
    }

    /// The `Update` fake for an actor object.
    fn update_person_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("update-person", &view);
            Ok(Outcome::Handled)
        })
    }

    /// The `Create` pre-check fake of the vote ticket's guard: it claims every
    /// document that names an option and passes over every other one.
    fn claims_named(_state: AppState, object: &Value) -> BoxFuture<'static, bool> {
        let named = object
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty());
        Box::pin(async move { named })
    }

    /// The handler the [`claims_named`] guard routes to.
    fn prechecked_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("prechecked", &view);
            Ok(Outcome::Handled)
        })
    }

    /// The `Delete` fake for an object type the sub-registry claims.
    fn delete_tombstone_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("delete-tombstone", &view);
            Ok(Outcome::Handled)
        })
    }

    /// The `Delete` fake for the fallback: the route that resolves an id.
    fn delete_fallback_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("delete-fallback", &view);
            Ok(Outcome::Handled)
        })
    }

    /// The `Undo` fake for a stored `Follow`.
    fn undo_follow_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("undo-follow", &view);
            Ok(Outcome::Handled)
        })
    }

    /// The `Undo` fake for a stored `Announce`.
    fn undo_announce_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("undo-announce", &view);
            Ok(Outcome::Handled)
        })
    }

    /// The rejecting policy fake.
    fn reject_mrf(_state: AppState, _view: &ActivityView) -> BoxFuture<'static, MrfDecision> {
        Box::pin(async { MrfDecision::Reject("test policy".to_owned()) })
    }

    /// How many ledger rows carry `ap_id`.
    async fn ledger_rows(pool: &PgPool, ap_id: &str) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM received_activities WHERE ap_id = $1")
            .bind(ap_id)
            .fetch_one(pool)
            .await
            .expect("the ledger reads")
    }

    /// The first attempt info, for the job-shaped handlers.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };

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

    /// The live registry holds the actor routes, the note-create routes, the
    /// chat routes and the poll-vote pre-check and nothing else: the `Update`
    /// table is keyed by the five actor object types, `Delete` runs through
    /// the object-type entry the chat handler claims and the fallback that
    /// resolves an object id, and the `Create` table is keyed by the eight
    /// `Note`-family object types the parser accepts plus the chat handler's
    /// `ChatMessage`, with the poll ticket's guard running ahead of the
    /// `Note` entry it shares with the note handler. The object-type tables
    /// are also the entry points later tickets add their own keys to.
    #[test]
    fn the_live_registry_holds_the_actor_routes() {
        let handlers = Handlers::default();
        assert!(
            handlers.by_type.is_empty() && handlers.undo_by_type.is_empty(),
            "no activity type is routed by its own name yet, and no Undo either"
        );
        for object_type in ["Person", "Service", "Group", "Organization", "Application"] {
            assert!(
                handlers.update_by_object_type.contains_key(object_type),
                "an Update of a {object_type} folds through the actor handler"
            );
        }
        for object_type in ["Note", "Question", "Article", "Page"] {
            assert!(
                handlers.update_by_object_type.contains_key(object_type),
                "an Update of a {object_type} folds through the note-edit handler"
            );
        }
        assert_eq!(
            handlers.update_by_object_type.len(),
            9,
            "the actor objects plus the note family's editable four"
        );
        assert!(
            handlers.delete_by_object_type.contains_key("ChatMessage"),
            "a Delete of an embedded `ChatMessage` withdraws through the chat handler"
        );
        for object_type in ["Note", "Question", "Article", "Page", "Tombstone"] {
            assert!(
                handlers.delete_by_object_type.contains_key(object_type),
                "a Delete of an embedded `{object_type}` withdraws through the note handler"
            );
        }
        assert_eq!(
            handlers.delete_by_object_type.len(),
            6,
            "the chat entry plus the note family's deletable five"
        );
        assert!(
            handlers.delete_fallback.is_some(),
            "the Delete fallback is registered: it is what tells a peer deleting itself from one deleting a post"
        );
        for object_type in crate::federation::note_parser::OBJECT_TYPES {
            assert!(
                handlers.create_by_object_type.contains_key(object_type),
                "a Create of a {object_type} folds through the note-ingest handler"
            );
        }
        assert!(
            handlers.create_by_object_type.contains_key("ChatMessage"),
            "and a Create of a `ChatMessage` folds through the chat handler"
        );
        assert_eq!(
            handlers.create_by_object_type.len(),
            crate::federation::note_parser::OBJECT_TYPES.len() + 1,
            "the note family plus the chat ticket's key: later tickets (polls) add their own"
        );
        assert!(
            handlers.pre_checks_create("Note"),
            "a Create of a `Note` is put to the poll ticket's guard first: an answer is a vote, not a post"
        );
        assert!(
            !handlers.pre_checks_create("Question"),
            "and the guard covers the note shape alone: a `Question` is a poll, folded by the note handler"
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

    /// Normalization of the cardinalities: every field through its string,
    /// object and array form.
    #[test]
    fn normalization_reads_every_field_shape() {
        let view = ActivityView::parse(&json!({
            "id": "https://peer.example/activities/1",
            "type": ["Create", "Service"],
            "actor": [{"id": "https://peer.example/users/alice", "type": "Person"}],
            "object": {"id": "https://peer.example/posts/1", "type": "Note"},
            "target": {"id": "https://peer.example/collections/featured"},
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [{"type": "Collection", "id": "https://peer.example/users/alice/followers"},
                    "https://peer.example/users/bob"],
        }))
        .expect("the fixture parses");
        assert_eq!(view.kind, "Create", "the first string of the array type");
        assert_eq!(
            view.actor, "https://peer.example/users/alice",
            "an actor object inside an array reads as its id"
        );
        assert!(
            matches!(view.object, ObjectRef::Embedded(_)),
            "an embedded object stays a document"
        );
        assert_eq!(
            view.object.id(),
            Some("https://peer.example/posts/1"),
            "and the id it carries reads off it"
        );
        assert_eq!(
            view.target.as_deref(),
            Some("https://peer.example/collections/featured"),
            "an object target reads as its id"
        );
        assert_eq!(view.to.len(), 1, "the one-element addressing array");
        assert_eq!(
            view.cc,
            vec![
                "https://peer.example/users/alice/followers".to_owned(),
                "https://peer.example/users/bob".to_owned()
            ],
            "a mixed cc array normalizes element by element"
        );

        let referenced = ActivityView::parse(&json!({
            "id": "https://peer.example/activities/2",
            "type": "Announce",
            "actor": ["https://peer.example/users/alice"],
            "object": ["https://peer.example/posts/1"],
        }))
        .expect("the announce parses");
        assert!(
            matches!(&referenced.object, ObjectRef::Id(id)
                if id == "https://peer.example/posts/1"),
            "an array of references reads as the head reference"
        );
        assert!(referenced.target.is_none(), "no target, no reference");
        assert!(
            referenced.to.is_empty() && referenced.cc.is_empty(),
            "addressing fields that are absent normalize to empty"
        );
    }

    /// An activity the dispatcher cannot route — no id, no type, no
    /// actor, no object — does not parse.
    #[test]
    fn normalization_requires_the_four_routing_fields() {
        let complete = json!({
            "id": "https://peer.example/activities/3",
            "type": "Follow",
            "actor": "https://peer.example/users/alice",
            "object": "https://test.example/users/bob",
        });
        assert!(ActivityView::parse(&complete).is_some(), "the base case");
        for field in ["id", "type", "actor", "object"] {
            let mut missing = complete.as_object().expect("an object").clone();
            let _gone: Option<Value> = missing.remove(field);
            assert!(
                ActivityView::parse(&Value::Object(missing)).is_none(),
                "no view without {field}"
            );
        }
        assert!(
            ActivityView::parse(&json!({
                "id": "https://peer.example/activities/4",
                "type": "Follow",
                "actor": "https://peer.example/users/alice",
                "object": 7,
            }))
            .is_none(),
            "an object that is neither a document nor a reference cannot route"
        );
        let idless = ActivityView::parse(&json!({
            "id": "https://peer.example/activities/5",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": {"type": "Note"},
        }))
        .expect("an embedded object without an id still parses");
        assert!(
            idless.object.id().is_none(),
            "and says so through ObjectRef::id"
        );
    }

    /// The ledger is the dedupe: the second delivery of the same id
    /// completes without reaching the handler.
    #[tokio::test]
    async fn a_redelivered_activity_is_not_processed_twice() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = signer(&pool).await;
        let activity = follow_activity("dedupe-1");
        let args = incoming_args(&activity, &alice);
        let handlers = test_handlers();

        let first =
            dispatch_activity(&state, args.clone(), &handlers, mrf::filter_incoming_boxed).await;
        let second = dispatch_activity(&state, args, &handlers, mrf::filter_incoming_boxed).await;

        assert_eq!(first, Ok(()), "the first delivery completes");
        assert_eq!(second, Ok(()), "the redelivery completes too");
        assert_eq!(
            seen("ok", "https://peer.example/activities/dedupe-1"),
            1,
            "and the handler ran exactly once"
        );
        assert_eq!(
            ledger_rows(&pool, "https://peer.example/activities/dedupe-1").await,
            1,
            "one ledger row per activity id"
        );
    }

    /// A handler that fails transiently asks for a retry — and the ledger
    /// row is deleted again so the retry is not swallowed by the dedupe.
    #[tokio::test]
    async fn a_transient_failure_releases_the_ledger_row_for_the_retry() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = signer(&pool).await;
        let activity = json!({
            "id": "https://peer.example/activities/retry-1",
            "type": "Like",
            "actor": "https://peer.example/users/alice",
            "object": "https://test.example/objects/post-1",
        });
        let args = incoming_args(&activity, &alice);
        let handlers = test_handlers();

        let first =
            dispatch_activity(&state, args.clone(), &handlers, mrf::filter_incoming_boxed).await;
        let second = dispatch_activity(&state, args, &handlers, mrf::filter_incoming_boxed).await;

        assert!(
            matches!(&first, Err(JobError::Retry(detail)) if detail == "injected"),
            "the transient failure is a retry: {first:?}"
        );
        assert!(
            matches!(&second, Err(JobError::Retry(_))),
            "the retry found the door open again: {second:?}"
        );
        assert_eq!(
            seen("transient", "https://peer.example/activities/retry-1"),
            2,
            "the handler ran on both attempts"
        );
        assert_eq!(
            ledger_rows(&pool, "https://peer.example/activities/retry-1").await,
            0,
            "and after the last failure no row blocks the next attempt"
        );
    }

    /// A permanent failure discards the job and keeps the ledger row: a
    /// redelivery of a refused activity must not run it again.
    #[tokio::test]
    async fn a_permanent_failure_discards_and_keeps_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = signer(&pool).await;
        let activity = json!({
            "id": "https://peer.example/activities/discard-1",
            "type": "Block",
            "actor": "https://peer.example/users/alice",
            "object": "https://test.example/users/carol",
        });

        let outcome = dispatch_activity(
            &state,
            incoming_args(&activity, &alice),
            &test_handlers(),
            mrf::filter_incoming_boxed,
        )
        .await;

        assert!(
            matches!(&outcome, Err(JobError::Discard(detail)) if detail == "injected"),
            "the permanent failure is a discard: {outcome:?}"
        );
        assert_eq!(
            seen("permanent", "https://peer.example/activities/discard-1"),
            1,
            "the handler ran once, then nothing more"
        );
        assert_eq!(
            ledger_rows(&pool, "https://peer.example/activities/discard-1").await,
            1,
            "the refused activity stays claimed"
        );
    }

    /// Containment, first arm: an activity whose actor is not the signer
    /// is discarded before anything is written.
    #[tokio::test]
    async fn an_actor_that_is_not_the_signer_is_discarded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let activity = json!({
            "id": "https://peer.example/activities/contain-1",
            "type": "Follow",
            "actor": "https://peer.example/users/mallory",
            "object": "https://test.example/users/bob",
        });

        let outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&activity, &alice),
            &test_handlers(),
            mrf::filter_incoming_boxed,
        )
        .await;

        assert_eq!(outcome, Ok(()), "the job completes, silently");
        assert_eq!(
            seen("ok", "https://peer.example/activities/contain-1"),
            0,
            "nothing was routed"
        );
        assert_eq!(
            ledger_rows(&pool, "https://peer.example/activities/contain-1").await,
            0,
            "a discard leaves no ledger row"
        );
    }

    /// Containment, the host arms: an activity id on another host is
    /// dropped, a foreign embedded object is dropped, and the same
    /// foreign object under an exception-listed type passes.
    #[tokio::test]
    async fn ids_from_foreign_hosts_are_discarded_except_reference_objects() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = signer(&pool).await;
        let handlers = test_handlers();

        let wrong_id = json!({
            "id": "https://evil.example/activities/contain-2",
            "type": "Follow",
            "actor": "https://peer.example/users/alice",
            "object": "https://test.example/users/bob",
        });
        let dropped_id = dispatch_activity(
            &state,
            incoming_args(&wrong_id, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(dropped_id, Ok(()), "a mismatched activity id is dropped");

        let foreign_create = json!({
            "id": "https://peer.example/activities/contain-3",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://evil.example/posts/stolen",
                "type": "Note",
                "content": "not alice's",
            },
        });
        let dropped_create = dispatch_activity(
            &state,
            incoming_args(&foreign_create, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(
            dropped_create,
            Ok(()),
            "a Create of a foreign embedded object is dropped"
        );

        let foreign_announce = json!({
            "id": "https://peer.example/activities/contain-4",
            "type": "Announce",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://other.example/posts/1",
                "type": "Note",
                "content": "someone else's post, announced",
            },
        });
        let announced = dispatch_activity(
            &state,
            incoming_args(&foreign_announce, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(announced, Ok(()), "an Announce of a foreign object passes");
        assert_eq!(
            seen("ok", "https://peer.example/activities/contain-4"),
            1,
            "the exception list is what let it through"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/contain-3"),
            0,
            "and the same object under Create did not"
        );
    }

    /// An unknown type reaches no handler and costs nothing: the job
    /// completes and the ledger marks the id seen.
    #[tokio::test]
    async fn unknown_types_are_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let activity = json!({
            "id": "https://peer.example/activities/unknown-1",
            "type": "Offer",
            "actor": "https://peer.example/users/alice",
            "object": "https://test.example/users/bob",
        });

        let outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&activity, &alice),
            &Handlers::new(),
            mrf::filter_incoming_boxed,
        )
        .await;

        assert_eq!(outcome, Ok(()), "an unroutable activity completes the job");
        assert_eq!(
            seen("ok", "https://peer.example/activities/unknown-1"),
            0,
            "no handler ran"
        );
        assert_eq!(
            ledger_rows(&pool, "https://peer.example/activities/unknown-1").await,
            1,
            "and the id is spent: it will not be re-ignored forever"
        );
    }

    /// Delivers `activity` authenticated by `who` through `handlers`, the
    /// way the `federator.incoming` job does, and answers the job's outcome.
    async fn deliver(
        state: &AppState,
        who: &User,
        activity: &Value,
        handlers: &Handlers,
    ) -> Result<(), JobError> {
        dispatch_activity(
            state,
            incoming_args(activity, who),
            handlers,
            mrf::filter_incoming_boxed,
        )
        .await
    }

    /// The `Update` arm of the dispatch: the type of the embedded object
    /// decides the route, and the activity's own type table is never
    /// consulted for it.
    #[tokio::test]
    async fn an_update_routes_by_its_objects_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let mut handlers = Handlers::new();
        handlers.register("Update", ok_handler);
        handlers.register_update("Person", update_person_handler);
        let activity = json!({
            "id": "https://peer.example/activities/update-person",
            "type": "Update",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/users/alice",
                "type": "Person",
                "name": "Alice again",
            },
        });

        let folded = deliver(&app_state(&pool), &alice, &activity, &handlers).await;

        assert_eq!(folded, Ok(()), "the actor update completes");
        assert_eq!(
            seen(
                "update-person",
                "https://peer.example/activities/update-person"
            ),
            1,
            "the object's own type decided the route"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/update-person"),
            0,
            "the activity type table was not consulted for an Update"
        );
    }

    /// An `Update` of an object type no ticket has registered a route for —
    /// or of an object that is only a reference and so declares no type —
    /// reaches nobody and completes the job.
    #[tokio::test]
    async fn an_update_of_an_unclaimed_object_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let handlers = Handlers::new();
        let note_update = json!({
            "id": "https://peer.example/activities/update-note",
            "type": "Update",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/posts/1",
                "type": "Note",
                "content": "edited, but nobody here folds notes yet",
            },
        });
        let bare_update = json!({
            "id": "https://peer.example/activities/update-bare",
            "type": "Update",
            "actor": "https://peer.example/users/alice",
            "object": "https://peer.example/users/alice",
        });
        let state = app_state(&pool);

        let unfolded = deliver(&state, &alice, &note_update, &handlers).await;
        let unreferenced = deliver(&state, &alice, &bare_update, &handlers).await;

        assert_eq!(
            unfolded,
            Ok(()),
            "an unregistered object type completes as an ignore"
        );
        assert_eq!(
            unreferenced,
            Ok(()),
            "and so does an object that declares no type"
        );
        assert_eq!(
            seen(
                "update-person",
                "https://peer.example/activities/update-note"
            ) + seen("ok", "https://peer.example/activities/update-note")
                + seen(
                    "update-person",
                    "https://peer.example/activities/update-bare"
                )
                + seen("ok", "https://peer.example/activities/update-bare"),
            0,
            "neither delivery reached a handler"
        );
    }

    /// The `Delete` arm: an embedded document whose type the sub-registry
    /// claims goes to that claimant; one it does not claim — an unclaimed
    /// type or a bare id — falls to the route that resolves the id.
    #[tokio::test]
    async fn a_delete_routes_by_its_objects_type_or_through_the_fallback() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let mut handlers = Handlers::new();
        handlers.register_delete("Tombstone", delete_tombstone_handler);
        handlers.register_delete_fallback(delete_fallback_handler);
        let claimed_type = json!({
            "id": "https://peer.example/activities/delete-tombstone",
            "type": "Delete",
            "actor": "https://peer.example/users/alice",
            "object": {"type": "Tombstone", "id": "https://peer.example/posts/1"},
        });
        let unclaimed_type = json!({
            "id": "https://peer.example/activities/delete-article",
            "type": "Delete",
            "actor": "https://peer.example/users/alice",
            "object": {"type": "Article", "id": "https://peer.example/articles/1"},
        });
        let bare = json!({
            "id": "https://peer.example/activities/delete-bare",
            "type": "Delete",
            "actor": "https://peer.example/users/alice",
            "object": "https://peer.example/users/alice",
        });
        let state = app_state(&pool);

        let claimed = deliver(&state, &alice, &claimed_type, &handlers).await;
        let other = deliver(&state, &alice, &unclaimed_type, &handlers).await;
        let resolved = deliver(&state, &alice, &bare, &handlers).await;

        assert_eq!(claimed, Ok(()), "the claimed type's route ran");
        assert_eq!(other, Ok(()), "an unclaimed type still falls back");
        assert_eq!(resolved, Ok(()), "and so does a bare id");
        assert_eq!(
            seen(
                "delete-tombstone",
                "https://peer.example/activities/delete-tombstone"
            ),
            1,
            "the embedded document went to its type's entry"
        );
        assert_eq!(
            seen(
                "delete-fallback",
                "https://peer.example/activities/delete-bare"
            ) + seen(
                "delete-fallback",
                "https://peer.example/activities/delete-article"
            ),
            2,
            "the id and the unclaimed type both reached the fallback"
        );
        assert_eq!(
            seen(
                "delete-tombstone",
                "https://peer.example/activities/delete-bare"
            ) + seen(
                "delete-tombstone",
                "https://peer.example/activities/delete-article"
            ),
            0,
            "and neither reached the wrong entry"
        );
    }

    /// A `Delete` arriving at a registry with no route for it — neither a
    /// type claim nor a fallback — is ignored, not failed.
    #[tokio::test]
    async fn a_delete_without_any_route_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let activity = json!({
            "id": "https://peer.example/activities/delete-no-routes",
            "type": "Delete",
            "actor": "https://peer.example/users/alice",
            "object": "https://peer.example/users/alice",
        });

        let outcome = deliver(&app_state(&pool), &alice, &activity, &Handlers::new()).await;

        assert_eq!(outcome, Ok(()), "an unroutable Delete completes the job");
        assert_eq!(
            seen(
                "delete-fallback",
                "https://peer.example/activities/delete-no-routes"
            ),
            0,
            "and reached no handler"
        );
    }

    /// The `Create` arm of the dispatch: the embedded object's own type
    /// decides the route, a bare reference and an unclaimed type reach
    /// nobody, and the activity type table is never consulted for a
    /// `Create` either.
    #[tokio::test]
    async fn a_create_routes_by_its_objects_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let mut handlers = Handlers::new();
        handlers.register("Create", ok_handler);
        handlers.register_create("Note", ok_handler);
        let embedded_note = json!({
            "id": "https://peer.example/activities/create-note",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/posts/cn",
                "type": "Note",
                "content": "a note to fold",
            },
        });
        let bare = json!({
            "id": "https://peer.example/activities/create-bare",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": "https://peer.example/posts/cb",
        });
        let unclaimed = json!({
            "id": "https://peer.example/activities/create-chat",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/posts/cc",
                "type": "ChatMessage",
                "content": "no chat ticket has claimed this yet",
            },
        });
        let state = app_state(&pool);

        let folded = deliver(&state, &alice, &embedded_note, &handlers).await;
        let referenced = deliver(&state, &alice, &bare, &handlers).await;
        let unknown = deliver(&state, &alice, &unclaimed, &handlers).await;

        assert_eq!(folded, Ok(()), "the registered object type ran its handler");
        assert_eq!(
            referenced,
            Ok(()),
            "a bare reference completes as an ignore"
        );
        assert_eq!(unknown, Ok(()), "and so does an unclaimed object type");
        assert_eq!(
            seen("ok", "https://peer.example/activities/create-note"),
            1,
            "the object's own type decided the route"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/create-bare")
                + seen("ok", "https://peer.example/activities/create-chat"),
            0,
            "neither the reference nor the unclaimed type reached a handler"
        );
    }

    /// The reference arm of the `Create` dispatch once a ticket registers
    /// it: a bare-id object reaches the `create_by_reference` handler.
    #[tokio::test]
    async fn a_create_of_a_bare_reference_reaches_the_registered_reference_handler() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let mut handlers = Handlers::new();
        handlers.register_create_reference(ok_handler);
        let activity = json!({
            "id": "https://peer.example/activities/create-ref",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": "https://peer.example/posts/ref",
        });
        let state = app_state(&pool);

        let delivered = deliver(&state, &alice, &activity, &handlers).await;

        assert_eq!(
            delivered,
            Ok(()),
            "the reference handler completing ends the job"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/create-ref"),
            1,
            "the bare-id Create reached the reference route"
        );
    }

    /// A `Create` pre-check runs ahead of its object type's entry: a document
    /// the guard claims leaves through the guard's own handler, and one it
    /// passes over folds through the table exactly as it did before the guard
    /// existed. That is the shape of the poll vote — a `Note` that is not a
    /// post — and the fall-through is what keeps ordinary notes alone.
    #[tokio::test]
    async fn a_create_pre_check_runs_ahead_of_its_object_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let mut handlers = Handlers::new();
        handlers.register_create("Note", ok_handler);
        handlers.register_create_prechecked("Note", claims_named, prechecked_handler);
        let named = json!({
            "id": "https://peer.example/activities/guarded-note",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/posts/guarded",
                "type": "Note",
                "name": "the claimed option",
            },
        });
        let plain = json!({
            "id": "https://peer.example/activities/unguarded-note",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/posts/unguarded",
                "type": "Note",
                "content": "an ordinary post",
            },
        });
        let state = app_state(&pool);

        let claimed = deliver(&state, &alice, &named, &handlers).await;
        let passed = deliver(&state, &alice, &plain, &handlers).await;

        assert_eq!(claimed, Ok(()), "the claimed document completes");
        assert_eq!(passed, Ok(()), "and so does the passed-over one");
        assert_eq!(
            seen("prechecked", "https://peer.example/activities/guarded-note"),
            1,
            "the guard's handler folded the document it claimed"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/guarded-note"),
            0,
            "and the type's own entry never saw it"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/unguarded-note"),
            1,
            "the document the guard passed over reached the table"
        );
        assert_eq!(
            seen(
                "prechecked",
                "https://peer.example/activities/unguarded-note"
            ),
            0,
            "and not the guarded handler"
        );
    }

    /// The MRF reject path: a rejected activity is dropped, with the
    /// handler never reached.
    #[tokio::test]
    async fn a_rejected_activity_is_dropped_before_the_handler() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let activity = follow_activity("mrf-1");

        let outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&activity, &alice),
            &test_handlers(),
            reject_mrf,
        )
        .await;

        assert_eq!(outcome, Ok(()), "a rejection completes the job");
        assert_eq!(
            seen("ok", "https://peer.example/activities/mrf-1"),
            0,
            "the handler never saw it"
        );
        assert_eq!(
            ledger_rows(&pool, "https://peer.example/activities/mrf-1").await,
            1,
            "the dropped activity is still marked seen"
        );
    }

    /// The instance-policy arm: an actor host on the `reject` list gets
    /// its activities dropped.
    #[tokio::test]
    async fn instance_policy_rejects_the_actors_domain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let _policy = sqlx::query(
            "INSERT INTO instance_policy_domains (domain, action, reason) \
             VALUES ($1, 'reject', 'test')",
        )
        .bind("peer.example")
        .execute(&pool)
        .await
        .expect("the policy row inserts");
        let activity = follow_activity("policy-1");

        let outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&activity, &alice),
            &test_handlers(),
            mrf::filter_incoming_boxed,
        )
        .await;

        assert_eq!(outcome, Ok(()), "the rejection completes the job");
        assert_eq!(
            seen("ok", "https://peer.example/activities/policy-1"),
            0,
            "the handler never saw it"
        );
    }

    /// `Undo` routing, first arm: the embedded inner object states its
    /// own type, and the matching `Undo` handler runs.
    #[tokio::test]
    async fn an_undo_routes_by_its_embedded_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let activity = json!({
            "id": "https://peer.example/activities/undo-1",
            "type": "Undo",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/activities/follow-1",
                "type": "Follow",
                "actor": "https://peer.example/users/alice",
                "object": "https://test.example/users/bob",
            },
        });

        let outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&activity, &alice),
            &test_handlers(),
            mrf::filter_incoming_boxed,
        )
        .await;

        assert_eq!(outcome, Ok(()), "the undo completes");
        assert_eq!(
            seen("undo-follow", "https://peer.example/activities/undo-1"),
            1,
            "and routed through the Follow undo handler"
        );
    }

    /// `Undo` routing, second arm: an object that is only an id learns
    /// its type from the stored rows — a follow by its `ap_id`, a reblog
    /// by its `Announce` uri — and each routes to its own handler.
    #[tokio::test]
    async fn an_undo_resolves_a_bare_id_through_the_stored_activities() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let bob = local_user(&pool, "bob").await;
        let _followed = sqlx::query(
            "INSERT INTO follows (id, follower_id, followee_id, state, ap_id)
             VALUES ($1, $2, $3, 'accepted', 'https://peer.example/activities/follow-stored')",
        )
        .bind(Uuid::now_v7())
        .bind(alice.id)
        .bind(bob.id)
        .execute(&pool)
        .await
        .expect("the follow fixture inserts");
        let original_id = Uuid::now_v7();
        let announce_id = Uuid::now_v7();
        for (id, uri, reblog_of) in [
            (original_id, "https://test.example/objects/post-9", None),
            (
                announce_id,
                "https://peer.example/activities/announce-stored",
                Some(original_id),
            ),
        ] {
            let _inserted = sqlx::query(
                "INSERT INTO statuses (id, uri, user_id, conversation_id, local, visibility, reblog_of_id)
                 VALUES ($1, $2, $3, $1, false, 'public', $4)",
            )
            .bind(id)
            .bind(uri)
            .bind(alice.id)
            .bind(reblog_of)
            .execute(&pool)
            .await
            .expect("the status fixtures insert");
        }
        let handlers = test_handlers();

        let undo_follow = undo_activity("undo-2", "https://peer.example/activities/follow-stored");
        let follow_outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&undo_follow, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(follow_outcome, Ok(()), "the follow undo completes");

        let undo_announce =
            undo_activity("undo-3", "https://peer.example/activities/announce-stored");
        let announce_outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&undo_announce, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(announce_outcome, Ok(()), "the announce undo completes");

        assert_eq!(
            seen("undo-follow", "https://peer.example/activities/undo-2"),
            1,
            "the stored follow routed by ap_id"
        );
        assert_eq!(
            seen("undo-announce", "https://peer.example/activities/undo-3"),
            1,
            "the stored reblog routed by its Announce uri"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/undo-2")
                + seen("ok", "https://peer.example/activities/undo-3"),
            0,
            "nothing routed as a plain activity"
        );
    }

    /// An `Undo` stored under someone else's actor, and an `Undo` of an
    /// id nothing stores, both route to nobody.
    #[tokio::test]
    async fn undos_of_foreign_and_unknown_ids_are_dropped() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let bob = local_user(&pool, "bob").await;
        let _followed = sqlx::query(
            "INSERT INTO follows (id, follower_id, followee_id, state, ap_id)
             VALUES ($1, $2, $3, 'accepted', 'https://peer.example/activities/bobs-follow')",
        )
        .bind(Uuid::now_v7())
        .bind(bob.id)
        .bind(alice.id)
        .execute(&pool)
        .await
        .expect("the follow fixture inserts");
        let state = app_state(&pool);
        let handlers = test_handlers();

        // bob's follow, undone by alice: the actors differ, so it stops.
        let foreign = undo_activity("undo-6", "https://peer.example/activities/bobs-follow");
        let foreign_outcome = dispatch_activity(
            &state,
            incoming_args(&foreign, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(
            foreign_outcome,
            Ok(()),
            "the actor mismatch completes the job"
        );

        let unknown = undo_activity("undo-7", "https://peer.example/activities/never-stored");
        let unknown_outcome = dispatch_activity(
            &state,
            incoming_args(&unknown, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(unknown_outcome, Ok(()), "and so does the unknown id");

        assert_eq!(
            seen("undo-follow", "https://peer.example/activities/undo-6")
                + seen("undo-follow", "https://peer.example/activities/undo-7"),
            0,
            "neither undo reached a handler"
        );
    }

    /// Arguments no inbox route builds are discarded, and an activity
    /// that cannot normalize is dropped — before any database touch.
    #[tokio::test]
    async fn unusable_args_discard_and_unparseable_activities_drop() {
        let state = broken_state().await;
        let handlers = Handlers::new();

        let missing_activity =
            dispatch_activity(&state, json!({}), &handlers, mrf::filter_incoming_boxed).await;
        assert!(
            matches!(missing_activity, Err(JobError::Discard(_))),
            "no activity at all is a discard: {missing_activity:?}"
        );

        let bad_signer = dispatch_activity(
            &state,
            json!({"activity": {"id": "x"}, "signer_id": "not-a-uuid"}),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert!(
            matches!(bad_signer, Err(JobError::Discard(_))),
            "a signer id that is no uuid is a discard: {bad_signer:?}"
        );

        let unparseable = dispatch_activity(
            &state,
            args_with(
                &json!({"id": "https://peer.example/activities/5", "type": "Follow"}),
                Uuid::now_v7(),
            ),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(
            unparseable,
            Ok(()),
            "an activity without an actor or object never reaches the ledger"
        );
    }

    /// A signer whose row is gone discards the job: the account being
    /// deleted after the delivery was queued is not something a retry
    /// fixes.
    #[tokio::test]
    async fn a_deleted_signer_discards() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let _gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture account deletes");
        let activity = follow_activity("gone-1");

        let outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&activity, &alice),
            &test_handlers(),
            mrf::filter_incoming_boxed,
        )
        .await;

        assert!(
            matches!(outcome, Err(JobError::Discard(_))),
            "the gone signer is a discard: {outcome:?}"
        );
    }

    /// A signer the database cannot answer retries: the delivery is still
    /// queued, and the ledger holds nothing that could swallow the retry.
    #[tokio::test]
    async fn an_unreachable_database_retries() {
        let state = broken_state().await;
        let activity = follow_activity("retry-db-1");

        let outcome = dispatch_activity(
            &state,
            args_with(&activity, Uuid::now_v7()),
            &Handlers::new(),
            mrf::filter_incoming_boxed,
        )
        .await;

        assert!(
            matches!(outcome, Err(JobError::Retry(_))),
            "a dead database asks for another attempt: {outcome:?}"
        );
    }

    /// The prune: rows past two weeks go, younger rows stay, a dead
    /// database retries.
    #[tokio::test]
    async fn the_prune_deletes_only_the_old_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        for (ap_id, age_days) in [
            ("https://peer.example/activities/old", 15_i32),
            ("https://peer.example/activities/fresh", 13_i32),
        ] {
            let _inserted = sqlx::query(
                "INSERT INTO received_activities (ap_id, inserted_at)
                 VALUES ($1, now() - make_interval(days => $2))",
            )
            .bind(ap_id)
            .bind(age_days)
            .execute(&pool)
            .await
            .expect("the ledger fixtures insert");
        }

        handle_cleanup_received(app_state(&pool), FIRST_ATTEMPT, json!({}))
            .await
            .expect("the prune runs");

        assert_eq!(
            ledger_rows(&pool, "https://peer.example/activities/old").await,
            0,
            "the 15-day row is gone"
        );
        assert_eq!(
            ledger_rows(&pool, "https://peer.example/activities/fresh").await,
            1,
            "the 13-day row stays"
        );

        let outcome = handle_cleanup_received(broken_state().await, FIRST_ATTEMPT, json!({})).await;
        assert!(
            matches!(outcome, Err(JobError::Retry(_))),
            "a prune against a dead database retries: {outcome:?}"
        );
    }

    /// The registered job handler drives the same pipeline
    /// [`handle_incoming`] does: over the live registry a real `Follow` — a
    /// type no handler module has claimed — runs containment, claims its
    /// ledger row, and completes as an unknown-type ignore.
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
