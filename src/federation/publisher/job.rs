//! The background half of publishing: the handler of the
//! [`KIND_PUBLISH`](super::KIND_PUBLISH) job, which loads the local actor
//! the job names and runs the ordinary [`publish`] for it.

use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use super::{PublishArgs, PublishOpts, publish};
use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError, JobErrorExt as _},
    models::user::{User, get_by_id},
    state::AppState,
};

/// The [`KIND_PUBLISH`](super::KIND_PUBLISH) handler, registered by
/// [`crate::serve`]: loads the actor and runs the ordinary [`publish`] with
/// default options.
pub fn handle(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { run_publish(state, args).await })
}

/// Runs one background publish. What can never work (malformed arguments,
/// an activity with no id, an actor that is gone or not a local one)
/// discards; what may work later (a database hiccup in the fan-out)
/// retries.
async fn run_publish(state: AppState, args: Value) -> Result<(), JobError> {
    let args =
        serde_json::from_value::<PublishArgs>(args).job_discard("malformed publish arguments")?;
    if !activity_has_id(&args.activity) {
        return Err(JobError::Discard(
            "the activity carries no string `id`".to_owned(),
        ));
    }
    let actor = publishing_actor(&state, args.actor_id).await?;
    let opts = PublishOpts {
        extra_inboxes: args.extra_inboxes,
        only_to: None,
    };
    let count = publish(&state, &actor, &args.activity, opts)
        .await
        .map_err(|err| {
            JobError::Retry(format!(
                "could not publish an activity as {}: {err}",
                args.actor_id
            ))
        })?;
    debug!(actor_id = %args.actor_id, count, "background publish enqueued deliveries");
    Ok(())
}

/// Reads the actor to publish as, by id: only a row this instance owns can
/// address its own followers. Gone and remote rows are permanent failures;
/// a database that will not answer is transient.
async fn publishing_actor(state: &AppState, actor_id: Uuid) -> Result<User, JobError> {
    let found = get_by_id(&state.db, actor_id)
        .await
        .job_retry("could not read the publishing actor")?;
    found
        .filter(|candidate| candidate.local.get())
        .ok_or_else(|| JobError::Discard(format!("actor {actor_id} is not a local publisher")))
}

/// Whether `activity` carries the string `id` every federating document
/// must have — the condition
/// [`enqueue_delivery`](crate::federation::delivery::enqueue_delivery)
/// refuses on, checked here so a doomed job discards before any fan-out
/// query runs.
fn activity_has_id(activity: &Value) -> bool {
    activity.get("id").and_then(Value::as_str).is_some()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::*;
    use super::super::{KIND_PUBLISH, publish_in_background};
    use super::*;
    use crate::{jobs::QUEUE_FEDERATOR_OUTGOING, models::job::Job};

    /// The first attempt a background handler runs on.
    const fn first_attempt() -> AttemptInfo {
        AttemptInfo {
            attempts: 1,
            max_attempts: 5,
        }
    }

    /// How a [`handle`] run reads in a test message: the handler's own
    /// `Display`, since a `Result` has none.
    fn outcome_text(outcome: &Result<(), JobError>) -> String {
        match outcome {
            Ok(()) => "completed".to_owned(),
            Err(err) => err.to_string(),
        }
    }

    /// The background entry point queues one publish job, and its handler
    /// loads the actor and fans the activity out exactly as a direct
    /// publish would.
    #[tokio::test]
    async fn the_background_job_publishes_when_it_runs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let erin = mirror(
            &pool,
            "erin",
            "other.example",
            "https://other.example/users/erin/inbox",
            None,
        )
        .await;
        followed(&pool, &erin, &alice).await;
        let state = app_state(&pool);
        let activity = public_activity(&alice);

        publish_in_background(&state, alice.id, &activity, &[], None)
            .await
            .unwrap();
        let job = sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 LIMIT 1")
            .bind(KIND_PUBLISH)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            job.queue, QUEUE_FEDERATOR_OUTGOING,
            "the publish job rides the outgoing queue"
        );
        assert_eq!(job.args["actor_id"], json!(alice.id), "the actor argument");
        assert_eq!(job.args["activity"], activity, "the activity argument");

        let outcome = handle(state.clone(), first_attempt(), job.args.clone()).await;
        assert_eq!(outcome, Ok(()), "the handler completes");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://other.example/users/erin/inbox"],
            "the handler's fan-out queued the follower's delivery"
        );
    }

    /// The background job also carries the recipients the activity's own
    /// addressing does not name: the `extra_inboxes` argument reaches
    /// [`publish`] through [`handle`], and a second enqueue under the same
    /// `unique_key` is swallowed by the queue's dedup — the debounced
    /// publisher's contract.
    #[tokio::test]
    async fn the_background_job_carries_extra_recipients_and_debounces() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let state = app_state(&pool);
        let activity = json!({
            "id": ACTIVITY_ID,
            "type": "Update",
            "actor": alice.ap_id,
            "object": alice.ap_id,
            "to": [PUBLIC],
        });
        let extra = vec!["https://extra.example/inbox".to_owned()];

        publish_in_background(&state, alice.id, &activity, &extra, Some("debounce/1"))
            .await
            .unwrap();
        publish_in_background(&state, alice.id, &activity, &extra, Some("debounce/1"))
            .await
            .unwrap();

        let jobs = sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1")
            .bind(KIND_PUBLISH)
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1, "the repeat hits the live job's key");
        assert_eq!(jobs[0].args["extra_inboxes"], json!(extra));

        let outcome = handle(state.clone(), first_attempt(), jobs[0].args.clone()).await;
        assert_eq!(outcome, Ok(()), "the handler completes");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://extra.example/inbox"],
            "and delivered what the caller named beside the addressing"
        );
    }

    /// What the background handler can never publish — malformed
    /// arguments, an activity with no id, an actor that is gone or not a
    /// local one — discards instead of retrying.
    #[tokio::test]
    async fn the_background_handler_discards_undeliverable_work() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let remote = mirror(
            &pool,
            "bob",
            "peer.example",
            "https://peer.example/users/bob/inbox",
            None,
        )
        .await;
        let state = app_state(&pool);
        let activity = public_activity(&alice);

        let cases = [
            (
                "junk arguments",
                json!({"inbox": "https://peer.example/inbox"}),
            ),
            (
                "no activity id",
                json!({"actor_id": alice.id, "activity": {"type": "Create"}}),
            ),
            (
                "a gone actor",
                json!({"actor_id": Uuid::now_v7(), "activity": activity}),
            ),
            (
                "a remote actor",
                json!({"actor_id": remote.id, "activity": activity}),
            ),
        ];
        for (what, args) in cases {
            let outcome = handle(state.clone(), first_attempt(), args).await;
            assert!(
                matches!(outcome, Err(JobError::Discard(_))),
                "{what} can never publish, got {}",
                outcome_text(&outcome)
            );
        }
        assert_eq!(job_count(&pool).await, 0, "no delivery was queued");
    }

    /// The worker knows the background publish kind.
    #[test]
    fn the_kind_is_registered_with_its_handler() {
        let registry = crate::job_registry(&crate::config::Config::for_tests());
        assert!(
            registry.get(KIND_PUBLISH).is_some(),
            "the worker knows `{KIND_PUBLISH}`"
        );
    }
}
