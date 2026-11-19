//! The follow-request endpoint's arms: the `Accept` an authorization and the
//! `Reject` a refusal are, the `Reject` a forced removal of a REMOTE
//! follower answers with, and the gate both run through.
//!
//! The documents themselves are [`super::follows`]' [`accept_follow`] and
//! [`reject_follow`]; the requester read and the answer's logging are here.

use tracing::{debug, warn};

use super::follows::{FollowAnswer, accept_follow, reject_follow};
use super::load_user;
use crate::{
    models::{follow::Follow, user::User},
    state::AppState,
};

/// Answer the follow request `resolver` just resolved when the requester
/// lives on another instance.
///
/// An authorization is an [`accept_follow`], a refusal a [`reject_follow`],
/// both answering the delivered `Follow` by the id the request row stores
/// and travelling to the follower's inbox alone. A local requester's answer
/// is the row itself (their timeline is the delivery), and a request that
/// carries no delivered activity id has nothing to answer by. As every
/// endpoint hook here the answer is fire-and-forget: a failed fan-out
/// arrives in the log, never in the client's response — the resolution
/// itself already stands.
pub async fn after_follow_request_resolved(
    state: &AppState,
    resolver: &User,
    request: &Follow,
    granted: bool,
) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !resolver.local.get() {
        return; // only our own actors answer their follow requests
    }
    let Some(follower) = load_user(state, request.follower_id).await else {
        return; // a requester whose row is gone is nobody to answer
    };
    answer_follow_request(state, resolver, &follower, request, granted).await;
}

/// Refuse a REMOTE follower whose accepted follow of the local `target` was
/// just forced out of the table by [`remove_follower`].
///
/// The document is the `Reject` [`reject_follow`] publishes for the pair,
/// answering the delivered `Follow` by its stored id. A local follower, a
/// gone row and an id-less request answer with silence, as in
/// [`after_follow_request_resolved`].
///
/// [`remove_follower`]: crate::models::follow::remove_follower
pub async fn after_follower_removed(state: &AppState, target: &User, removed: &Follow) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !target.local.get() {
        return; // only our own actors answer their followers
    }
    let Some(follower) = load_user(state, removed.follower_id).await else {
        return; // a follower whose row is gone is nobody to refuse
    };
    answer_follow_request(state, target, &follower, removed, false).await;
}

/// The shared body of [`after_follow_request_resolved`] and
/// [`after_follower_removed`]: the REMOTE gate on the requester, the
/// delivered activity's stored id, and the answer [`accept_follow`] /
/// [`reject_follow`] publish to the follower's inbox alone — logged at both
/// outcomes, since the endpoint's write never waits on the wire.
async fn answer_follow_request(
    state: &AppState,
    target: &User,
    follower: &User,
    request: &Follow,
    granted: bool,
) {
    if follower.local.get() {
        return; // a local pair is settled in the database
    }
    let Some(follow_id) = request.ap_id.as_deref() else {
        return; // no delivered activity to answer by its id
    };
    let answer = FollowAnswer {
        follow_id,
        follower_ap_id: &follower.ap_id,
    };
    let outcome = if granted {
        accept_follow(state, target, &answer).await
    } else {
        reject_follow(state, target, &answer).await
    };
    match outcome {
        Ok(count) => debug!(
            actor = %target.ap_id,
            follower = %follower.ap_id,
            granted,
            count,
            "queued follow-request answer"
        ),
        Err(err) => warn!(
            %err,
            actor = %target.ap_id,
            follower = %follower.ap_id,
            "could not queue follow-request answer"
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        models::follow,
        services::federation_out::test_support::{
            app_state, assert_fresh_activity_id, delivery_jobs, job_count, local, remote,
        },
    };

    /// Records the REMOTE requester `follower`'s follow of the local
    /// `target` and lands the delivered activity's id on it — the shape the
    /// inbound fold stores for a peer's `Follow`, which the answers name.
    async fn remote_request(pool: &PgPool, follower: &User, target: &User, n: &str) -> Follow {
        let mut row = follow::follow(pool, TEST_BASE_URL, follower, target)
            .await
            .expect("the request fixture writes");
        let delivered = format!("https://remote.example/activities/{n}");
        follow::set_ap_id(pool, row.id, &delivered)
            .await
            .expect("the delivered id lands");
        row.ap_id = Some(delivered);
        row
    }

    /// Authorizing a REMOTE requester's follow delivers the `Accept` to
    /// their personal inbox alone: the followee acts, the delivered
    /// `Follow` rides inside by its id — the same answer the inbound
    /// handler sends on the spot for an unlocked account.
    #[tokio::test]
    async fn authorizing_a_remote_request_delivers_the_accept() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = local(&pool, "carol").await;
        let bob = remote(&pool, "bob").await;
        let row = remote_request(&pool, &bob, &carol, "fo-ask-1").await;
        let state = app_state(&pool);

        after_follow_request_resolved(&state, &carol, &row, true).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the grant answers once");
        assert_eq!(
            jobs[0]["inbox"], "https://remote.example/users/bob/inbox",
            "the follower's personal inbox is the sole target"
        );
        assert_eq!(
            jobs[0]["actor_id"],
            carol.id.to_string(),
            "signed as the followee"
        );
        let body = &jobs[0]["body"];
        assert_eq!(body["type"], "Accept");
        assert_eq!(body["actor"], carol.ap_id);
        assert_eq!(body["to"], json!([bob.ap_id]));
        assert_fresh_activity_id(body["id"].as_str().expect("the answer has an id"));
        assert_eq!(
            body["object"],
            json!({
                "id": row.ap_id.expect("the delivered id stands"),
                "type": "Follow",
                "actor": bob.ap_id,
                "object": carol.ap_id,
            }),
            "the answer embeds the delivered Follow, the shape peers match on"
        );
    }

    /// Refusing a REMOTE requester is the same document under `Reject`.
    #[tokio::test]
    async fn rejecting_a_remote_request_delivers_the_reject() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = local(&pool, "carol").await;
        let bob = remote(&pool, "bob").await;
        let row = remote_request(&pool, &bob, &carol, "fo-refuse-1").await;
        let state = app_state(&pool);

        after_follow_request_resolved(&state, &carol, &row, false).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the refusal answers once");
        assert_eq!(jobs[0]["inbox"], "https://remote.example/users/bob/inbox");
        let body = &jobs[0]["body"];
        assert_eq!(body["type"], "Reject");
        assert_eq!(body["actor"], carol.ap_id);
        assert_eq!(body["to"], json!([bob.ap_id]));
        assert_eq!(body["object"]["type"], "Follow");
        assert_eq!(
            body["object"]["id"],
            row.ap_id.expect("the delivered id stands"),
            "the refusal answers the same delivered activity"
        );
    }

    /// A LOCAL requester's answer — granted or refused — moves the row and
    /// never a wire: the timeline is the delivery. An id-less request (no
    /// delivered activity to answer) and a resolver who is not one of our
    /// own actors are silent too.
    #[tokio::test]
    async fn follow_request_answers_that_stay_local_deliver_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = local(&pool, "carol").await;
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let local_request = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("the local request writes");
        let idless = follow::follow(&pool, TEST_BASE_URL, &bob, &carol)
            .await
            .expect("the remote request writes");
        assert!(
            idless.ap_id.is_none(),
            "a remote follower's row starts without an id"
        );
        let state = app_state(&pool);

        after_follow_request_resolved(&state, &carol, &local_request, true).await;
        after_follow_request_resolved(&state, &carol, &local_request, false).await;
        after_follow_request_resolved(&state, &carol, &idless, true).await;
        after_follow_request_resolved(&state, &bob, &local_request, true).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "no answer of these four has a remote side to reach"
        );
    }

    /// Dropping a REMOTE follower answers their `Follow` with a `Reject`
    /// at their personal inbox — Pleroma's `User.remove_follower` outcome,
    /// the removed row's stored id embedded.
    #[tokio::test]
    async fn removing_a_remote_follower_delivers_the_reject() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let _request = remote_request(&pool, &bob, &alice, "fo-drop-1").await;
        let removed = follow::remove_follower(&pool, alice.id, bob.id)
            .await
            .expect("the removal runs")
            .expect("the follower was there");
        let state = app_state(&pool);

        after_follower_removed(&state, &alice, &removed).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the dropped follower is answered once");
        assert_eq!(
            jobs[0]["inbox"], "https://remote.example/users/bob/inbox",
            "the follower's personal inbox is the sole target"
        );
        assert_eq!(
            jobs[0]["actor_id"],
            alice.id.to_string(),
            "signed as the followee"
        );
        let body = &jobs[0]["body"];
        assert_eq!(body["type"], "Reject");
        assert_eq!(body["actor"], alice.ap_id);
        assert_eq!(body["to"], json!([bob.ap_id]));
        assert_fresh_activity_id(body["id"].as_str().expect("the answer has an id"));
        assert_eq!(
            body["object"]["id"],
            removed.ap_id.expect("the delivered id stands"),
            "the Reject answers the very Follow the row stood for"
        );
    }

    /// A LOCAL follower removed from the list is settled in the database:
    /// the answer never reaches a wire.
    #[tokio::test]
    async fn removing_a_local_follower_delivers_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let row = follow::follow(&pool, TEST_BASE_URL, &bob, &alice)
            .await
            .expect("the local follow writes");
        let removed = follow::remove_follower(&pool, alice.id, bob.id)
            .await
            .expect("the removal runs")
            .expect("the follower was there");
        assert_eq!(
            row.state, removed.state,
            "the fixture removed the accepted row"
        );
        let state = app_state(&pool);

        after_follower_removed(&state, &alice, &removed).await;

        assert_eq!(job_count(&pool).await, 0, "a local follower hears nothing");
    }
}
