//! The received-activity dedupe ledger: the claim a delivery makes on its
//! activity id, the release a retrying delivery gives back, and the prune of
//! rows past [`RECEIVED_MAX_AGE`].

use chrono::TimeDelta;
use sqlx::PgPool;
use tracing::{debug, warn};

use crate::{jobs::JobError, state::AppState};

/// How long a `received_activities` row lives: 14 days. A peer that
/// redelivers the same activity after that horizon gets it processed
/// again, which is the point — a two-week-old activity is long past any
/// inbox retry chain, and the ledger only has to be exact while one can
/// still be running.
const RECEIVED_MAX_AGE: TimeDelta = TimeDelta::days(14);

/// The ledger claim: one row per activity id, and a lost claim is the
/// `rows_affected = 0` that says "already seen".
const INSERT_RECEIVED_SQL: &str =
    "INSERT INTO received_activities (ap_id) VALUES ($1) ON CONFLICT DO NOTHING";
/// Releasing a claim so the next attempt of the same job can run the
/// activity again.
const DELETE_RECEIVED_SQL: &str = "DELETE FROM received_activities WHERE ap_id = $1";
/// The daily prune of the ledger rows past [`RECEIVED_MAX_AGE`].
const PRUNE_RECEIVED_SQL: &str = "DELETE FROM received_activities WHERE inserted_at < now() - $1";

/// Claims the ledger row: `true` when this is the first sight of the id.
/// The claim itself failing is the database being unreachable — the one
/// failure mode that must retry before anything is decided about the
/// activity.
pub(super) async fn claim_ledger_row(state: &AppState, key: &str) -> Result<bool, JobError> {
    let written = sqlx::query(INSERT_RECEIVED_SQL)
        .bind(key)
        .execute(&state.db)
        .await
        .map_err(|err| JobError::Retry(format!("the received-activity claim failed: {err}")))?;
    Ok(written.rows_affected() > 0)
}

/// Releases a claim whose processing will be retried, so the next attempt
/// finds the door open. A release that fails leaves the row to the daily
/// prune — one swallowed retry inside two weeks is the cost.
pub(super) async fn release_ledger_row(state: &AppState, ap_id: &str) {
    if let Err(err) = sqlx::query(DELETE_RECEIVED_SQL)
        .bind(ap_id)
        .execute(&state.db)
        .await
    {
        warn!(ap_id, %err, "could not release the ledger row of a retrying activity");
    }
}

/// One prune pass: the ledger rows older than [`RECEIVED_MAX_AGE`].
pub(super) async fn prune_ledger(db: &PgPool) -> Result<(), JobError> {
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
    use serde_json::json;

    use super::*;
    use crate::federation::{
        dispatch::{
            Handlers, dedupe_key, dispatch_activity, handle_cleanup_received,
            test_support::{
                FIRST_ATTEMPT, app_state, broken_state, follow_activity, incoming_args,
                ledger_rows, ok_handler, seen, signer, test_handlers,
            },
        },
        mrf,
    };

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

    /// A same-second `Update` under a reused activity id claims its own
    /// row when its object differs; an exact redelivery is still deduplicated.
    #[tokio::test]
    async fn updates_sharing_an_id_are_deduplicated_by_object() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = signer(&pool).await;
        let mut handlers = Handlers::new();
        handlers.register_update("Note", ok_handler);
        let update = |content: &str| {
            json!({
                "id": "https://peer.example/activities/same-second#updates/1",
                "type": "Update",
                "actor": "https://peer.example/users/alice",
                "object": {"id": "https://peer.example/objects/n1", "type": "Note", "content": content},
            })
        };

        for content in ["second", "third", "third"] {
            let outcome = dispatch_activity(
                &state,
                incoming_args(&update(content), &alice),
                &handlers,
                mrf::filter_incoming_boxed,
            )
            .await;
            assert_eq!(outcome, Ok(()), "{content} completes");
        }

        assert_eq!(
            seen(
                "ok",
                "https://peer.example/activities/same-second#updates/1"
            ),
            2,
            "the two distinct objects ran, the redelivery did not"
        );
        for content in ["second", "third"] {
            let key = dedupe_key(&update(content)).expect("the update has an id");
            assert_eq!(ledger_rows(&pool, &key).await, 1, "one row for {content}");
        }
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
}
