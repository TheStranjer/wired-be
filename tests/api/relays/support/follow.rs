//! The follow handshake, the restated follow, the followable shapes and the
//! withdrawal.

use super::*;

// --- the handshake -------------------------------------------------------------------

/// Following a relay writes the pair of rows and sends the `Follow` to
/// the relay's inbox — once, however often the follow is restated while
/// the send is still queued; the worker's POST lands at the mock; and the
/// relay's own `Accept` — folded by the existing handler, nobody having
/// to follow the relay for it — grants both the `follows` row and the
/// subscription.
#[tokio::test]
async fn following_a_relay_runs_the_handshake_to_accepted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;

    let subscription = follow_relay(&state, &mock.actor_ap_id())
        .await
        .expect("the relay follow runs");
    assert_eq!(
        subscription.actor_ap_id,
        mock.actor_ap_id(),
        "the subscription is keyed by the relay actor id"
    );
    assert_eq!(
        subscription.inbox,
        mock.inbox_url(),
        "and carries the inbox the fetched document named"
    );
    assert_eq!(subscription.state, RelayState::Pending, "pending first");

    let relay_user = relay_mirror(&state, &mock).await;
    assert_eq!(
        relay_user.actor_type, "Application",
        "the fetched actor is mirrored as the Application it is"
    );
    let row = instance_follow(&state, &relay_user)
        .await
        .expect("the instance actor now follows the relay");
    assert_eq!(row.state, FollowState::Pending, "the remote follow waits");
    let follow_id = row.ap_id.clone().expect("the row minted its id");
    assert!(
        follow_id.starts_with(&format!("{BASE}/activities/")),
        "the id is decision 6's local activity address: {follow_id}"
    );
    assert_queued_follow(&pool, &mock, &follow_id).await;

    let ran = common::run_all_jobs(&state).await;
    assert!(ran >= 1, "the delivery job ran");
    let posts = mock.received();
    assert_eq!(posts.len(), 1, "the relay's inbox recorded the one Follow");
    assert_eq!(posts[0]["id"], json!(follow_id), "with its activity id");

    answer_accept(&state, &mock, &relay_user, &follow_id).await;
    let granted = instance_follow(&state, &relay_user)
        .await
        .expect("the pair stands after the grant");
    assert_eq!(
        granted.state,
        FollowState::Accepted,
        "the follow stands granted"
    );
    assert_eq!(
        standing_relay(&pool, &mock.actor_ap_id()).await.state,
        RelayState::Accepted,
        "and so does the subscription"
    );
}

/// A restated follow while the send is still queued: one subscription,
/// the same pair under the same activity id, and the queue's per-activity
/// dedup swallows the repeat.
#[tokio::test]
async fn a_restated_relay_follow_sends_nothing_twice() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;

    let _subscription = follow_relay(&state, &mock.actor_ap_id())
        .await
        .expect("the relay follow runs");
    let relay_user = relay_mirror(&state, &mock).await;
    let row = instance_follow(&state, &relay_user)
        .await
        .expect("the instance actor now follows the relay");
    let follow_id = row.ap_id.clone().expect("the row minted its id");

    let restated = follow_relay(&state, &mock.actor_ap_id())
        .await
        .expect("re-following a relay is no error");
    assert_eq!(
        restated.state,
        RelayState::Pending,
        "the restatement does not move the state"
    );
    let again = instance_follow(&state, &relay_user)
        .await
        .expect("the pair stands");
    assert_eq!(again.id, row.id, "the same follows row");
    assert_eq!(again.ap_id, Some(follow_id), "the same activity id");
    let again_id = again.ap_id.expect("just compared equal to Some");
    assert_queued_follow(&pool, &mock, &again_id).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM relays")
            .fetch_one(&pool)
            .await
            .expect("the ledger reads"),
        1,
        "and still one subscription"
    );
}

/// A `Service` and a `Person` are followable relay shapes; a `Group` is
/// refused, and so is an address with no actor behind it or of no shape
/// at all.
#[tokio::test]
async fn only_relay_shapes_are_followable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;

    for path in ["/service", "/person"] {
        let url = format!("{}{path}", mock.mock.base);
        let subscription = follow_relay(&state, &url)
            .await
            .unwrap_or_else(|err| panic!("following `{path}` runs: {err}"));
        assert_eq!(
            subscription.actor_ap_id, url,
            "the subscription names the actor that was fetched"
        );
    }

    let group = follow_relay(&state, &format!("{}/group", mock.mock.base)).await;
    assert!(
        matches!(group, Err(AppError::Unprocessable(_))),
        "a Group actor is no relay: {group:?}"
    );
    let gone = follow_relay(&state, &format!("{}/missing", mock.mock.base)).await;
    assert!(
        matches!(gone, Err(AppError::NotFound)),
        "an actor that answers 404 is refused: {gone:?}"
    );
    let nonsense = follow_relay(&state, "not a host/ at all").await;
    assert!(
        matches!(nonsense, Err(AppError::Unprocessable(_))),
        "an address that is neither URL nor host is refused: {nonsense:?}"
    );
}

// --- the withdrawal -------------------------------------------------------------------

/// Unfollowing a relay sends the `Undo` of that very `Follow` to its
/// inbox, deletes the `follows` row and the `relays` row, and takes the
/// relay out of the fan-out; an unknown relay is the 404.
#[tokio::test]
async fn unfollowing_a_relay_withdraws_the_follow_and_the_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;
    let (row, relay_user) = followed_and_accepted(&state, &mock).await;
    let follow_id = row.ap_id.clone().expect("the minted id");
    let _delivered = common::run_all_jobs(&state).await;
    assert_eq!(mock.received().len(), 1, "the Follow reached the relay");

    unfollow_relay(&state, &mock.actor_ap_id())
        .await
        .expect("the withdrawal runs");

    let jobs = delivery_jobs(&pool).await;
    assert_eq!(jobs.len(), 2, "the Follow's Undo rides beside it");
    assert_eq!(jobs[1]["inbox"], mock.inbox_url(), "to the relay's inbox");
    let body = &jobs[1]["body"];
    assert_eq!(body["type"], "Undo", "the withdrawal activity");
    assert_eq!(
        body["actor"],
        format!("{BASE}{INSTANCE_ACTOR_PATH}"),
        "by the instance actor"
    );
    assert_eq!(
        body["to"],
        json!([mock.inbox_url()]),
        "addressed to the inbox"
    );
    assert_eq!(body["object"]["type"], "Follow", "embedding the Follow");
    assert_eq!(
        body["object"]["id"],
        json!(follow_id),
        "under its stored id"
    );
    assert_eq!(
        body["object"]["actor"],
        format!("{BASE}{INSTANCE_ACTOR_PATH}"),
        "as the instance actor sent it"
    );

    assert!(
        instance_follow(&state, &relay_user).await.is_none(),
        "the follows row is gone"
    );
    assert!(
        relay::get(&pool, &mock.actor_ap_id())
            .await
            .unwrap()
            .is_none(),
        "and so is the subscription"
    );
    assert!(
        list_relays(&pool).await.unwrap().is_empty(),
        "the ledger reads empty"
    );
    assert!(
        instance::relay_inboxes(&pool).await.unwrap().is_empty(),
        "and the fan-out hook holds nothing"
    );

    let _drained = common::run_all_jobs(&state).await;
    let posts = mock.received();
    assert_eq!(posts.len(), 2, "the relay's inbox recorded the Undo too");
    assert_eq!(posts[1]["type"], "Undo", "as an Undo");

    let stranger = unfollow_relay(&state, "never-followed.example").await;
    assert!(
        matches!(stranger, Err(AppError::NotFound)),
        "withdrawing a relay that was never followed is the 404: {stranger:?}"
    );
}
