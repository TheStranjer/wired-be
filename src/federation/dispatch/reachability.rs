//! The reachability bookkeeping of an authenticated delivery: a signed
//! request from the signer's host is the evidence that host is alive.

use tracing::warn;

use crate::{
    federation::json,
    models::{instance, user::User},
    state::AppState,
};

/// The reachability bookkeeping of an authenticated delivery: the signer's
/// host just got a signed request through, which is the evidence
/// [`crate::models::instance::set_reachable`] exists for, and the delivery
/// may be the host's first sight, which
/// [`crate::models::instance::touch`] records. Both writes are pure
/// bookkeeping: a failure is a lost ledger entry, logged and never allowed
/// to change the job's outcome. An `ap_id` that is not an `http(s)` url
/// names no host to record, and is skipped the same.
pub(super) async fn note_signer_host_reachable(state: &AppState, signer: &User) {
    let Some(host) = json::http_host(&signer.ap_id) else {
        return;
    };
    if let Err(err) = instance::touch(&state.db, &host).await {
        warn!(host = %host, %err, "could not record the signer's instance");
    }
    if let Err(err) = instance::set_reachable(&state.db, &host).await {
        warn!(host = %host, %err, "could not clear the signer instance's unreachable mark");
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta, Utc};
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        federation::dispatch::{
            Handlers,
            test_support::{app_state, deliver, follow_activity, seen, signer, test_handlers},
        },
        models::user::{Flag, NewUser, insert},
    };

    /// The `instances` row of `host`: `inserted_at`, `updated_at` and its
    /// mark.
    async fn instance_row(
        pool: &PgPool,
        host: &str,
    ) -> (DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>) {
        sqlx::query_as::<_, (DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>)>(
            "SELECT inserted_at, updated_at, unreachable_since FROM instances WHERE host = $1",
        )
        .bind(host)
        .fetch_one(pool)
        .await
        .expect("the host has a row")
    }

    /// A successfully authenticated delivery is proof the signer's host is
    /// alive: the pipeline clears its `unreachable_since` mark and touches
    /// the row, and the job completes exactly as it would have — the
    /// bookkeeping never changes the outcome, so asserting it after the
    /// success covers both.
    #[tokio::test]
    async fn an_authenticated_delivery_clears_the_unreachable_mark() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        // The host went quiet a day ago and its stamps aged with it, so the
        // pipeline's touch is an observable move rather than a race with
        // the clock. The comparisons below read the aged stamps back from
        // the database: those are the exact values the columns hold.
        let quiet = Utc::now() - TimeDelta::days(1);
        instance::set_unreachable(&pool, "peer.example", quiet)
            .await
            .expect("the mark seeds");
        let _aged =
            sqlx::query("UPDATE instances SET inserted_at = $2, updated_at = $2 WHERE host = $1")
                .bind("peer.example")
                .bind(quiet)
                .execute(&pool)
                .await
                .expect("the stamps are settable");
        let (inserted, aged, mark) = instance_row(&pool, "peer.example").await;
        assert!(mark.is_some(), "the host starts out marked unreachable");

        let outcome = deliver(
            &app_state(&pool),
            &alice,
            &follow_activity("reachable-1"),
            &test_handlers(),
        )
        .await;

        assert_eq!(outcome, Ok(()), "the pipeline completes as always");
        assert_eq!(
            seen("ok", "https://peer.example/activities/reachable-1"),
            1,
            "and routed the activity"
        );
        let (inserted_now, updated_now, mark_now) = instance_row(&pool, "peer.example").await;
        assert!(
            mark_now.is_none(),
            "the authenticated delivery cleared the mark"
        );
        assert_eq!(inserted_now, inserted, "first sight is never rewritten");
        assert!(updated_now > aged, "and the row was touched");
    }

    /// The mark-clear of a host the ledger never saw updates no row — the
    /// database's own no-op, which cannot fail — and the touch creates the
    /// row unmarked; a signer whose `ap_id` names no host is skipped
    /// outright. Both deliveries complete like any other.
    #[tokio::test]
    async fn a_first_sight_host_is_recorded_unmarked_and_a_hostless_signer_is_skipped() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = insert(
            &pool,
            &NewUser {
                nickname: "carol@fresh.example".to_owned(),
                ap_id: "https://fresh.example/users/carol".to_owned(),
                local: Flag::new(false),
                domain: Some("fresh.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts");
        let first_sight = json!({
            "id": "https://fresh.example/activities/first-1",
            "type": "Follow",
            "actor": "https://fresh.example/users/carol",
            "object": "https://test.example/users/bob",
        });

        let outcome = deliver(&app_state(&pool), &carol, &first_sight, &test_handlers()).await;

        assert_eq!(
            outcome,
            Ok(()),
            "a delivery with no mark to clear completes"
        );
        let (_, _, mark) = instance_row(&pool, "fresh.example").await;
        assert!(mark.is_none(), "and the created row carries no mark");

        let mallory = insert(
            &pool,
            &NewUser {
                nickname: "mallory@no-host.example".to_owned(),
                ap_id: "mailto:no-host".to_owned(),
                local: Flag::new(false),
                domain: Some("no-host.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the hostless fixture inserts");
        let hostless = json!({
            "id": "https://no-host.example/activities/1",
            "type": "Follow",
            "actor": "mailto:no-host",
            "object": "https://test.example/users/bob",
        });

        let hostless_outcome =
            deliver(&app_state(&pool), &mallory, &hostless, &Handlers::new()).await;

        assert_eq!(
            hostless_outcome,
            Ok(()),
            "a signer with no parsable host skips the record and completes"
        );
    }
}
