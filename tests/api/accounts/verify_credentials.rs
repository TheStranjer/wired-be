//! `GET /api/v1/accounts/verify_credentials`: the owner view and its
//! `source` block, the pending follow-request count and the auth gates.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowState},
        user::Flag,
    },
};

use super::support::{
    BASE, INVALID_TOKEN, NO_SCOPE, NO_TOKEN, USER_REQUIRED, assert_identity, owner_view, source_of,
    verify_credentials,
};
use crate::common;

/// `verify_credentials` answers the caller's own account as they see it,
/// with the `source` block built from the stored settings: the raw note and
/// posting defaults, the owner-only `pleroma` keys, the top-level
/// `also_known_as` list, and the hidden counters visible.
#[tokio::test]
async fn verify_credentials_renders_the_owner_view_with_its_source() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = owner_view(&pool).await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);
    let response = verify_credentials(&app, &token).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the account's own token answers"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the entity is JSON"
    );
    let body = response.json_value();
    assert_identity(
        &body,
        &alice,
        "alice",
        "alice",
        &format!("{BASE}/users/alice"),
    );
    assert_eq!(
        body["note"], "<p>hi <strong>there</strong></p>",
        "the public note keeps rendering the sanitized HTML"
    );
    assert_eq!(
        body["followers_count"], 5,
        "a count hidden from strangers is the owner's own"
    );
    assert_eq!(
        body["pleroma"]["is_admin"], true,
        "show_role: false hides the role from others, never from the owner"
    );
    assert_eq!(
        body["pleroma"]["settings_store"],
        json!({"pleroma_fe": {"theme": "dark"}}),
        "the owner-only keys of the pleroma block are in the answer"
    );
    assert_eq!(
        body["pleroma"]["relationship"],
        json!({}),
        "an account has no relationship to itself to embed"
    );
    assert_eq!(
        body["also_known_as"],
        json!(["https://old.example/users/alice"]),
        "the own view carries the alias list as a top-level key"
    );
    assert_eq!(
        source_of(&body),
        json!({
            "privacy": "unlisted",
            "sensitive": true,
            "language": "eo",
            "note": "hi **there**",
            "fields": [],
            "follow_requests_count": 0,
            "pleroma": {
                "show_role": false,
                "no_rich_text": true,
                "discoverable": true,
                "actor_type": "Person",
            },
        }),
        "the source block is the account's own editing state: the raw note, \
         the stored posting defaults and the four Pleroma settings"
    );
}

/// The columns an account never edited answer the block's documented
/// defaults: public, not sensitive, no language, no note, no fields and an
/// empty request queue.
#[tokio::test]
async fn verify_credentials_source_defaults_are_the_column_defaults() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);
    let body = verify_credentials(&app, &token).await.json_value();
    assert_eq!(
        source_of(&body),
        json!({
            "privacy": "public",
            "sensitive": false,
            "language": null,
            "note": "",
            "fields": [],
            "follow_requests_count": 0,
            "pleroma": {
                "show_role": true,
                "no_rich_text": false,
                "discoverable": false,
                "actor_type": "Person",
            },
        }),
        "a freshly registered account has edited none of it"
    );
}

/// The `source` fields are the text an owner edits, not the markup a
/// stranger reads: the entity renders the `fields` jsonb as stored and the
/// source block round-trips the `raw_fields` jsonb beside it.
#[tokio::test]
async fn verify_credentials_source_fields_are_the_raw_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.fields = json!([
            {"name": "site", "value": "<p><a href=\"https://alice.example\" rel=\"ugc nofollow noopener noreferrer\">alice.example</a></p>"},
            {"name": "motto", "value": "<p><b>beep</b></p>"},
        ]);
        new.raw_fields = json!([
            {"name": "site", "value": "https://alice.example"},
            {"name": "motto", "value": "<b>beep</b>"},
        ]);
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);
    let body = verify_credentials(&app, &token).await.json_value();
    assert_eq!(
        body["fields"],
        json!([
            {"name": "site", "value": "<p><a href=\"https://alice.example\" rel=\"ugc nofollow noopener noreferrer\">alice.example</a></p>", "verified_at": null},
            {"name": "motto", "value": "<p><b>beep</b></p>", "verified_at": null},
        ]),
        "the public entity keeps rendering the stored markup"
    );
    assert_eq!(
        source_of(&body)["fields"],
        json!([
            {"name": "site", "value": "https://alice.example", "verified_at": null},
            {"name": "motto", "value": "<b>beep</b>", "verified_at": null},
        ]),
        "the source block hands back the raw text stored beside the markup"
    );
}

/// `follow_requests_count` is the queue waiting for this account: incoming
/// pending requests only — not an answered one, not a request this account
/// sent somewhere else — and it shrinks as the account answers it.
#[tokio::test]
async fn verify_credentials_counts_the_pending_follow_requests() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_local_user_with(&pool, "carol", |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await;
    let erin = common::create_local_user_with(&pool, "erin", |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await;
    let one = common::create_local_user(&pool, "one").await;
    let two = common::create_local_user(&pool, "two").await;
    let dave = common::create_local_user(&pool, "dave").await;

    let mut waiting = Vec::new();
    for follower in [&one, &two] {
        let queued = follow::follow(&pool, TEST_BASE_URL, follower, &carol)
            .await
            .expect("the request is recorded");
        assert_eq!(
            queued.state,
            FollowState::Pending,
            "a locked account answers its own requests"
        );
        waiting.push(queued.id);
    }
    let granted = follow::follow(&pool, TEST_BASE_URL, &dave, &carol)
        .await
        .expect("the third request is recorded");
    let answered = follow::accept(&pool, granted.id)
        .await
        .expect("one request is answered");
    assert_eq!(
        answered.state,
        FollowState::Accepted,
        "the answered one is out of the queue"
    );
    let outgoing = follow::follow(&pool, TEST_BASE_URL, &carol, &erin)
        .await
        .expect("carol asks to follow erin too");
    assert_eq!(
        outgoing.state,
        FollowState::Pending,
        "and hers waits in erin's queue"
    );

    let token = common::create_app_and_token(&pool, &carol, "read:accounts").await;
    let carol_pool = pool.clone();
    let app = common::test_app(pool);
    let body = verify_credentials(&app, &token).await.json_value();
    assert_eq!(
        source_of(&body)["follow_requests_count"],
        2,
        "the two requests still waiting for carol, neither the answered one \
         nor her own outgoing request"
    );

    let carol_answered = follow::accept(&carol_pool, waiting[0])
        .await
        .expect("carol answers one");
    assert_eq!(
        carol_answered.state,
        FollowState::Accepted,
        "and the answered request leaves the queue the count reads"
    );
    let after = verify_credentials(&app, &token).await.json_value();
    assert_eq!(
        source_of(&after)["follow_requests_count"],
        1,
        "answering a request takes it out of the count"
    );
}

/// A token that does not grant `read:accounts` is a 403 naming the scope,
/// even when it is a valid user token.
#[tokio::test]
async fn verify_credentials_without_the_scope_is_forbidden() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool);
    let response = verify_credentials(&app, &token).await;
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "a valid token outside the scope is refused"
    );
    assert_eq!(response.text(), NO_SCOPE, "the denial names the scope");
}

/// The broad `read` scope covers `read:accounts` (decision 9), so a client
/// that never asked for the sub-scope still gets its own account.
#[tokio::test]
async fn verify_credentials_accepts_a_broad_read_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read").await;
    let app = common::test_app(pool);
    let response = verify_credentials(&app, &token).await;
    assert_eq!(response.status, StatusCode::OK, "`read` grants the check");
    let body = response.json_value();
    assert_eq!(
        body["id"],
        alice.id.to_string(),
        "and answers the token's own account"
    );
    assert!(
        source_of(&body).get("privacy").is_some(),
        "with the source block attached"
    );
}

/// An app-only (`client_credentials`) token has no account to describe: 403
/// from the extractor, before the scope is even read.
#[tokio::test]
async fn verify_credentials_requires_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let token = common::create_app_only_token(&pool, "read:accounts").await;
    let app = common::test_app(pool);
    let response = verify_credentials(&app, &token).await;
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "an app-only token is not a user"
    );
    assert_eq!(response.text(), USER_REQUIRED, "AuthUser's wording");
}

/// No token at all is the 401 [`AuthUser`] owes, and a token that does not
/// resolve is the invalid-credential 401: the endpoint never answers
/// anonymously.
#[tokio::test]
async fn verify_credentials_without_valid_credentials_is_unauthorized() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);
    let anonymous = common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .send(&app)
        .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "the endpoint requires auth"
    );
    assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

    let lapsed = common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer("bogus-token")
        .send(&app)
        .await;
    assert_eq!(
        lapsed.status,
        StatusCode::UNAUTHORIZED,
        "a token that resolves to nothing is refused too"
    );
    assert_eq!(lapsed.text(), INVALID_TOKEN, "and says which");
}
