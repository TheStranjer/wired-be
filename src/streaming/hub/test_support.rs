//! Fixtures shared by the tests of the hub and its submodules.

use std::{sync::Arc, time::Duration};

use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::{
    sync::{broadcast, oneshot},
    time::{Instant, timeout},
};
use uuid::Uuid;

use super::{Audience, Hub, RenderSpec, StreamEvent, run_pg_listener};

/// Generous ceiling on one `NOTIFY` round trip in these tests.
pub(super) const BRIDGE_WAIT: Duration = Duration::from_secs(30);
/// How long a hub must stay quiet to prove it did not echo an event
/// back; the round trip the assertions wait on takes milliseconds.
const QUIET_WAIT: Duration = Duration::from_secs(2);
/// How long one warm-up round waits for its payload before publishing
/// the next one: a round trip takes milliseconds and a lost `NOTIFY`
/// never answers at all, so a short wait tells the two apart without
/// costing the test anything.
const WARM_UP_ROUND: Duration = Duration::from_millis(200);

/// A lazy, never-connected pool, for tests that never reach Postgres.
pub(super) fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy("postgres://localhost/unused")
        .unwrap()
}

/// A public `update` event carrying `payload` as its entity.
pub(super) fn update_event(payload: &str) -> StreamEvent {
    StreamEvent {
        streams: vec!["public".to_owned()],
        event: "update".to_owned(),
        payload: Some(payload.to_owned()),
        audience: Audience::Public,
        render: None,
        local_only: false,
    }
}

/// An event on exactly the streams in `streams`, with no payload.
pub(super) fn event_on(streams: &[&str]) -> StreamEvent {
    StreamEvent {
        streams: streams.iter().copied().map(String::from).collect(),
        event: "update".to_owned(),
        payload: None,
        audience: Audience::Public,
        render: None,
        local_only: false,
    }
}

/// A public `update` carrying a small entity plus `render` and
/// `local_only`.
pub(super) fn render_event(render: RenderSpec, local_only: bool) -> StreamEvent {
    StreamEvent {
        render: Some(render),
        local_only,
        ..update_event(r#"{"id":"7"}"#)
    }
}

/// The eight `(render, local_only)` shapes the hub must thread through
/// the bridge: every [`RenderSpec`] variant with the flag on and off.
pub(super) fn render_cases(id: Uuid) -> Vec<(RenderSpec, bool)> {
    let mut cases = Vec::new();
    for render in [
        RenderSpec::Status(id),
        RenderSpec::Notification(id),
        RenderSpec::Chat(id),
        RenderSpec::Conversation(id),
    ] {
        for local_only in [true, false] {
            cases.push((render.clone(), local_only));
        }
    }
    cases
}

/// The shutdown future [`run_pg_listener`] accepts: resolves once
/// `rx` is sent on (or dropped).
async fn stop_when(rx: oneshot::Receiver<()>) {
    let _signal = rx.await.is_ok();
}

/// Spawns a listener task for `hub` over `pool`, returning the handle
/// and its stopper.
pub(super) fn spawn_listener(
    pool: &PgPool,
    hub: &Hub,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let (stop, ready) = oneshot::channel();
    let task = tokio::spawn(run_pg_listener(pool.clone(), hub.clone(), stop_when(ready)));
    (stop, task)
}

/// Waits until `rx` yields an event whose payload is `want` and hands
/// that event back, failing the test past [`BRIDGE_WAIT`]; lagged
/// reports are skipped over.
pub(super) async fn await_payload(
    rx: &mut broadcast::Receiver<Arc<StreamEvent>>,
    want: &str,
) -> Arc<StreamEvent> {
    let deadline = Instant::now() + BRIDGE_WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "the wait for payload {want:?} ran out of time"
        );
        match timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) if event.payload.as_deref() == Some(want) => return event,
            Ok(Ok(_other)) => {}
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(closed)) => panic!("the bus closed while waiting: {closed:?}"),
            Err(elapsed) => panic!("payload {want:?} never arrived: {elapsed}"),
        }
    }
}

/// Bridges `payload` from `source` to `target`, republishing until one
/// copy crosses: the listener's first `LISTEN` races the test's first
/// publish, and a `NOTIFY` that reaches Postgres before its listener
/// subscribed is dropped on the floor (the bridge is at-most-once), so
/// one-shot warm-up publish is no barrier at all. When this returns the
/// `source` to `target` direction is live and the test's real
/// assertions may depend on it.
pub(super) async fn warm_up(
    source: &Hub,
    target: &mut broadcast::Receiver<Arc<StreamEvent>>,
    payload: &str,
) {
    let deadline = Instant::now() + BRIDGE_WAIT;
    loop {
        source.publish(update_event(payload));
        if arrived_within(target, payload, WARM_UP_ROUND).await {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the bridge never carried {payload:?} within {BRIDGE_WAIT:?}"
        );
    }
}

/// Whether `rx` yields the event whose payload is `payload` inside
/// `within`, skipping the other payloads and lag reports a retried
/// warm-up leaves in its way; `false` is the round simply having run out
/// of time.
async fn arrived_within(
    rx: &mut broadcast::Receiver<Arc<StreamEvent>>,
    payload: &str,
    within: Duration,
) -> bool {
    let deadline = Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) if event.payload.as_deref() == Some(payload) => return true,
            Ok(Ok(_other)) => {}
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(closed)) => panic!("the bus closed while warming up: {closed:?}"),
            Err(_elapsed) => return false,
        }
    }
}

/// Drains everything `rx` holds right now, without waiting.
fn drain(rx: &mut broadcast::Receiver<Arc<StreamEvent>>) {
    while rx.try_recv().is_ok() {}
}

/// Empties `rx` and then waits until it has stayed quiet for
/// [`QUIET_WAIT`], so nothing of the warm-up is still on its way. One
/// publish bridges on its own spawned task, so retried warm-up copies
/// can commit in any order and a late one would otherwise be the first
/// thing the real assertion reads.
pub(super) async fn settle(rx: &mut broadcast::Receiver<Arc<StreamEvent>>) {
    loop {
        drain(rx);
        match timeout(QUIET_WAIT, rx.recv()).await {
            Ok(Err(broadcast::error::RecvError::Closed)) => return,
            Ok(_delivered) => {}
            Err(_quiet) => return,
        }
    }
}

/// Asserts `rx` already holds `want` and then goes quiet: the hub
/// delivered its own event exactly once, not also via the bridge.
pub(super) async fn assert_single_delivery(
    rx: &mut broadcast::Receiver<Arc<StreamEvent>>,
    want: &str,
    who: &str,
) {
    let first = rx.recv().await.unwrap();
    assert_eq!(
        first.payload.as_deref(),
        Some(want),
        "{who} received its own event locally"
    );
    let echo = timeout(QUIET_WAIT, rx.recv()).await;
    assert!(
        echo.is_err(),
        "{who} received a second copy of an event it published itself"
    );
}
