//! The fixtures the dispatcher's unit tests share: the recording fake
//! handlers and the registry built from them, the test state over a live or
//! a dead pool, the signer and local-account rows, the delivery args and
//! activity builders, the ledger read and the `deliver` helper. Test-only.

use std::sync::Mutex;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

use super::{ActivityView, FederationError, Handlers, Outcome, dispatch_activity};
use crate::{
    config::Config,
    federation::mrf,
    jobs::{AttemptInfo, JobError},
    models::user::{Flag, NewUser, User, insert, local_ap_id},
    state::AppState,
};

/// Every `(label, activity id)` a fake handler ran for. Handlers are
/// `fn` pointers that cannot capture a counter, so the fakes record
/// here; tests count their own unique activity ids and never collide
/// with the suite's concurrent runs.
static SEEN: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Records a fake handler run.
pub(super) fn record(label: &str, view: &ActivityView) {
    let mut seen = SEEN.lock().expect("the test lock");
    seen.push((label.to_owned(), view.id.clone()));
}

/// How often the handler labelled `label` ran for the activity `ap_id`.
pub(super) fn seen(label: &str, ap_id: &str) -> usize {
    SEEN.lock()
        .expect("the test lock")
        .iter()
        .filter(|(seen_label, seen_id)| seen_label == label && seen_id == ap_id)
        .count()
}

/// Test state over `pool`.
pub(super) fn app_state(pool: &PgPool) -> AppState {
    AppState::for_tests(pool.clone(), Config::for_tests())
}

/// Test state over a pool that cannot answer: the database being
/// unreachable, as the handlers see it.
pub(super) async fn broken_state() -> AppState {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused")
        .expect("a lazy pool needs no database");
    pool.close().await;
    app_state(&pool)
}

/// The signer fixture: a mirror of `alice` on `peer.example`.
pub(super) async fn signer(pool: &PgPool) -> User {
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
pub(super) async fn local_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &local_ap_id("https://test.example", nickname)),
    )
    .await
    .expect("the local fixture inserts")
}

/// The args shape an inbox route writes.
pub(super) fn args_with(activity: &Value, signer_id: Uuid) -> Value {
    json!({"activity": activity, "signer_id": signer_id, "inbox_owner": null})
}

/// The args of a delivery authenticated by `who`.
pub(super) fn incoming_args(activity: &Value, who: &User) -> Value {
    args_with(activity, who.id)
}

/// A `Follow` of the local `bob` from `alice@peer.example`, with the
/// activity id `n`.
pub(super) fn follow_activity(n: &str) -> Value {
    json!({
        "id": format!("https://peer.example/activities/{n}"),
        "type": "Follow",
        "actor": "https://peer.example/users/alice",
        "object": "https://test.example/users/bob",
    })
}

/// An `Undo` by alice of the activity id `what`, named by bare id.
pub(super) fn undo_activity(n: &str, what: &str) -> Value {
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
pub(super) fn test_handlers() -> Handlers {
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
pub(super) fn ok_handler(
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

/// How many ledger rows carry `ap_id`.
pub(super) async fn ledger_rows(pool: &PgPool, ap_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM received_activities WHERE ap_id = $1")
        .bind(ap_id)
        .fetch_one(pool)
        .await
        .expect("the ledger reads")
}

/// The first attempt info, for the job-shaped handlers.
pub(super) const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: 5,
};

/// Delivers `activity` authenticated by `who` through `handlers`, the
/// way the `federator.incoming` job does, and answers the job's outcome.
pub(super) async fn deliver(
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
