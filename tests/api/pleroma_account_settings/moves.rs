//! The Pleroma `move_account` route over HTTP: the follower transfer, the
//! federated `Move`, the gates, the cooldown and the auth and scope checks.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::federation::publisher;

use super::support::{
    ALIASES, PASSWORD, UNRESOLVABLE, aliased_target, assert_carried, assert_move_queued,
    assert_stays, fixture, four_followers, get_aliases, move_account, queued_jobs, stored,
};
use crate::common;

/// The full move: gates pass, the account is marked moved with its
/// cooldown stamp, every local follower that allows the move follows the
/// target (accepted on the spot, the target is local and unlocked) and
/// unfollows the origin — while opted-out and remote followers stay —
/// and the counters end where the relationships are.
#[tokio::test]
async fn move_transfers_local_followers_and_marks_the_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let mut bob = aliased_target(&pool, "bob", &alice).await;
    let zoe = common::create_local_user(&pool, "zoe").await;
    // A target that moved on itself: the render must not follow it (see
    // the entity assertions below).
    let _retargeted = sqlx::query("UPDATE users SET moved_to_ap_id = $2 WHERE id = $1")
        .bind(bob.id)
        .bind(&zoe.ap_id)
        .execute(&pool)
        .await
        .expect("the target's own move stamps in");
    bob = stored(&pool, bob.id).await;

    let (carol, dave, erin, frank) = four_followers(&pool, &alice).await;

    let app = common::test_app(pool.clone());
    let response = move_account(&app, &token, PASSWORD, "bob").await;
    assert_eq!(response.status, StatusCode::OK, "the move answers");
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "the documented success body"
    );

    let moved = stored(&pool, alice.id).await;
    assert_eq!(
        moved.moved_to_ap_id.as_deref(),
        Some(bob.ap_id.as_str()),
        "the account records its move target"
    );
    assert!(
        moved.last_move_at.is_some(),
        "and the cooldown stamp the next attempt reads"
    );

    for follower in [&carol, &dave] {
        assert_carried(&pool, follower, &alice, &bob).await;
    }
    for stay in [&erin, &frank] {
        assert_stays(&pool, stay, &alice, &bob).await;
    }
    assert_eq!(
        (
            stored(&pool, alice.id).await.followers_count,
            stored(&pool, bob.id).await.followers_count
        ),
        (2, 2),
        "two followers left the origin, two arrived at the target"
    );

    // The entity: the account's `moved` key is now the target's nested
    // account, one level deep.
    let show = common::TestRequest::get(&format!("/api/v1/accounts/{}", alice.id))
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        show["moved"]["id"],
        json!(bob.id.to_string()),
        "moved renders the target's Account entity"
    );
    assert_eq!(show["moved"]["acct"], json!("bob"), "with its handle");
    assert_eq!(
        show["moved"]["moved"],
        Value::Null,
        "the nesting is non-recursive: the target's own move renders null"
    );

    // The federation half: the move is announced as a `Move`.
    assert_move_queued(&pool, &alice, &bob).await;
}

/// A refused move federates nothing: no gate that answers before the
/// write is reached queues a publish job.
#[tokio::test]
async fn a_refused_move_announces_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let _bob = aliased_target(&pool, "bob", &alice).await;
    let app = common::test_app(pool.clone());

    let wrong = move_account(&app, &token, "not-my-password", "bob").await;
    assert_eq!(wrong.status, StatusCode::OK, "the refusal body");
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Invalid password."}),
        "the password refusal"
    );
    assert!(
        queued_jobs(&pool, publisher::KIND_PUBLISH).await.is_empty(),
        "a refused move announces nothing"
    );
}

/// The gates, in Pleroma's order and with Pleroma's bodies: the wrong
/// password is a 200 error, an unresolvable target the endpoint's own
/// 404, and a target that has not listed the origin refuses with the
/// `alsoKnownAs` message. None of them touches the account.
#[tokio::test]
async fn move_refuses_at_every_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let _bob = aliased_target(&pool, "bob", &alice).await;
    let _stranger = common::create_local_user(&pool, "stranger").await;
    let app = common::test_app(pool.clone());

    let wrong = move_account(&app, &token, "not-my-password", "bob").await;
    assert_eq!(wrong.status, StatusCode::OK, "Pleroma answers 200");
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Invalid password."}),
        "the password refusal"
    );

    let missing = move_account(&app, &token, PASSWORD, UNRESOLVABLE).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "404");
    assert_eq!(
        missing.json_value(),
        json!({"error": "Target account not found."}),
        "the endpoint's own not-found message, period included"
    );

    let unaliased = move_account(&app, &token, PASSWORD, "stranger").await;
    assert_eq!(unaliased.status, StatusCode::OK, "Pleroma answers 200");
    assert_eq!(
        unaliased.json_value(),
        json!({"error": "Target account must have the origin in `alsoKnownAs`"}),
        "the target has not listed the origin"
    );

    let alice = stored(&pool, alice.id).await;
    assert!(
        alice.moved_to_ap_id.is_none() && alice.last_move_at.is_none(),
        "no refusal marked the account moved"
    );
}

/// A second move inside the window is refused with the cooldown message;
/// after 31 days the same account may move again.
#[tokio::test]
async fn move_is_limited_to_once_per_thirty_days() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let first = aliased_target(&pool, "first_target", &alice).await;
    let second = aliased_target(&pool, "second_target", &alice).await;
    let app = common::test_app(pool.clone());

    let ok = move_account(&app, &token, PASSWORD, "first_target").await;
    assert_eq!(
        ok.json_value(),
        json!({"status": "success"}),
        "the first move succeeds"
    );

    let throttled = move_account(&app, &token, PASSWORD, "second_target").await;
    assert_eq!(throttled.status, StatusCode::OK, "Pleroma answers 200");
    assert_eq!(
        throttled.json_value(),
        json!({"error": "You can't move your account more than once in 30 days"}),
        "the cooldown refusal, and it runs after the alias gate"
    );
    assert_eq!(
        stored(&pool, alice.id).await.moved_to_ap_id.as_deref(),
        Some(first.ap_id.as_str()),
        "the refused attempt kept the original target"
    );

    let _aged: sqlx::postgres::PgQueryResult =
        sqlx::query("UPDATE users SET last_move_at = now() - interval '31 days' WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the backdate runs");
    let later = move_account(&app, &token, PASSWORD, "second_target").await;
    assert_eq!(
        later.json_value(),
        json!({"status": "success"}),
        "outside the window the account may move again"
    );
    assert_eq!(
        stored(&pool, alice.id).await.moved_to_ap_id.as_deref(),
        Some(second.ap_id.as_str()),
        "and the target updates"
    );
}

/// Every route requires a user token, `GET` the `read:accounts` scope
/// and the mutations `write:accounts`: no token is the extractor's 401,
/// a narrow or app-only credential one of the 403s, and a dead token
/// the invalid-token 401.
#[tokio::test]
async fn every_route_enforces_auth_and_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let read_only = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let wrong_scope = common::create_app_and_token(&pool, &alice, "read:chats").await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool.clone());

    let anonymous = get_aliases(&app, "").await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "no token");
    assert_eq!(
        anonymous.json_value(),
        json!({"error": "Invalid credentials"}),
        "the AuthUser no-token body"
    );

    let lapsed = common::TestRequest::get(ALIASES)
        .bearer("not-a-stored-token")
        .send(&app)
        .await;
    assert_eq!(lapsed.status, StatusCode::UNAUTHORIZED, "an invalid token");

    let read_gets = get_aliases(&app, &read_only).await;
    assert_eq!(
        read_gets.status,
        StatusCode::OK,
        "read:accounts may list aliases"
    );
    for verb in [
        common::TestRequest::put(ALIASES),
        common::TestRequest::delete(ALIASES),
    ] {
        let denied = verb
            .bearer(&read_only)
            .json(&json!({"alias": "x"}))
            .send(&app)
            .await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "read:accounts is not enough to change aliases"
        );
        assert_eq!(
            denied.json_value(),
            json!({"error": "Insufficient permissions: write:accounts."}),
            "the scope denial names the missing grant"
        );
    }

    let wrong = get_aliases(&app, &wrong_scope).await;
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Insufficient permissions: read:accounts."}),
        "read:chats reaches neither read nor write of accounts"
    );
    let moved = move_account(&app, &wrong_scope, PASSWORD, "whoever").await;
    assert_eq!(moved.status, StatusCode::FORBIDDEN, "and the move gate");

    let app_token = get_aliases(&app, &app_only).await;
    assert_eq!(app_token.status, StatusCode::FORBIDDEN, "an app token");
    assert_eq!(
        app_token.json_value(),
        json!({"error": "This API requires an authenticated user"}),
        "is refused the user endpoints"
    );
}
