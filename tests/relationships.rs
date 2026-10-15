//! Integration tests for `GET /api/v1/accounts/relationships`: the batched
//! relationship flags in every follow state, the requested order with
//! unknown and invalid ids skipped, the bracketed array and single `id=`
//! parameter forms, the empty array for no usable ids, the `read:follows`
//! scope and user-token auth in front of it all — plus the
//! `pleroma.relationship` key `GET /api/v1/accounts/{id}` and `…/lookup`
//! embed for a stranger viewer (and keep `{}` anonymously and for the
//! account's own viewer).

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowState},
        user::{Flag, User},
    },
};

/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body `require` renders for a token without the scope.
const NO_SCOPE: &str = r#"{"error":"Insufficient permissions: read:follows."}"#;

/// `GET /api/v1/accounts/relationships{query}` with `token` as bearer.
async fn relationships(app: &Router, token: &str, query: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/api/v1/accounts/relationships{query}"))
        .bearer(token)
        .send(app)
        .await
}

/// The full relationship JSON for `target` with nothing recorded about
/// them: every flag at the ticket's documented default —
/// `showing_reblogs` true, `note` empty, the rest false.
fn expected_default(target: &Uuid) -> Value {
    json!({
        "id": target.to_string(),
        "following": false,
        "requested": false,
        "endorsed": false,
        "followed_by": false,
        "blocking": false,
        "blocked_by": false,
        "muting": false,
        "muting_notifications": false,
        "notifying": false,
        "showing_reblogs": true,
        "domain_blocking": false,
        "note": "",
        "subscribing": false,
    })
}

/// [`expected_default`] with the follow-derived flags in `computed`
/// overwritten: the object form keeps the expectation helpers to two
/// parameters and lets each test name exactly the flags it asserts.
fn expected(target: &Uuid, computed: &Value) -> Value {
    let mut value = expected_default(target);
    let base = value.as_object_mut().expect("the default is an object");
    let overrides = computed.as_object().expect("the flags are a JSON object");
    for (key, flag) in overrides {
        let replaced = base.insert(key.clone(), flag.clone());
        assert!(
            replaced.is_some(),
            "`{key}` overrides a documented default, not a typo"
        );
    }
    value
}

/// Records `follower`'s follow of `target` through the model, asserting
/// it lands in the `wanted` state — the fixture every test below reads.
async fn follow_as(pool: &PgPool, follower: &User, target: &User, wanted: FollowState) {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state, wanted,
        "the fixture follow is in the state the test expects"
    );
}

/// Stores the non-default toggles (`show_reblogs = false`, `notify = true`)
/// on `follower`'s row, the way the follow-actions ticket's endpoints will:
/// the model itself only ever writes the column defaults.
async fn flip_toggles(pool: &PgPool, follower: &Uuid, target: &Uuid) {
    let updated = sqlx::query(
        "UPDATE follows SET show_reblogs = false, notify = true
          WHERE follower_id = $1 AND followee_id = $2",
    )
    .bind(follower)
    .bind(target)
    .execute(pool)
    .await
    .expect("the toggle update runs");
    assert_eq!(
        updated.rows_affected(),
        1,
        "the fixture row is there to flip"
    );
}

/// Every computed flag in each follow state — accepted with flipped
/// toggles, pending, incoming-only, mutual — as one batched request.
#[tokio::test]
async fn relationships_answer_every_follow_state() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user_with(&pool, "carol", |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await;
    let dave = common::create_local_user(&pool, "dave").await;
    let erin = common::create_local_user(&pool, "erin").await;
    // alice → bob, accepted with both toggles flipped.
    follow_as(&pool, &alice, &bob, FollowState::Accepted).await;
    flip_toggles(&pool, &alice.id, &bob.id).await;
    // alice → carol, waiting on the locked account.
    follow_as(&pool, &alice, &carol, FollowState::Pending).await;
    // dave → alice, the incoming side.
    follow_as(&pool, &dave, &alice, FollowState::Accepted).await;
    // alice ↔ erin, both directions granted.
    follow_as(&pool, &alice, &erin, FollowState::Accepted).await;
    follow_as(&pool, &erin, &alice, FollowState::Accepted).await;
    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app = common::test_app(pool);

    let body = relationships(
        &app,
        &token,
        &format!(
            "?id%5B%5D={}&id%5B%5D={}&id%5B%5D={}&id%5B%5D={}",
            bob.id, carol.id, dave.id, erin.id
        ),
    )
    .await;
    assert_eq!(body.status, StatusCode::OK, "the batch answers");
    assert_eq!(
        body.header("content-type"),
        Some("application/json"),
        "the array is JSON"
    );
    let value = body.json_value();
    let rows = value.as_array().expect("a JSON array of relationships");
    assert_eq!(rows.len(), 4, "four ids in, four relationships out");
    assert_eq!(
        rows[0],
        expected(
            &bob.id,
            &json!({
                "following": true,
                "notifying": true,
                "subscribing": true,
                "showing_reblogs": false,
            })
        ),
        "the accepted follow: following, with the row's flipped toggles \
         and Pleroma's subscribing alias riding on notifying"
    );
    assert_eq!(
        rows[1],
        expected(&carol.id, &json!({"requested": true})),
        "the pending request: requested, not following"
    );
    assert_eq!(
        rows[2],
        expected(&dave.id, &json!({"followed_by": true})),
        "the incoming follow: followed_by only"
    );
    assert_eq!(
        rows[3],
        expected(&erin.id, &json!({"following": true, "followed_by": true})),
        "the mutual follow: both directions at once"
    );
}

/// The requested order is kept, unknown and invalid ids are skipped, and a
/// repeated id is answered twice.
#[tokio::test]
async fn relationships_keep_the_order_and_skip_unknown_and_invalid_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    follow_as(&pool, &alice, &bob, FollowState::Accepted).await;
    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app = common::test_app(pool);
    let unknown = Uuid::now_v7();

    let body = relationships(
        &app,
        &token,
        &format!(
            "?id%5B%5D={unknown}&id%5B%5D={}&id=not-an-id&id%5B%5D={}&id%5B%5D={}",
            carol.id, bob.id, bob.id
        ),
    )
    .await;
    assert_eq!(body.status, StatusCode::OK, "the mixed batch answers");
    let value = body.json_value();
    let rows = value.as_array().expect("a JSON array");
    let answers: Vec<String> = rows
        .iter()
        .map(|row| {
            format!(
                "{}:{}",
                row["id"].as_str().expect("ids are strings"),
                row["following"].as_bool().expect("flags are bools")
            )
        })
        .collect();
    assert_eq!(
        answers,
        vec![
            format!("{}:false", carol.id),
            format!("{}:true", bob.id),
            format!("{}:true", bob.id),
        ],
        "the unknown id and the garbage value are skipped, the order is the \
         request's, and a repeated id answers twice with its own flags"
    );
}

/// The single `id=` form — Pleroma's `List.wrap` — answers the one row.
#[tokio::test]
async fn relationships_accept_the_single_id_form() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    follow_as(&pool, &alice, &bob, FollowState::Accepted).await;
    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app = common::test_app(pool);

    let body = relationships(&app, &token, &format!("?id={}", bob.id)).await;
    assert_eq!(body.status, StatusCode::OK, "the single form answers");
    assert_eq!(
        body.json_value(),
        json!([expected(&bob.id, &json!({"following": true}))]),
        "one id in, the one relationship out"
    );
}

/// No `id`, an empty `id`, only-garbage ids and an empty array parameter
/// are all the empty array, never an error (Pleroma's `json(conn, [])`).
#[tokio::test]
async fn relationships_without_usable_ids_answer_the_empty_array() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app = common::test_app(pool);

    for query in ["", "?", "?id=", "?id%5B%5D=", "?id=nope", "?other="] {
        let body = relationships(&app, &token, query).await;
        assert_eq!(
            body.status,
            StatusCode::OK,
            "`{query:?}` is empty, not missing-parameter"
        );
        assert_eq!(body.json_value(), json!([]), "`{query:?}` answers []");
    }
}

/// The endpoint demands a user token: no token is the 401, an invalid one
/// the 401, and an app-only credential the 403.
#[tokio::test]
async fn relationships_require_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app_only = common::create_app_only_token(&pool, "read:follows").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::get("/api/v1/accounts/relationships")
        .send(&app)
        .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token cannot read relationships"
    );
    assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

    let invalid = common::TestRequest::get("/api/v1/accounts/relationships?id=x")
        .bearer("bogus-token")
        .send(&app)
        .await;
    assert_eq!(
        invalid.status,
        StatusCode::UNAUTHORIZED,
        "an invalid token is not treated as anonymous"
    );
    assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");

    let app_token = common::TestRequest::get("/api/v1/accounts/relationships")
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

/// The scope gate is Pleroma's `read:follows`: broader `read`, the `follow`
/// root and the exact scope pass; anything else fails with the
/// insufficient-permissions 403.
#[tokio::test]
async fn relationships_require_the_read_follows_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mut passing = Vec::new();
    for scopes in ["read", "follow", "read:follows"] {
        passing.push(common::create_app_and_token(&pool, &alice, scopes).await);
    }
    let mut failing = Vec::new();
    for scopes in ["read:accounts", "write:follows", "write"] {
        failing.push(common::create_app_and_token(&pool, &alice, scopes).await);
    }
    let app = common::test_app(pool);

    for token in &passing {
        let body = relationships(&app, token, "?id=").await;
        assert_eq!(
            body.status,
            StatusCode::OK,
            "a broader or exact grant covers read:follows"
        );
    }
    for token in &failing {
        let body = relationships(&app, token, "?id=").await;
        assert_eq!(
            body.status,
            StatusCode::FORBIDDEN,
            "a narrower or unrelated grant does not"
        );
        assert_eq!(body.text(), NO_SCOPE, "the denial names the scope");
    }
}

/// The account entity embeds the viewer's relationship in
/// `pleroma.relationship` when the viewer is looking at someone else.
#[tokio::test]
async fn show_embeds_the_relationship_for_a_stranger_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    follow_as(&pool, &alice, &bob, FollowState::Accepted).await;
    follow_as(&pool, &bob, &alice, FollowState::Accepted).await;
    let alice_token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);

    let stranger = common::TestRequest::get(&format!("/api/v1/accounts/{}", bob.id))
        .bearer(&alice_token)
        .send(&app)
        .await;
    assert_eq!(
        stranger.status,
        StatusCode::OK,
        "the viewer sees the account"
    );
    assert_eq!(
        stranger.json_value()["pleroma"]["relationship"],
        expected(&bob.id, &json!({"following": true, "followed_by": true})),
        "show embeds the viewer's full relationship for another account"
    );
}

/// The same row seen anonymously and seen by its own user keeps the empty
/// object Pleroma renders without a reading user — and the owner has no
/// self-relationship to see either.
#[tokio::test]
async fn show_keeps_the_empty_relationship_without_a_stranger() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    follow_as(&pool, &alice, &bob, FollowState::Accepted).await;
    let bob_token = common::create_app_and_token(&pool, &bob, "read:accounts").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::get(&format!("/api/v1/accounts/{}", bob.id))
        .send(&app)
        .await;
    assert_eq!(
        anonymous.json_value()["pleroma"]["relationship"],
        json!({}),
        "an anonymous show has no relationship to embed"
    );

    let own = common::TestRequest::get(&format!("/api/v1/accounts/{}", bob.id))
        .bearer(&bob_token)
        .send(&app)
        .await;
    assert_eq!(
        own.json_value()["pleroma"]["relationship"],
        json!({}),
        "the account's own view embeds nothing: no self relationship"
    );
}

/// `lookup` renders for its viewer exactly as `show` does, so it embeds
/// the relationship through the same path.
#[tokio::test]
async fn lookup_embeds_the_relationship() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    follow_as(&pool, &alice, &bob, FollowState::Accepted).await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);

    let body = common::TestRequest::get("/api/v1/accounts/lookup?acct=bob")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(body.status, StatusCode::OK, "the lookup answers");
    assert_eq!(
        body.json_value()["pleroma"]["relationship"],
        expected(&bob.id, &json!({"following": true})),
        "lookup embeds the viewer's relationship the same way"
    );
}
