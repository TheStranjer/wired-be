//! The subscribe and unsubscribe routes over HTTP: the granted follow they
//! need, the flags they write and the relationship they answer.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::follow::{self, Follow, FollowState};

use super::support::{
    NOT_FOLLOWING, asking, expected_relationship, following, locked_user, relationship_batch,
    stored, subscription,
};
use crate::common;

/// Subscribing needs a granted follow of the target: no relationship at all,
/// a request nobody answered, and the other direction of a relationship are
/// the same 422 with the ticket's wording, and none of them writes anything.
#[tokio::test]
async fn subscribe_requires_an_accepted_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = locked_user(&pool, "carol").await;
    let erin = locked_user(&pool, "erin").await;
    let _waiting: Follow = asking(&pool, &alice, &carol).await;
    // bob follows alice, which is bob's subscription to have, not alice's.
    let _incoming: Follow = following(&pool, &bob, &alice).await;
    let token = common::create_app_and_token(&pool, &alice, "follow").await;
    let app = common::test_app(pool.clone());

    // erin: a live account alice never asked to follow; carol: a request
    // nobody answered; bob: the other end of a granted follow.
    for target in [erin.id, carol.id, bob.id] {
        let refused = subscription(&app, &token, &target, false).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "no accepted follow of {target} means no subscription"
        );
        assert_eq!(
            refused.header("content-type"),
            Some("application/json"),
            "errors are JSON"
        );
        assert_eq!(refused.text(), NOT_FOLLOWING, "the ticket's refusal body");
    }

    assert!(
        !stored(&pool, &alice, &carol).await.notify,
        "the pending row keeps its stored flag"
    );
    assert!(
        !stored(&pool, &bob, &alice).await.notify,
        "and so does the incoming relationship"
    );
    assert!(
        follow::get(&pool, alice.id, erin.id)
            .await
            .unwrap()
            .is_none(),
        "nothing was created for the account that was never asked about"
    );
    assert_eq!(
        stored(&pool, &alice, &carol).await.state,
        FollowState::Pending,
        "the refused write neither answered the request nor changed its row"
    );
    assert_eq!(
        follow::pending_count(&pool, carol.id).await.unwrap(),
        1,
        "the request is still in the queue it belongs to"
    );
}

/// A successful subscribe answers the caller's full `Relationship` with both
/// `notifying` and its `subscribing` alias `true`, stores the flag on the
/// follow row, leaves everything else about the relationship alone, and reads
/// back the same way from the Mastodon relationship batch; subscribing twice
/// is idempotent.
#[tokio::test]
async fn subscribe_sets_both_flags_and_answers_the_relationship() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let granted = following(&pool, &alice, &bob).await;
    let token = common::create_app_and_token(&pool, &alice, "write:follows").await;
    let read_token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app = common::test_app(pool.clone());

    let response = subscription(&app, &token, &bob.id, false).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the subscription goes through"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the relationship is JSON"
    );
    assert_eq!(
        response.json_value(),
        expected_relationship(
            &bob.id,
            &json!({
                "following": true,
                "notifying": true,
                "subscribing": true,
            })
        ),
        "the caller's view of the target: following, and subscribed on both \
         aliases, with every other flag at its default"
    );

    let updated = stored(&pool, &alice, &bob).await;
    assert!(updated.notify, "the follows column holds the subscription");
    assert_eq!(updated.id, granted.id, "the same row, not a new one");
    assert_eq!(
        updated.state,
        FollowState::Accepted,
        "subscribing does not change the relationship's state"
    );
    assert!(updated.show_reblogs, "and leaves the reblog toggle alone");
    assert!(
        updated.updated_at >= updated.inserted_at,
        "the write refreshes the row's timestamp"
    );
    assert_eq!(
        (
            common::reload_user(&pool, bob.id).await.followers_count,
            common::reload_user(&pool, alice.id).await.following_count
        ),
        (1, 1),
        "subscribing moves no counters: the follow already counted"
    );
    assert_eq!(
        follow::pending_count(&pool, bob.id).await.unwrap(),
        0,
        "no request was created underneath"
    );

    let batch = relationship_batch(&app, &read_token, &bob.id).await;
    assert_eq!(batch["notifying"], true, "the batch read sees the flag");
    assert_eq!(batch["subscribing"], true, "under its alias too");

    let twice = subscription(&app, &token, &bob.id, false).await;
    assert_eq!(twice.status, StatusCode::OK, "subscribing twice is allowed");
    assert_eq!(
        twice.json_value(),
        expected_relationship(
            &bob.id,
            &json!({
                "following": true,
                "notifying": true,
                "subscribing": true,
            })
        ),
        "and answers the same relationship"
    );
}

/// Unsubscribing clears the same column and answers both flags `false`, the
/// caller stays following, and the write needs the same accepted follow the
/// subscribe route does (a relationship the caller never granted is the 422,
/// an id that names no account is the shared 404).
#[tokio::test]
async fn unsubscribe_clears_the_flag_again() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = locked_user(&pool, "carol").await;
    let _granted: Follow = following(&pool, &alice, &bob).await;
    let _waiting: Follow = asking(&pool, &alice, &carol).await;
    let token = common::create_app_and_token(&pool, &alice, "follow").await;
    let read_token = common::create_app_and_token(&pool, &alice, "read").await;
    let app = common::test_app(pool.clone());

    let subscribed = subscription(&app, &token, &bob.id, false).await;
    assert_eq!(subscribed.status, StatusCode::OK, "the fixture subscribes");
    assert!(
        stored(&pool, &alice, &bob).await.notify,
        "and the flag is stored"
    );

    let response = subscription(&app, &token, &bob.id, true).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the un-subscription goes through"
    );
    assert_eq!(
        response.json_value(),
        expected_relationship(
            &bob.id,
            &json!({
                "following": true,
                "notifying": false,
                "subscribing": false,
            })
        ),
        "both flags are off again, while the follow itself stands"
    );
    assert!(
        !stored(&pool, &alice, &bob).await.notify,
        "the follows column holds the answer"
    );
    assert_eq!(
        stored(&pool, &alice, &bob).await.state,
        FollowState::Accepted,
        "and the relationship is still granted"
    );

    let batch = relationship_batch(&app, &read_token, &bob.id).await;
    assert_eq!(batch["notifying"], false, "the batch read agrees");
    assert_eq!(batch["subscribing"], false, "on both aliases");

    let twice = subscription(&app, &token, &bob.id, true).await;
    assert_eq!(
        twice.status,
        StatusCode::OK,
        "unsubscribing twice is allowed"
    );
    assert_eq!(
        twice.json_value()["subscribing"],
        false,
        "and answers the stored flag again"
    );

    let refused = subscription(&app, &token, &carol.id, true).await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a request nobody answered cannot be un-subscribed either"
    );
    assert_eq!(refused.text(), NOT_FOLLOWING, "the same refusal body");
    assert!(
        !stored(&pool, &alice, &carol).await.notify,
        "and nothing was written"
    );
}
