//! The producing half of delivery: [`best_inbox`] picks the address an
//! actor receives at and [`enqueue_delivery`] puts one signed delivery of
//! one activity onto the outgoing queue. Neither talks to the peer: every
//! refusal this half makes is about the address or the instance's own
//! policy, and the activity then waits in the database until [`handle`]
//! runs an attempt.
//!
//! [`handle`]: crate::federation::delivery::handle

use serde_json::{Value, json};
use tracing::{debug, warn};

use super::{
    KIND_DELIVER, MAX_ATTEMPTS, UNIQUE_KEY_PREFIX,
    send::{activity_id_of, is_local_inbox},
};
use crate::{
    error::AppResult,
    federation::policy::host_refused,
    jobs::QUEUE_FEDERATOR_OUTGOING,
    models::{
        job::{NewJob, enqueue},
        user::User,
    },
    state::AppState,
    util::url::{canonical_host, parse_http_url},
};

/// The inbox to deliver to `actor`: their shared (instance-wide) inbox when
/// they published one, their personal inbox otherwise.
///
/// `ActivityPub`'s shared-inbox rule, Pleroma's `maybe_use_sharedinbox/1`;
/// `None` means there is nowhere to send anything at all.
///
/// This only picks the address: whether several recipients on one host share
/// a single POST is the caller's decision, and every caller starts from the
/// same choice.
#[must_use]
pub fn best_inbox(actor: &User) -> Option<String> {
    actor.shared_inbox.clone().or_else(|| actor.inbox.clone())
}

/// Queues one signed delivery of `activity` to `inbox`, signed as the local
/// actor `actor`.
///
/// The activity rides in the job's `body` argument as JSON and is serialized
/// to bytes by [`handle`], once per attempt, because the bytes that are sent
/// must be exactly the bytes that were digested and signed. The job lands on
/// [`QUEUE_FEDERATOR_OUTGOING`] with [`MAX_ATTEMPTS`] and the deduplication
/// key `deliver:{activity_id}:{inbox}`, so calling this twice for the same
/// pair while a delivery is live queues nothing the second time.
///
/// While the instance's configuration says federation is off
/// ([`Config::federation_enabled`](crate::config::Config::federation_enabled)),
/// the call is a no-op: an instance that does not federate sends nothing,
/// and refusing to federate is not a failure.
///
/// Three inboxes are refused without an error and without a job: one this
/// instance serves itself (its authority is the actor's own, so the activity
/// needs no round trip through HTTP), one that is not an `http(s)` URL with a
/// host, and one whose host the instance refuses — a `reject` row of
/// the policy table (for the host or a parent domain) or a host its
/// [`mrf`] domain lists reject ([`MrfConfig::rejected`]).
///
/// [`mrf`]: crate::federation::mrf
/// [`MrfConfig::rejected`]: crate::federation::mrf::MrfConfig::rejected
/// [`handle`]: crate::federation::delivery::handle
/// [`AppError::Unprocessable`]: crate::error::AppError::Unprocessable
///
/// # Errors
///
/// [`AppError::Unprocessable`] when `activity` carries no string `id` (every
/// federating document has one, so that is a producer bug); a database
/// failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
pub async fn enqueue_delivery(
    state: &AppState,
    inbox: &str,
    actor: &User,
    activity: &Value,
) -> AppResult<()> {
    if !state.config.federation_enabled.get() {
        debug!(inbox, "delivery skipped: federation is disabled");
        return Ok(());
    }
    let Some(target) = parse_http_url(inbox) else {
        warn!(inbox, "delivery refused: the inbox is not an http(s) URL");
        return Ok(());
    };
    if is_local_inbox(&target, &actor.ap_id) {
        debug!(inbox, "delivery skipped: the inbox is this instance's own");
        return Ok(());
    }
    let host = canonical_host(target.host_str().unwrap_or_default());
    if host_refused(state, &host).await? {
        debug!(inbox, "delivery refused: instance policy rejects {host}");
        return Ok(());
    }
    enqueue_checked(state, inbox, actor, activity).await
}

/// [`enqueue_delivery`] without the checks: queues one signed delivery of
/// `activity` to `inbox` and nothing else.
///
/// The caller has already established everything [`enqueue_delivery`]
/// would: federation is on, `inbox` is an `http(s)` URL on a foreign
/// authority, and its host is not refused by instance policy. The
/// publisher does that once per publish, over the whole candidate set, so
/// this per-inbox step does no policy read and no MRF configuration load.
/// The deduplication key and queue are the same as [`enqueue_delivery`]'s.
///
/// # Errors
///
/// The same as [`enqueue_delivery`]: [`AppError::Unprocessable`] for an
/// `activity` without a string `id`, a database failure as
/// [`AppError::Database`](crate::error::AppError).
///
/// [`AppError::Unprocessable`]: crate::error::AppError::Unprocessable
pub async fn enqueue_checked(
    state: &AppState,
    inbox: &str,
    actor: &User,
    activity: &Value,
) -> AppResult<()> {
    let activity_id = activity_id_of(activity)?;
    let unique_key = format!("{UNIQUE_KEY_PREFIX}:{activity_id}:{inbox}");
    let queued = enqueue(
        &state.db,
        NewJob {
            queue: QUEUE_FEDERATOR_OUTGOING,
            kind: KIND_DELIVER,
            args: json!({
                "inbox": inbox,
                "actor_id": actor.id,
                "body": activity,
                "activity_id": activity_id,
            }),
            run_at: None,
            max_attempts: Some(MAX_ATTEMPTS),
            unique_key: Some(&unique_key),
        },
    )
    .await?;
    if queued.is_none() {
        debug!(%unique_key, "delivery already queued for this activity and inbox");
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::{
        config::{Config, Toggle},
        error::AppError,
        federation::delivery::test_support::{
            activity, actor, job_count, mirror, policy, queued_job, state_with, test_state,
        },
        services::runtime_config,
    };

    #[tokio::test]
    async fn best_inbox_prefers_the_shared_inbox() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let shared = mirror(
            &pool,
            "shared",
            Some("https://peer.example/users/bob/inbox"),
            Some("https://peer.example/inbox"),
        )
        .await;
        let personal = mirror(
            &pool,
            "personal",
            Some("https://peer.example/users/bob/inbox"),
            None,
        )
        .await;
        let nowhere = mirror(&pool, "nowhere", None, None).await;

        assert_eq!(
            best_inbox(&shared).as_deref(),
            Some("https://peer.example/inbox"),
            "one POST per instance when the actor publishes a shared inbox"
        );
        assert_eq!(
            best_inbox(&personal).as_deref(),
            Some("https://peer.example/users/bob/inbox"),
            "the personal inbox when there is no shared one"
        );
        assert_eq!(best_inbox(&nowhere), None, "and nowhere to send at all");
    }

    #[tokio::test]
    async fn a_local_inbox_is_refused_without_a_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;

        enqueue_delivery(
            &test_state(&pool),
            "https://test.example/users/bob/inbox",
            &alice,
            &activity(),
        )
        .await
        .unwrap();

        assert_eq!(
            job_count(&pool).await,
            0,
            "delivering into this instance's own inbox is skipped"
        );
    }

    #[tokio::test]
    async fn a_rejected_domain_is_refused_without_a_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        policy(&pool, "spam.example", "reject").await;
        policy(&pool, "quiet.example", "silence").await;
        let alice = actor(&pool).await;

        for (inbox, expected) in [
            ("https://spam.example/inbox", 0),
            ("https://quiet.example/inbox", 1),
        ] {
            enqueue_delivery(&test_state(&pool), inbox, &alice, &activity())
                .await
                .unwrap();
            assert_eq!(
                job_count(&pool).await,
                expected,
                "`{inbox}` leaves {expected} job(s) behind: a rejected host is \
                 refused, a silenced one is still delivered to"
            );
        }
    }

    /// The MRF domain lists refuse a delivery the way a table row does:
    /// a host on the `reject` list, and — allowlist mode — every host the
    /// `accept` list does not name, queue nothing.
    #[tokio::test]
    async fn a_domain_the_mrf_lists_reject_is_refused_without_a_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        runtime_config::write(
            &pool,
            ":pleroma",
            ":mrf",
            &json!({
                "policies": ["simple"],
                "simple": {"reject": ["*.evil.example"], "accept": [["peer.example", "our peer"]]},
            }),
        )
        .await
        .unwrap();
        let alice = actor(&pool).await;

        for (inbox, expected) in [
            ("https://cdn.evil.example/inbox", 0),
            ("https://peer.example/inbox", 1),
            ("https://someone-else.example/inbox", 1),
        ] {
            enqueue_delivery(&test_state(&pool), inbox, &alice, &activity())
                .await
                .unwrap();
            assert_eq!(
                job_count(&pool).await,
                expected,
                "`{inbox}` leaves the queue at {expected} job(s): the reject list names \
                 the evil host, the allowlist keeps only the peer"
            );
        }
    }

    #[tokio::test]
    async fn a_non_http_inbox_is_refused_without_a_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        for inbox in ["ftp://peer.example/inbox", "not a url", "https://", ""] {
            enqueue_delivery(&test_state(&pool), inbox, &alice, &activity())
                .await
                .unwrap_or_else(|err| panic!("`{inbox}` refuses without an error: {err}"));
        }

        assert_eq!(job_count(&pool).await, 0, "nothing was queued");
    }

    #[tokio::test]
    async fn the_unique_key_queues_one_delivery_per_pair() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let inbox = "https://peer.example/inbox";

        enqueue_delivery(&test_state(&pool), inbox, &alice, &activity())
            .await
            .unwrap();
        enqueue_delivery(&test_state(&pool), inbox, &alice, &activity())
            .await
            .unwrap();

        let job = queued_job(&pool).await;
        assert_eq!(job_count(&pool).await, 1, "the second call queued nothing");
        assert_eq!(
            job.unique_key.as_deref(),
            Some("deliver:https://test.example/activities/1:https://peer.example/inbox"),
            "the dedup key names the activity and the inbox"
        );
        assert_eq!(job.kind, KIND_DELIVER, "the delivery kind");
        assert_eq!(job.queue, QUEUE_FEDERATOR_OUTGOING, "the outgoing queue");
        assert_eq!(job.max_attempts, MAX_ATTEMPTS, "seven attempts");
        assert_eq!(job.attempts, 0, "nothing has been tried yet");
        assert_eq!(job.args["inbox"], inbox, "the inbox argument");
        assert_eq!(
            job.args["actor_id"],
            alice.id.to_string(),
            "the signing actor"
        );
        assert_eq!(job.args["body"], activity(), "the activity rides along");
        assert_eq!(
            job.args["activity_id"],
            activity()["id"],
            "and its id is spelled out"
        );

        enqueue_delivery(
            &test_state(&pool),
            "https://other.example/inbox",
            &alice,
            &activity(),
        )
        .await
        .unwrap();
        assert_eq!(job_count(&pool).await, 2, "a second inbox is its own job");
    }

    /// While `FEDERATION_ENABLED` is off, [`enqueue_delivery`] is a no-op:
    /// even a well-formed pair aimed at a reachable remote leaves no job
    /// behind, because an instance that does not federate sends nothing.
    #[tokio::test]
    async fn a_disabled_instance_queues_no_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let mut config = Config::for_tests();
        config.federation_enabled = Toggle::new(false);

        enqueue_delivery(
            &state_with(&pool, config),
            "https://peer.example/inbox",
            &alice,
            &activity(),
        )
        .await
        .unwrap();

        assert_eq!(
            job_count(&pool).await,
            0,
            "a disabled federation refuses to queue, without an error"
        );
    }

    #[tokio::test]
    async fn an_activity_without_an_id_is_an_error() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let keyless = json!({"type": "Create"});

        let err = enqueue_delivery(
            &test_state(&pool),
            "https://peer.example/inbox",
            &alice,
            &keyless,
        )
        .await
        .expect_err("an unnamed activity cannot be deduplicated");

        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "a producer bug is a 422, got {err}"
        );
        assert_eq!(job_count(&pool).await, 0, "and nothing was queued");
    }
}
