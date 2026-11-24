//! The receiving half of the Postgres bridge: the `LISTEN` session that
//! re-broadcasts other instances' events, and the sweep of expired payload
//! references.

use std::future::Future;

use chrono::{TimeDelta, Utc};
use serde_json::Value;
use sqlx::{PgPool, postgres::PgListener};
use tokio::time::sleep;
use tracing::{debug, warn};

use super::{Hub, NOTIFY_CHANNEL, PAYLOAD_TTL, RECONNECT_DELAY, bridge::BridgeError};
use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    state::AppState,
};

/// Deletes the reference rows older than [`PAYLOAD_TTL`], returning how
/// many went. [`KIND_CLEANUP`](super::KIND_CLEANUP)'s whole job, run by
/// [`handle_cleanup`].
async fn cleanup_payloads(db: &PgPool) -> Result<u64, BridgeError> {
    let ttl = i64::try_from(PAYLOAD_TTL.as_secs()).unwrap_or(i64::MAX);
    let cutoff = Utc::now() - TimeDelta::try_seconds(ttl).unwrap_or_default();
    let result = sqlx::query("DELETE FROM stream_event_payloads WHERE inserted_at < $1")
        .bind(cutoff)
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for
/// [`KIND_CLEANUP`](super::KIND_CLEANUP): one sweep of expired reference
/// payloads.
///
/// A failed sweep is a [`JobError::Retry`], and the scheduler enqueues the
/// next period regardless, so a missed pass costs a minute of reference
/// rows, not a lost event.
pub fn handle_cleanup(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        match cleanup_payloads(&state.db).await {
            Ok(count) => {
                if count > 0 {
                    debug!(
                        count,
                        "streaming cleanup removed expired reference payloads"
                    );
                }
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!(
                "streaming reference-table sweep failed: {err}"
            ))),
        }
    })
}

/// How one [`listen_session`] ended.
#[derive(Debug)]
enum SessionOutcome {
    /// The shutdown future resolved: the process is stopping.
    Shutdown,
    /// Connecting, `LISTEN`ing or receiving failed, or the connection was
    /// lost; the string is why, and [`run_pg_listener`] retries after
    /// [`RECONNECT_DELAY`].
    Disconnected(String),
}

/// One `LISTEN wired_events` session over its own connection from `pool`:
/// each notification goes to [`Hub::ingest`] until it fails or
/// `shutdown` resolves.
async fn listen_session<F>(pool: &PgPool, hub: &Hub, mut shutdown: F) -> SessionOutcome
where
    F: Future<Output = ()> + Unpin,
{
    let mut listener = match PgListener::connect_with(pool).await {
        Ok(listener) => listener,
        Err(err) => return SessionOutcome::Disconnected(err.to_string()),
    };
    if let Err(err) = listener.listen(NOTIFY_CHANNEL).await {
        return SessionOutcome::Disconnected(err.to_string());
    }
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => return SessionOutcome::Shutdown,
            notification = listener.try_recv() => match notification {
                Ok(Some(note)) => hub.ingest(note.payload()).await,
                Ok(None) => {
                    return SessionOutcome::Disconnected(
                        "the listener connection was lost".to_owned(),
                    );
                }
                Err(err) => return SessionOutcome::Disconnected(err.to_string()),
            },
        }
    }
}

/// Mirrors every event the other instances publish onto `hub`'s local bus
/// until `shutdown` resolves.
///
/// The loop is a `PgListener` session on [`NOTIFY_CHANNEL`] that
/// reconnects one second after any session fails; events
/// tagged with this process' id are dropped, so this is the bridge's
/// receiving half, started from [`crate::serve`] beside the job worker.
///
/// Delivery here is at-most-once: a notification missed while every
/// connection was down is gone, which is the trade Postgres `NOTIFY`
/// makes for a streaming bus, and what Pleroma's `PubSub` does too.
pub async fn run_pg_listener<F>(pool: PgPool, hub: Hub, shutdown: F)
where
    F: Future<Output = ()>,
{
    tokio::pin!(shutdown);
    loop {
        match listen_session(&pool, &hub, &mut shutdown).await {
            SessionOutcome::Shutdown => break,
            SessionOutcome::Disconnected(reason) => {
                warn!(%reason, "streaming listener session ended; reconnecting");
                tokio::select! {
                    biased;
                    () = &mut shutdown => break,
                    () = sleep(RECONNECT_DELAY) => {}
                }
            }
        }
    }
    debug!("streaming pg listener stopped");
}

#[cfg(test)]
mod tests {
    use tokio::time::timeout;
    use uuid::Uuid;

    use super::*;
    use crate::streaming::hub::{
        MAX_NOTIFY_BYTES, RenderSpec,
        bridge::{Envelope, fetch_payload, store_payload},
        test_support::{
            BRIDGE_WAIT, assert_single_delivery, await_payload, lazy_pool, render_event, settle,
            spawn_listener, update_event, warm_up,
        },
    };

    /// Both new fields cross a live two-hub `NOTIFY`: an event published
    /// on hub A with a render spec and the local flag arrives complete on
    /// hub B's bus.
    #[tokio::test]
    async fn render_and_local_only_cross_the_live_bridge() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hub_a = Hub::new(&pool);
        let hub_b = Hub::new(&pool);
        let (stop_a, task_a) = spawn_listener(&pool, &hub_a);
        let (stop_b, task_b) = spawn_listener(&pool, &hub_b);
        let mut rx_a = hub_a.subscribe();
        let mut rx_b = hub_b.subscribe();
        warm_up(&hub_a, &mut rx_b, "\"warm-a\"").await;
        warm_up(&hub_b, &mut rx_a, "\"warm-b\"").await;
        settle(&mut rx_a).await;
        settle(&mut rx_b).await;

        let event = render_event(RenderSpec::Notification(Uuid::now_v7()), true);
        hub_a.publish(event.clone());

        let arrived = await_payload(&mut rx_b, event.payload.as_deref().unwrap()).await;
        assert_eq!(
            &*arrived, &event,
            "the far hub reassembles both new fields from the NOTIFY"
        );

        drop(stop_a);
        drop(stop_b);
        timeout(BRIDGE_WAIT, task_a)
            .await
            .expect("listener A stopped")
            .expect("listener A did not panic");
        timeout(BRIDGE_WAIT, task_b)
            .await
            .expect("listener B stopped")
            .expect("listener B did not panic");
    }

    #[tokio::test]
    async fn the_listener_loop_ends_with_its_shutdown() {
        let pool = lazy_pool();
        let hub = Hub::new(&pool);
        let (stop, task) = spawn_listener(&pool, &hub);

        drop(stop);

        timeout(BRIDGE_WAIT, task)
            .await
            .expect("the listener returned after its shutdown")
            .expect("the listener task did not panic");
    }

    #[tokio::test]
    async fn two_hubs_bridge_small_events_over_postgres() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hub_a = Hub::new(&pool);
        let hub_b = Hub::new(&pool);
        let (stop_a, task_a) = spawn_listener(&pool, &hub_a);
        let (stop_b, task_b) = spawn_listener(&pool, &hub_b);
        let mut rx_a = hub_a.subscribe();
        let mut rx_b = hub_b.subscribe();

        // Warm up both directions first, so the real assertions do not
        // race the listeners' initial connect-and-LISTEN: the barrier
        // republishes until a copy actually crosses (a `NOTIFY` that beats
        // its listener's `LISTEN` is lost) and then waits for the bus to go
        // quiet, so what follows is the only event in flight.
        warm_up(&hub_a, &mut rx_b, "\"warm-a\"").await;
        warm_up(&hub_b, &mut rx_a, "\"warm-b\"").await;
        settle(&mut rx_a).await;
        settle(&mut rx_b).await;

        hub_a.publish(update_event("\"final\""));

        let _final = await_payload(&mut rx_b, "\"final\"").await;
        assert_single_delivery(&mut rx_a, "\"final\"", "the origin").await;

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM stream_event_payloads")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0, "an inline event never touches the table");

        drop(stop_a);
        drop(stop_b);
        timeout(BRIDGE_WAIT, task_a)
            .await
            .expect("listener A stopped")
            .expect("listener A did not panic");
        timeout(BRIDGE_WAIT, task_b)
            .await
            .expect("listener B stopped")
            .expect("listener B did not panic");
    }

    #[tokio::test]
    async fn an_oversized_payload_crosses_the_bridge_by_reference() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hub_a = Hub::new(&pool);
        let hub_b = Hub::new(&pool);
        let (stop_a, task_a) = spawn_listener(&pool, &hub_a);
        let (stop_b, task_b) = spawn_listener(&pool, &hub_b);
        let mut rx_a = hub_a.subscribe();
        let mut rx_b = hub_b.subscribe();
        warm_up(&hub_a, &mut rx_b, "\"warm-a\"").await;
        warm_up(&hub_b, &mut rx_a, "\"warm-b\"").await;
        settle(&mut rx_a).await;
        settle(&mut rx_b).await;

        let big = format!(r#"{{"content":"{}"}}"#, "x".repeat(2 * MAX_NOTIFY_BYTES));
        hub_a.publish(update_event(&big));

        let _spilled = await_payload(&mut rx_b, &big).await;
        assert_single_delivery(&mut rx_a, &big, "the origin").await;

        let stored: Vec<String> = sqlx::query_scalar("SELECT payload FROM stream_event_payloads")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            stored.len(),
            1,
            "the oversized envelope spilled to exactly one reference row"
        );
        let envelope: Envelope = serde_json::from_str(stored.first().unwrap()).unwrap();
        assert_eq!(
            envelope.payload,
            Value::String(big),
            "and it holds the whole inline envelope, payload verbatim, for every listener"
        );

        drop(stop_a);
        drop(stop_b);
        timeout(BRIDGE_WAIT, task_a)
            .await
            .expect("listener A stopped")
            .expect("listener A did not panic");
        timeout(BRIDGE_WAIT, task_b)
            .await
            .expect("listener B stopped")
            .expect("listener B did not panic");
    }

    #[tokio::test]
    async fn cleanup_removes_only_expired_reference_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let fresh = Uuid::now_v7();
        let expired = Uuid::now_v7();
        store_payload(&pool, fresh, "fresh-body").await.unwrap();
        store_payload(&pool, expired, "expired-body").await.unwrap();
        let _result = sqlx::query(
            "UPDATE stream_event_payloads SET inserted_at = now() - make_interval(secs => 120) WHERE id = $1",
        )
        .bind(expired)
        .execute(&pool)
        .await
        .unwrap();

        let removed = cleanup_payloads(&pool).await.unwrap();

        assert_eq!(removed, 1, "exactly the row past its TTL went");
        let kept = fetch_payload(&pool, fresh).await.unwrap();
        assert_eq!(
            kept.as_deref(),
            Some("fresh-body"),
            "a row still inside the TTL keeps serving listeners"
        );
        assert!(
            fetch_payload(&pool, expired).await.unwrap().is_none(),
            "the expired row is gone"
        );
    }
}
