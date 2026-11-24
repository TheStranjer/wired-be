//! The stages a job delivery runs through between the inbox route's args and
//! the registry dispatch: reading the args, loading the signer, containment,
//! the ledger claim and the policy hooks.

use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use super::{
    Handlers, KIND_INCOMING,
    containment::containment_failure,
    error::is_retryable,
    ledger::{claim_ledger_row, release_ledger_row},
    routing::route,
    types::{ActivityView, MrfFn},
};
use crate::{
    federation::{json, mrf, policy::host_refused},
    jobs::JobError,
    models::{user, user::User},
    state::AppState,
};

/// The activity document and the signer's id, exactly as the inbox route
/// wrote them.
pub(super) struct Incoming {
    /// The received `ActivityPub` document.
    pub(super) activity: Value,
    /// The `users.id` of the actor whose key authenticated the delivery.
    pub(super) signer_id: Uuid,
    /// The `users.id` of the local account whose personal inbox received
    /// the delivery; `None` for the shared inboxes.
    pub(super) inbox_owner: Option<Uuid>,
}

/// Reads [`Incoming`] out of the job args; an args object that does not
/// describe a delivery no inbox route enqueued is discarded, not retried,
/// because no retry can make it one. A null, missing or unparseable
/// `inbox_owner` reads as a shared-inbox delivery.
pub(super) fn extract_incoming(args: &Value) -> Result<Incoming, JobError> {
    let discard = |why: &str| JobError::Discard(format!("unusable {KIND_INCOMING} args: {why}"));
    let activity = args.get("activity").ok_or_else(|| discard("no activity"))?;
    let signer_id = args
        .get("signer_id")
        .and_then(Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok())
        .ok_or_else(|| discard("no signer id"))?;
    let inbox_owner = args
        .get("inbox_owner")
        .and_then(Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok());
    Ok(Incoming {
        activity: activity.clone(),
        signer_id,
        inbox_owner,
    })
}

/// Loads the signer the [`SignedActivity`] extractor authenticated. A
/// gone row means the account was deleted after the delivery was queued:
/// nothing to process it for any more.
///
/// [`SignedActivity`]: crate::federation::inbox_auth::SignedActivity
pub(super) async fn load_signer(state: &AppState, id: Uuid) -> Result<User, JobError> {
    user::get_by_id(&state.db, id)
        .await
        .map_err(|err| JobError::Retry(format!("could not read the signer: {err}")))?
        .ok_or_else(|| JobError::Discard("the signer account is gone".to_owned()))
}

/// Containment, the ledger claim, and everything after them.
pub(super) async fn run_pipeline(
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
    let Some(mut processed) = ActivityView::parse(&accepted) else {
        debug!("the policy pipeline accepted a document that does not parse; dropping");
        return Ok(());
    };
    processed.inbox_owner = view.inbox_owner;
    if policy_rejects(state, &processed).await? {
        debug!("instance policy rejects the actor's domain");
        return Ok(());
    }
    route(state, processed, signer, handlers).await
}

/// The instance-policy arm of the policy hooks: a signer host the instance
/// refuses — the policy table or the MRF domain lists, as
/// [`host_refused`] decides. The inbox refuses these deliveries at the door; this is
/// the same check inside the queue, where the [MRF
/// stage](mrf::filter_incoming) may have rewritten the actor.
async fn policy_rejects(state: &AppState, view: &ActivityView) -> Result<bool, JobError> {
    // Containment already proved the actor an http(s) url with a host.
    let Some(host) = json::http_host(&view.actor) else {
        return Ok(false);
    };
    host_refused(state, &host)
        .await
        .map_err(|err| JobError::Retry(format!("the instance policy read failed: {err}")))
}

#[cfg(test)]
mod tests {
    use futures::future::BoxFuture;
    use serde_json::json;

    use super::*;
    use crate::federation::{
        dispatch::{
            dispatch_activity,
            test_support::{
                app_state, args_with, broken_state, follow_activity, incoming_args, ledger_rows,
                seen, signer, test_handlers,
            },
        },
        mrf::MrfDecision,
    };

    /// The rejecting policy fake.
    fn reject_mrf(_state: AppState, _view: &ActivityView) -> BoxFuture<'static, MrfDecision> {
        Box::pin(async { MrfDecision::Reject("test policy".to_owned()) })
    }

    /// The `inbox_owner` arg: a uuid string is read, anything else is a
    /// shared-inbox delivery.
    #[test]
    fn the_inbox_owner_is_read_when_present_and_none_otherwise() {
        let owner = Uuid::now_v7();
        let base = |owner_arg: Option<Value>| {
            let mut args =
                json!({"activity": {"id": "x"}, "signer_id": Uuid::now_v7().to_string()});
            if let Some(value) = owner_arg {
                args["inbox_owner"] = value;
            }
            extract_incoming(&args)
                .expect("the args are usable")
                .inbox_owner
        };

        assert_eq!(
            base(Some(json!(owner.to_string()))),
            Some(owner),
            "a personal-inbox delivery names its owner"
        );
        assert_eq!(base(Some(Value::Null)), None, "null is a shared inbox");
        assert_eq!(base(None), None, "a missing key is a shared inbox");
        assert_eq!(base(Some(json!("not-a-uuid"))), None, "so is junk");
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

    /// The accepting policy fake: the follow fixture `policy-2`, whatever
    /// the view says.
    fn accept_mrf(_state: AppState, _view: &ActivityView) -> BoxFuture<'static, MrfDecision> {
        Box::pin(async { MrfDecision::Accept(follow_activity("policy-2")) })
    }

    /// A reject row for a parent domain reaches the actor's subdomain, and
    /// so does the MRF `reject` list, which needs no table row.
    #[tokio::test]
    async fn the_policy_arm_covers_parent_domains_and_the_mrf_lists() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let activity = follow_activity("policy-2");
        let state = app_state(&pool);
        let view = ActivityView::parse(&activity).expect("the fixture parses");

        assert!(
            !policy_rejects(&state, &view).await.unwrap(),
            "no policy, no rejection"
        );

        let _policy = sqlx::query(
            "INSERT INTO instance_policy_domains (domain, action, reason) \
             VALUES ('example', 'reject', 'test')",
        )
        .execute(&pool)
        .await
        .expect("the policy row inserts");
        assert!(
            policy_rejects(&state, &view).await.unwrap(),
            "the row for the parent domain covers `peer.example`"
        );

        let _cleared = sqlx::query("TRUNCATE instance_policy_domains")
            .execute(&pool)
            .await
            .expect("the table clears");
        crate::services::runtime_config::write(
            &pool,
            ":pleroma",
            ":mrf",
            &json!({"policies": ["simple"], "simple": {"reject": ["peer.example"]}}),
        )
        .await
        .expect("the MRF entry is stored");
        let outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&activity, &alice),
            &test_handlers(),
            accept_mrf,
        )
        .await;
        assert_eq!(outcome, Ok(()), "the rejection completes the job");
        assert_eq!(
            seen("ok", "https://peer.example/activities/policy-2"),
            0,
            "the MRF list refuses the actor even past an accepting filter"
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
}
