//! The retry scenarios: the `500` retried on the queue's own backoff and
//! the `503` at its `Retry-After` moment, both resending the same signed
//! bytes; the `410 Gone` that drops the address for good and marks the host
//! unreachable; and the peer answering `401` like a follower whose key no
//! longer verifies — with the delivery-row reader and the resend assertion
//! they share. Nothing sleeps: the attempts advance by backdating rows.

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use wired_be::models::{
    job::Job,
    user::{User, get_by_ap_id},
};

use super::expectations::create_id_of;
use super::support::{
    BOB, FIRST_BACKOFF_SECS, delivery_job, local, make_retries_due, peer_follows, post,
    seconds_until_due,
};
use crate::{common, remote};

/// Asserts one read of the retried delivery row: its state, the attempts
/// spent, and `answer` in the `last_error` of its most recent failure —
/// which [`complete`] leaves on the row even once the job finishes.
///
/// [`complete`]: wired_be::models::job::complete
async fn assert_job_after(
    pool: &PgPool,
    activity_id: &str,
    state_want: &str,
    attempts_want: i32,
    answer: &str,
) -> Job {
    let job = delivery_job(pool, activity_id).await;
    assert_eq!(job.state, state_want, "the delivery row's state");
    assert_eq!(job.attempts, attempts_want, "attempts spent so far");
    assert!(
        job.last_error
            .as_deref()
            .is_some_and(|err| err.contains(answer)),
        "the error names the peer's answer ({answer}): {:?}",
        job.last_error
    );
    job
}

/// Scenario: the retry ladder. The mock's inbox answers the `Create` once
/// with `500` and once with `503 Retry-After: 3600`: the first failure puts
/// the job back on the queue's own fifteen-second backoff — due later, and
/// untouched by a drain that runs now — the second schedules the peer's own
/// moment instead, and each later attempt resends the byte-identical signed
/// body. Nothing sleeps: the attempts advance by backdating the job row.
#[tokio::test]
async fn a_failing_inbox_retries_the_same_signed_bytes_until_it_answers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-retry-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let posted = post(&app, &alice_token, json!({"status": "ride it out"})).await;
    let create_id = create_id_of(&posted);
    peer.script_inbox(StatusCode::INTERNAL_SERVER_ERROR, None, 1);
    let _first_pass = common::run_all_jobs(&state).await;

    let stalled = assert_job_after(&pool, &create_id, "retryable", 1, "500").await;
    assert_eq!(
        seconds_until_due(&pool, &stalled).await,
        FIRST_BACKOFF_SECS,
        "due again at the queue's own backoff step"
    );
    let idle = common::run_all_jobs(&state).await;
    assert_eq!(idle, 0, "the not-yet-due delivery is not claimed early");

    make_retries_due(&pool).await;
    peer.script_inbox(
        StatusCode::SERVICE_UNAVAILABLE,
        Some(Duration::from_secs(3600)),
        1,
    );
    let _second_pass = common::run_all_jobs(&state).await;
    let throttled = assert_job_after(&pool, &create_id, "retryable", 2, "503").await;
    assert_eq!(
        seconds_until_due(&pool, &throttled).await,
        3600,
        "the peer's Retry-After beat the queue's own backoff"
    );
    assert!(
        throttled
            .last_error
            .as_deref()
            .is_some_and(|err| err.contains("3600")),
        "the error names the peer's own delay: {:?}",
        throttled.last_error
    );

    make_retries_due(&pool).await;
    let _third_pass = common::run_all_jobs(&state).await;
    let finished = assert_job_after(&pool, &create_id, "completed", 3, "503").await;
    assert_eq!(finished.state, "completed", "the ladder ends in a success");

    assert_signed_resends(&peer, &alice, 3);
}

/// Asserts the mock recorded `want` deliveries of the one `Create`, each
/// byte-identical to the first and each freshly signed by `alice`'s key.
fn assert_signed_resends(peer: &remote::MockPeer, alice: &User, want: usize) {
    let creates = peer.delivered("Create");
    assert_eq!(creates.len(), want, "each try arrived at the mock");
    let pem = alice.public_key.as_deref().expect("the fixture key");
    for delivery in &creates {
        assert_eq!(delivery.body(), creates[0].body(), "the same body, resent");
        assert!(
            delivery.digest_matches() && delivery.signature_verifies(pem),
            "every attempt went out freshly signed over the same bytes"
        );
        assert_eq!(
            delivery.key_id().as_deref(),
            Some(format!("{}#main-key", alice.ap_id)).as_deref(),
            "the same key names every attempt"
        );
    }
}

/// Scenario: the closed address. Once the mock answers `410 Gone`, the
/// delivery is discarded — not retried, never re-claimed — the peer's own
/// word marks its host unreachable in the ledger the admin surface reads,
/// and every later post skips the address: the fan-out computes its
/// candidates, finds the host marked, and queues nothing.
#[tokio::test]
async fn a_gone_inbox_drops_the_address_and_later_posts_skip_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-gone-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let _fine = post(&app, &alice_token, json!({"status": "delivered fine"})).await;
    let _drained = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("Create").len(),
        1,
        "one delivery lands first"
    );

    peer.script_inbox_permanently(StatusCode::GONE, None);
    let doomed = post(&app, &alice_token, json!({"status": "nowhere to go"})).await;
    let _drained_again = common::run_all_jobs(&state).await;
    let ended = assert_job_after(&pool, &create_id_of(&doomed), "discarded", 1, "410").await;
    assert!(
        ended.run_at <= chrono::Utc::now(),
        "a discard never reschedules"
    );

    make_retries_due(&pool).await;
    let _reclaim = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("Create").len(),
        2,
        "the discarded job is never re-claimed: it tried once"
    );

    let status = common::TestRequest::get("/api/v1/pleroma/federation_status")
        .send(&app)
        .await;
    common::assert_answer(&status, StatusCode::OK, "the reachability ledger reads");
    assert!(
        status.json_value()["unreachable"]
            .get("127.0.0.1")
            .is_some(),
        "the 410 marked the peer's host unreachable: {}",
        status.text()
    );

    let _skipped = post(&app, &alice_token, json!({"status": "never leaves"})).await;
    let _quiet = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("Create").len(),
        2,
        "and later posts skip the dropped address — nothing new arrives"
    );
}

/// Scenario: the signature the peer cannot verify. The mock rotates the key
/// its actor documents publish and its inbox thereafter answers `401`, the
/// way a follower that can no longer check our signature does: the delivery
/// treats the first refusal as a rotation worth retrying, and the second is
/// a real refusal it gives up on. The stored mirror keeps the key it was
/// fetched with — the delivery path never refetches a follower — and every
/// attempt's capture shows the valid signature this instance sent it under.
#[tokio::test]
async fn a_refused_signature_retries_once_and_the_capture_keeps_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-rotate-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;
    let mirror = get_by_ap_id(&pool, &peer.actor(BOB))
        .await
        .expect("the mirror reads")
        .expect("the follow mirrored the peer actor");
    let stored_pem = mirror.public_key.clone().expect("the mirror holds a key");

    peer.rotate_keys();
    assert_ne!(
        stored_pem,
        peer.served_public_key(),
        "the peer now publishes a different key than this instance holds"
    );
    peer.script_inbox_permanently(StatusCode::UNAUTHORIZED, None);

    let posted = post(&app, &alice_token, json!({"status": "who verifies me"})).await;
    let create_id = create_id_of(&posted);
    let _first_pass = common::run_all_jobs(&state).await;
    let retried = assert_job_after(&pool, &create_id, "retryable", 1, "401").await;
    assert_eq!(
        retried.attempts, 1,
        "the first 401 is given its refresh window"
    );

    make_retries_due(&pool).await;
    let _second_pass = common::run_all_jobs(&state).await;
    let dropped = assert_job_after(&pool, &create_id, "discarded", 2, "401").await;
    assert_eq!(dropped.max_attempts, 7, "the budget was never the issue");

    let unchanged = get_by_ap_id(&pool, &peer.actor(BOB))
        .await
        .expect("the mirror reads")
        .expect("the mirror stands");
    assert_eq!(
        unchanged.public_key.as_deref(),
        Some(stored_pem.as_str()),
        "the delivery path never refetched the follower's document"
    );
    assert_signed_resends(&peer, &alice, 2);
}
