//! The access gates of the subscription and outgoing routes: tokens, scopes,
//! unknown accounts and malformed cursors.

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;
use wired_be::models::follow::{self, Follow, FollowState};

use super::support::{
    INVALID_TOKEN, NO_READ_SCOPE, NO_TOKEN, NO_WRITE_SCOPE, NOT_FOUND, USER_REQUIRED, asking,
    deactivated_user, following, locked_user, outgoing, raw_subscription, subscribe_path,
    subscription, subscription_verb, unsubscribe_path,
};
use crate::common;

/// The subscription routes and the outgoing listing demand a user token: no
/// token is the 401 "Invalid credentials", an invalid one the 401 "The
/// access token is invalid", and an app-only credential the 403 — before any
/// scope or id is ever looked at. (The favourites route is the optional-auth
/// exception, tested in
/// `the_favourites_gate_demands_the_scope_of_a_presented_user_token`.)
#[tokio::test]
async fn every_route_requires_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app_only = common::create_app_only_token(&pool, "follow").await;
    let app = common::test_app(pool);
    let subscribe = subscribe_path(&alice.id);
    let unsubscribe = unsubscribe_path(&alice.id);
    let posts = [subscribe.as_str(), unsubscribe.as_str()];

    for path in posts {
        let anonymous = common::TestRequest::post(path).send(&app).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "no token cannot write {path}"
        );
        assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

        let invalid = common::TestRequest::post(path)
            .bearer("bogus-token")
            .send(&app)
            .await;
        assert_eq!(
            invalid.status,
            StatusCode::UNAUTHORIZED,
            "an invalid token is not treated as anonymous"
        );
        assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");

        let app_token = common::TestRequest::post(path)
            .bearer(&app_only)
            .send(&app)
            .await;
        assert_eq!(
            app_token.status,
            StatusCode::FORBIDDEN,
            "an app-only token is not a user"
        );
        assert_eq!(app_token.text(), USER_REQUIRED, "the user-required body");
    }

    let anonymous = outgoing(&app, None, "").await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token cannot list the asks"
    );
    assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");
    let invalid = outgoing(&app, Some("bogus-token"), "").await;
    assert_eq!(
        invalid.status,
        StatusCode::UNAUTHORIZED,
        "and the listing demands a real one"
    );
    assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");
    let app_token = outgoing(&app, Some(&app_only), "").await;
    assert_eq!(
        app_token.status,
        StatusCode::FORBIDDEN,
        "the listing refuses app-only credentials alike"
    );
    assert_eq!(app_token.text(), USER_REQUIRED, "the user-required body");
}

/// The scope gates: the two writes take anything covering `follow` or
/// `write:follows`, the listing anything covering `read:follows`; everything
/// else is Pleroma's insufficient-permissions 403, naming the missing scopes
/// the way each route's plug does.
#[tokio::test]
async fn the_routes_gate_on_the_follow_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _granted: Follow = following(&pool, &alice, &bob).await;
    let carol = locked_user(&pool, "carol").await;
    let mut waiting = asking(&pool, &alice, &carol).await;
    let app = common::test_app(pool.clone());

    for scopes in ["follow", "write:follows", "write"] {
        let good = common::create_app_and_token(&pool, &alice, scopes).await;
        for unsubscribe in [false, true] {
            let action = subscription_verb(unsubscribe);
            let response = subscription(&app, &good, &bob.id, unsubscribe).await;
            assert_eq!(response.status, StatusCode::OK, "{scopes} may {action}");
            assert_eq!(
                response.json_value()["subscribing"],
                Value::from(!unsubscribe),
                "and the write goes through, {action}"
            );
        }
    }
    for scopes in ["read:follows", "read", "write:accounts"] {
        let bad = common::create_app_and_token(&pool, &alice, scopes).await;
        for unsubscribe in [false, true] {
            let denied = subscription(&app, &bad, &bob.id, unsubscribe).await;
            assert_eq!(
                denied.status,
                StatusCode::FORBIDDEN,
                "{scopes} does not write subscriptions"
            );
            assert_eq!(
                denied.text(),
                NO_WRITE_SCOPE,
                "the denial joins both scopes"
            );
        }
    }

    for scopes in ["read", "read:follows", "follow"] {
        let good = common::create_app_and_token(&pool, &alice, scopes).await;
        let listed = outgoing(&app, Some(&good), "").await;
        assert_eq!(listed.status, StatusCode::OK, "{scopes} reads the asks");
        // `follow` is one of the listing's alternatives too, and the write it
        // grants is what takes the fixture ask out of the list again.
        let answered = follow::accept(&pool, waiting.id).await.expect("granted");
        assert_eq!(
            answered.state,
            FollowState::Accepted,
            "the ask is granted for the next round"
        );
        let empty = outgoing(&app, Some(&good), "").await;
        assert_eq!(
            empty.json_value(),
            json!([]),
            "{scopes} now reads an answered ask as gone"
        );
        // Take the answer back for the next round: rejecting the granted row
        // removes it, and a fresh request waits again.
        let withdrawn = follow::reject(&pool, answered.id).await.expect("withdrawn");
        assert_eq!(
            withdrawn.state,
            FollowState::Accepted,
            "the granted ask is withdrawn for {scopes}"
        );
        waiting = asking(&pool, &alice, &carol).await;
    }
    for scopes in ["write:follows", "write", "read:accounts"] {
        let bad = common::create_app_and_token(&pool, &alice, scopes).await;
        let denied = outgoing(&app, Some(&bad), "").await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "{scopes} does not read the asks"
        );
        assert_eq!(
            denied.text(),
            NO_READ_SCOPE,
            "the denial names read:follows"
        );
    }
}

/// Every account miss on the two writes is the same 404: an id that is not
/// uuid text, an id that names no user, and a deactivated account — and the
/// refusal comes before the follow check, so a caller who is not following
/// the id still sees the 404.
#[tokio::test]
async fn every_unknown_account_is_the_same_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let ghost = deactivated_user(&pool, "ghost").await;
    let token = common::create_app_and_token(&pool, &alice, "follow").await;
    let missing = Uuid::now_v7().to_string();
    let app = common::test_app(pool.clone());

    for action in ["subscribe", "unsubscribe"] {
        for target in [missing.as_str(), "not-an-id", &ghost.id.to_string()] {
            let response = raw_subscription(&app, &token, target, action).await;
            assert_eq!(
                response.status,
                StatusCode::NOT_FOUND,
                "`{target}` names no account, {action}"
            );
            assert_eq!(response.text(), NOT_FOUND, "decision 8's 404 body");
        }
    }
    assert_eq!(
        follow::get(&pool, alice.id, ghost.id)
            .await
            .unwrap()
            .map(|row| row.notify),
        None,
        "the refused writes stored no relationship with the deactivated account"
    );
}

/// A malformed cursor is the page parser's 400 with its message, and auth
/// still runs first: a bad token alongside a bad cursor is the 401.
#[tokio::test]
async fn a_malformed_cursor_is_a_bad_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app = common::test_app(pool);

    for query in ["?max_id=nope", "?min_id=01890b9a-nope", "?since_id=x%2y"] {
        let response = outgoing(&app, Some(&token), query).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "`{query}` is refused before any listing"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Invalid id"}),
            "with the page parser's message"
        );
    }

    let denied = outgoing(&app, Some("bogus-token"), "?max_id=nope").await;
    assert_eq!(
        denied.status,
        StatusCode::UNAUTHORIZED,
        "the 401 comes before the 400"
    );
}
