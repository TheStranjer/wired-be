//! The relay routes: following, unfollowing, the ledger and failures.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::{
    config::TEST_BASE_URL,
    federation::delivery::KIND_DELIVER,
    models::{follow, relay, user::get_by_ap_id},
    services::instance_actor::ensure_instance_actor,
};

use super::support::{
    ALL_ADMIN, MockRelay, RELAY, admin, assert_follow_recorded, assert_relay_withdrawn,
    federation_app, follow_back, post_relay, queued_args,
};
use crate::common;

/// Following a relay through the admin API answers the pair and writes
/// the ledger row, the `relay_follow` entry and the one queued `Follow`;
/// the listing then shows the `followed_back` flag flip once the relay
/// follows the instance actor back, and a restated follow stays one row,
/// one send — answering the current flag.
#[tokio::test]
async fn following_a_relay_through_the_admin_api_walks_the_handshake() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let mock = MockRelay::start().await;
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let followed = post_relay(
        &app,
        &token,
        &mock.actor_ap_id(),
        StatusCode::OK,
        "following a relay",
    )
    .await;
    assert_eq!(
        followed,
        json!({"actor": mock.actor_ap_id(), "followed_back": false}),
        "the answer names the relay and its fresh state"
    );
    assert_follow_recorded(&pool, &mock).await;

    let listed = common::TestRequest::get(RELAY)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&listed, StatusCode::OK, "listing the ledger");
    assert_eq!(
        listed.json_value(),
        json!({"relays": [{"actor": mock.actor_ap_id(), "followed_back": false}]}),
        "one entry, not followed back yet"
    );

    let relay_user = get_by_ap_id(&pool, &mock.actor_ap_id())
        .await
        .expect("the mirror reads")
        .expect("the follow mirrored the relay actor");
    follow_back(&pool, &relay_user).await;
    let back = common::TestRequest::get(RELAY)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        back.json_value()["relays"][0]["followed_back"],
        json!(true),
        "the accepted follow of the instance actor reads back"
    );

    let restated = post_relay(
        &app,
        &token,
        &mock.actor_ap_id(),
        StatusCode::OK,
        "restating the follow",
    )
    .await;
    assert_eq!(
        restated,
        json!({"actor": mock.actor_ap_id(), "followed_back": true}),
        "a restated follow answers the current flag"
    );
    assert_eq!(
        queued_args(&pool, KIND_DELIVER).await.len(),
        1,
        "and the queue still holds one send"
    );
    assert_eq!(
        relay::list(&pool).await.expect("the ledger reads").len(),
        1,
        "and still one subscription"
    );
}

/// Unfollowing through the admin API answers the requested address as a
/// JSON string, clears the `relays` row and the instance actor's
/// `follows` row, logs the withdrawal and queues the `Undo` beside the
/// `Follow` — and a second withdrawal of the same relay is the 400
/// `Not found`.
#[tokio::test]
async fn unfollowing_a_relay_through_the_admin_api_withdraws_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = federation_app(&pool);
    let mock = MockRelay::start().await;
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let _first = post_relay(
        &app,
        &token,
        &mock.actor_ap_id(),
        StatusCode::OK,
        "following the walk's relay",
    )
    .await;
    let relay_user = get_by_ap_id(&pool, &mock.actor_ap_id())
        .await
        .expect("the mirror reads")
        .expect("the follow mirrored the relay actor");
    follow_back(&pool, &relay_user).await;

    let unfollowed = common::TestRequest::delete(RELAY)
        .bearer(&token)
        .json(&json!({"relay_url": mock.actor_ap_id(), "force": true}))
        .send(&app)
        .await;
    common::assert_answer(&unfollowed, StatusCode::OK, "unfollowing a relay");
    assert_eq!(
        unfollowed.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    assert_eq!(
        unfollowed.json::<String>(),
        mock.actor_ap_id(),
        "the requested address, as a JSON string"
    );
    assert_relay_withdrawn(&pool, &relay_user, &mock.actor_ap_id()).await;
    let queued = queued_args(&pool, KIND_DELIVER).await;
    assert_eq!(queued.len(), 2, "the Undo rides beside the Follow");
    assert_eq!(queued[1]["body"]["type"], "Undo", "as an Undo");

    let emptied = common::TestRequest::get(RELAY)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        emptied.json_value(),
        json!({"relays": []}),
        "the ledger reads empty"
    );

    let repeat = common::TestRequest::delete(RELAY)
        .bearer(&token)
        .json(&json!({"relay_url": mock.actor_ap_id()}))
        .send(&app)
        .await;
    common::assert_answer(&repeat, StatusCode::BAD_REQUEST, "an unknown relay");
    common::assert_error(&repeat, "Not found", "the withdrawal of a stranger");

    let ran = common::run_all_jobs(&state).await;
    assert!(ran >= 2, "both deliveries ran");
    let posts = mock.received();
    assert_eq!(posts.len(), 2, "the relay's inbox saw both handshakes");
    assert_eq!(posts[0]["type"], "Follow", "the follow arrived");
    assert_eq!(posts[1]["type"], "Undo", "and the withdrawal");
}

/// The listing's shape over a seeded ledger: one entry per row, oldest
/// subscription first, `followed_back` true only for the relay whose
/// mirror holds an accepted follow of the instance actor — a pending
/// follow and an unmirrored actor both read false.
#[tokio::test]
async fn the_ledger_lists_every_relay_with_its_follow_back_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let (_root, token) = admin(&pool, "root", "admin:read:relays").await;

    let backer = common::create_remote_user(&pool, "relay", "back.example").await;
    let waiter = common::create_remote_user(&pool, "relay", "wait.example").await;
    let stranger = "https://stranger.example/relay";
    let _first = relay::upsert(&pool, &backer.ap_id, "https://back.example/relay/inbox")
        .await
        .expect("the backer's subscription stores");
    let _second = relay::upsert(&pool, stranger, "https://stranger.example/inbox")
        .await
        .expect("the unmirrored relay's subscription stores");
    let _third = relay::upsert(&pool, &waiter.ap_id, "https://wait.example/relay/inbox")
        .await
        .expect("the waiter's subscription stores");
    // Settle the order: back.example oldest, then the stranger, then wait.
    for (ap_id, secs) in [
        (backer.ap_id.as_str(), 120_i32),
        (stranger, 60),
        (waiter.ap_id.as_str(), 1),
    ] {
        let _aged = sqlx::query(
            "UPDATE relays SET inserted_at = now() - make_interval(secs => $2) WHERE actor_ap_id = $1",
        )
        .bind(ap_id)
        .bind(secs)
        .execute(&pool)
        .await
        .expect("the stamps are settable");
    }

    follow_back(&pool, &backer).await;
    let config = common::test_config();
    let instance = ensure_instance_actor(&pool, &config)
        .await
        .expect("the instance actor stands");
    let pending = follow::follow(&pool, TEST_BASE_URL, &waiter, &instance)
        .await
        .expect("the waiter's follow records");
    assert_eq!(
        pending.state,
        follow::FollowState::Pending,
        "a locked target keeps the relay's follow pending"
    );

    let listed = common::TestRequest::get(RELAY)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        listed.json_value(),
        json!({"relays": [
            {"actor": backer.ap_id, "followed_back": true},
            {"actor": stranger, "followed_back": false},
            {"actor": waiter.ap_id, "followed_back": false},
        ]}),
        "one entry per row, oldest first, only the accepted follow-back true"
    );
}

/// The follow refusals, each answered 400 with the service's own
/// message: an address that is neither URL nor host, a body that names
/// no address at all, a `Group` outside the relay shapes, and an actor
/// with nothing behind it.
#[tokio::test]
async fn every_failed_follow_is_a_400_with_its_message() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let mock = MockRelay::start().await;
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let refusals = [
        (
            "nope/nope",
            "`nope/nope` is neither a relay actor URL nor a host",
        ),
        ("", "no relay address was given"),
        (
            &mock.url_at("/group"),
            "the relay actor is a Group, not one of Application, Service or Person",
        ),
        (&mock.url_at("/missing"), "Record not found"),
    ];
    for (address, message) in refusals {
        let answer = common::TestRequest::post(RELAY)
            .bearer(&token)
            .json(&json!({"relay_url": address}))
            .send(&app)
            .await;
        common::assert_answer(&answer, StatusCode::BAD_REQUEST, "a refused follow");
        common::assert_error(&answer, message, &format!("refusing {address:?}"));
    }

    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM relays")
            .fetch_one(&pool)
            .await
            .expect("the ledger reads"),
        0,
        "no refusal left a subscription behind"
    );
}

// --- the instance deletion ---------------------------------------------------------------
