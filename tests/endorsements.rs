//! Integration tests for the endorsements endpoints (WBE-63, the
//! endorsements ticket): `POST /api/v1/accounts/{id}/endorse` and its
//! `/pin` alias demand an accepted follow (422 with the ticket's wording
//! without one) and answer the caller's relationship with `endorsed:
//! true`; `…/unendorse` and `…/unpin` withdraw the pin idempotently;
//! `GET /api/v1/endorsements` lists the caller's own pinned accounts and
//! `GET /api/v1/accounts/{id}/endorsements` (and its `/api/v1/pleroma`
//! twin) list another account's, auth optional — a `hide_follows` account
//! answers the empty array to everyone but itself. All listings page by
//! keyset over the endorsement row id with Pleroma's `Link` header, the
//! `endorsed` flag rides the relationship batch, unfollowing a pinned
//! account clears the endorsement, and every route half of the auth and
//! scope pipelines of the five routes is pinned.

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
    models::{endorsement, follow, user::Flag, user::User},
};

/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = "https://test.example";
/// The 422 body both pin routes answer without an accepted follow (the
/// wording this endpoint's ticket pins).
const NOT_FOLLOWING: &str = r#"{"error":"You must follow this user to endorse them"}"#;
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body of a write whose token lacks `write:accounts`.
const NO_WRITE_SCOPE: &str = r#"{"error":"Insufficient permissions: write:accounts."}"#;
/// The 403 body of a read whose presented token lacks `read:accounts`.
const NO_READ_SCOPE: &str = r#"{"error":"Insufficient permissions: read:accounts."}"#;
/// The 404 body every account route shares.
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 400 body a malformed page cursor gets from the extractor.
const INVALID_ID: &str = r#"{"error":"Invalid id"}"#;

/// The relationship JSON for `target` with nothing recorded about them:
/// every flag at the documented default — `showing_reblogs` true, `note`
/// empty, the rest false.
fn relationship_default(target: &Uuid) -> Value {
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

/// [`relationship_default`] with the flags in `computed` overwritten: the
/// tests name exactly what the write changed.
fn relationship(target: &Uuid, computed: &Value) -> Value {
    let mut value = relationship_default(target);
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

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// Records `follower`'s accepted follow of `target` through the model —
/// the follow-actions ticket owns the HTTP route, so the fixture goes
/// straight to the table (the way the follow-listing tests do).
async fn follow_pair(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        follow::FollowState::Accepted,
        "an open local account answers on the spot"
    );
}

/// A bodyless `POST` to `path` with `token`.
async fn post(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .send(app)
        .await
}

/// A `GET` of `path` with `token` (`None` anonymous).
async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// The pin route of account `target`, one of the four action paths.
fn action_path(target: &Uuid, action: &str) -> String {
    format!("/api/v1/accounts/{target}/{action}")
}

/// The endorsement listing path of account segment `account` under
/// `prefix`, with `query` appended (the segment stays text so the miss
/// tests can feed garbage).
fn list_path(prefix: &str, account: &str, query: &str) -> String {
    format!("{prefix}/accounts/{account}/endorsements{query}")
}

/// The Mastodon and Pleroma listing prefixes this ticket shares.
const LIST_PREFIXES: [&str; 2] = ["/api/v1", "/api/v1/pleroma"];

/// The `id` of every entity of a listing response, in response order.
fn listed_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the listing is an array")
        .iter()
        .map(|account| account["id"].as_str().expect("an account id").to_owned())
        .collect()
}

/// The stored `(id, target_id)` rows of `endorser`'s endorsements, newest
/// first — read from storage so same-millisecond `UUIDv7`s cannot make an
/// expectation depend on insert timing (decision 4).
async fn stored_endorsements(pool: &PgPool, endorser: &User) -> Vec<(Uuid, Uuid)> {
    sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT id, target_id FROM endorsements WHERE user_id = $1 ORDER BY id DESC",
    )
    .bind(endorser.id)
    .fetch_all(pool)
    .await
    .expect("the stored rows read")
}

/// The `Link` header Pleroma's `add_link_headers` writes for a page of
/// `path`: `next` walks to the oldest row, `prev` back to the newest.
fn expected_link(path: &str, oldest: &Uuid, newest: &Uuid) -> String {
    format!(
        "<{BASE}{path}?max_id={oldest}>; rel=\"next\", \
         <{BASE}{path}?min_id={newest}>; rel=\"prev\""
    )
}

/// Pinning demands an accepted follow: without one both action spellings
/// answer the ticket's 422; with one they store the endorsement and
/// answer the full relationship with `endorsed: true`; and re-pinning is
/// one row and the same answer.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn endorse_demands_a_follow_and_answers_the_relationship(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    for action in ["endorse", "pin"] {
        let refused = post(&app, &token, &action_path(&bob.id, action)).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{action}` without a follow is refused"
        );
        assert_eq!(refused.text(), NOT_FOLLOWING, "the ticket's wording");
        assert_eq!(
            stored_endorsements(&pool, &alice).await.len(),
            0,
            "and the refusal wrote nothing"
        );
    }

    follow_pair(&pool, &alice, &bob).await;
    for action in ["endorse", "pin"] {
        let body = post(&app, &token, &action_path(&bob.id, action)).await;
        assert_eq!(body.status, StatusCode::OK, "`{action}` now writes");
        assert_eq!(
            body.header("content-type"),
            Some("application/json"),
            "the relationship is JSON"
        );
        assert_eq!(
            body.json_value(),
            relationship(&bob.id, &json!({"following": true, "endorsed": true})),
            "the full entity with the two true flags, `{action}` alike"
        );
    }
    let rows = stored_endorsements(&pool, &alice).await;
    assert_eq!(rows.len(), 1, "pinning twice keeps one row");
}

/// `endorsed` is computed by the batch loader: the relationships endpoint
/// and the embedded view on the account both carry the stored pin.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn endorsed_rides_the_relationship_reads(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let bob = common::create_local_user(&pool, "bob").await;
    follow_pair(&pool, &alice, &bob).await;
    endorsement::endorse(&pool, alice.id, bob.id)
        .await
        .expect("the seed pin stores");
    let app = common::test_app(pool.clone());

    let batch = get(
        &app,
        Some(&token),
        &format!("/api/v1/accounts/relationships?id%5B%5D={}", bob.id),
    )
    .await;
    assert_eq!(batch.status, StatusCode::OK, "the batch answers");
    assert_eq!(
        batch.json_value()[0]["endorsed"],
        json!(true),
        "the loader reads the pin from the table"
    );

    let embed = get(&app, Some(&token), &format!("/api/v1/accounts/{}", bob.id)).await;
    assert_eq!(
        embed.json_value()["pleroma"]["relationship"]["endorsed"],
        json!(true),
        "and the embedded relationship agrees"
    );
}

/// Withdraw: unpinning a pinned account answers the relationship with the
/// pin off, unpinning one that was never pinned is content with that
/// (200, nothing written), and both action spellings do the same thing.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn unendorse_withdraws_the_pin_and_repeating_is_fine(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let bob = common::create_local_user(&pool, "bob").await;
    follow_pair(&pool, &alice, &bob).await;
    endorsement::endorse(&pool, alice.id, bob.id)
        .await
        .expect("the seed pin stores");
    let app = common::test_app(pool.clone());

    let body = post(&app, &token, &action_path(&bob.id, "unendorse")).await;
    assert_eq!(body.status, StatusCode::OK, "the withdrawal answers");
    assert_eq!(
        body.json_value(),
        relationship(&bob.id, &json!({"following": true})),
        "the pin is off the relationship, the follow stays"
    );

    let again = post(&app, &token, &action_path(&bob.id, "unpin")).await;
    assert_eq!(again.status, StatusCode::OK, "unpin agrees, twice");
    assert_eq!(again.json_value(), body.json_value(), "same answer");
    assert!(
        stored_endorsements(&pool, &alice).await.is_empty(),
        "and no row is left"
    );
}

/// Unfollowing a pinned account takes the pin with it: the relationship
/// batch and the own listing both stop seeing the endorsement the moment
/// the follow row is gone (see [`crate::models::follow::unfollow`]).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn unfollowing_clears_the_endorsement(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let bob = common::create_local_user(&pool, "bob").await;
    follow_pair(&pool, &alice, &bob).await;
    endorsement::endorse(&pool, alice.id, bob.id)
        .await
        .expect("the seed pin stores");
    let removed = follow::unfollow(&pool, &alice, &bob)
        .await
        .expect("the unfollow runs");
    assert!(removed, "the accepted follow was there to take back");
    let app = common::test_app(pool.clone());

    let own = get(&app, Some(&token), "/api/v1/endorsements").await;
    assert_eq!(own.status, StatusCode::OK, "the own listing answers");
    assert_eq!(own.json_value(), json!([]), "the pin went with the follow");

    let batch = get(
        &app,
        Some(&token),
        &format!("/api/v1/accounts/relationships?id%5B%5D={}", bob.id),
    )
    .await;
    assert_eq!(
        batch.json_value()[0],
        relationship_default(&bob.id),
        "the unfollow cleared the endorsement with the follow: every flag \
         is back at its default"
    );
}

/// A pending request is not a follow: pinning an account that has not
/// answered is the same 422, and a self-pin never passes either.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_pending_follow_and_a_self_pin_are_refused(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let carol = common::create_local_user_with(&pool, "carol", |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await;
    let requested = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
        .await
        .expect("the request to the locked account records");
    assert_eq!(
        requested.state,
        follow::FollowState::Pending,
        "the fixture waits"
    );
    let app = common::test_app(pool);

    let refused = post(&app, &token, &action_path(&carol.id, "endorse")).await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a request nobody answered is not a follow"
    );
    assert_eq!(refused.text(), NOT_FOLLOWING, "the ticket's wording");

    let yourself = post(&app, &token, &action_path(&alice.id, "pin")).await;
    assert_eq!(
        yourself.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "nobody follows themselves, so nobody pins themselves"
    );
    assert_eq!(yourself.text(), NOT_FOLLOWING, "the same refusal");
}

/// The malformed, unknown and deactivated id is the shared 404 on every
/// action route — and it outranks the follow check: an unknown id is
/// never the 422.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn actions_require_a_live_account(pool: PgPool) {
    let (_alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let ghost = common::create_deactivated_user_with_password(&pool, "ghost", "password123").await;
    let app = common::test_app(pool);

    for action in ["endorse", "pin", "unendorse", "unpin"] {
        let garbage = common::TestRequest::post(&format!("/api/v1/accounts/not-an-id/{action}"))
            .bearer(&token)
            .send(&app)
            .await;
        assert_eq!(
            garbage.status,
            StatusCode::NOT_FOUND,
            "a malformed id is a 404 on `{action}`"
        );
        assert_eq!(garbage.text(), NOT_FOUND, "the shared body");

        let missing = post(&app, &token, &action_path(&Uuid::now_v7(), action)).await;
        assert_eq!(
            missing.status,
            StatusCode::NOT_FOUND,
            "an unknown id is a 404 on `{action}`"
        );
        assert_eq!(missing.text(), NOT_FOUND, "the shared body");

        let suspended = post(&app, &token, &action_path(&ghost.id, action)).await;
        assert_eq!(
            suspended.status,
            StatusCode::NOT_FOUND,
            "and a deactivated account is a 404 on `{action}`"
        );
        assert_eq!(suspended.text(), NOT_FOUND, "the shared body");
    }
}

/// The pin routes demand a user token: no token is the 401, an invalid
/// one the 401, and an app-only credential the 403.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn actions_require_a_user_token(pool: PgPool) {
    let bob = common::create_local_user(&pool, "bob").await;
    let app_only = common::create_app_only_token(&pool, "write:accounts").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::post(&action_path(&bob.id, "endorse"))
        .send(&app)
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "no token");
    assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

    let invalid = common::TestRequest::post(&action_path(&bob.id, "pin"))
        .bearer("bogus-token")
        .send(&app)
        .await;
    assert_eq!(invalid.status, StatusCode::UNAUTHORIZED, "bad token");
    assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");

    let app_token = post(&app, &app_only, &action_path(&bob.id, "unendorse")).await;
    assert_eq!(app_token.status, StatusCode::FORBIDDEN, "app-only");
    assert_eq!(app_token.text(), USER_REQUIRED, "the user-required body");
}

/// The pin routes' scope is `write:accounts`: the root `write` and the
/// exact sub-scope pass (they reach the follow check, here the 422 —
/// nobody follows the fresh `bob`), anything else is the 403 at the gate,
/// ahead of the account lookup and the follow check.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn actions_require_the_write_accounts_scope(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let ghost = common::create_deactivated_user_with_password(&pool, "ghost", "password123").await;
    let mut passing = Vec::new();
    for scopes in ["write", "write:accounts"] {
        passing.push(common::create_app_and_token(&pool, &alice, scopes).await);
    }
    let mut failing = Vec::new();
    for scopes in ["read", "read:accounts", "write:follows", "follow"] {
        failing.push(common::create_app_and_token(&pool, &alice, scopes).await);
    }
    let app = common::test_app(pool);

    for token in &passing {
        let body = post(&app, token, &action_path(&bob.id, "endorse")).await;
        assert_eq!(
            body.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a granted token reaches the follow check"
        );
    }
    for token in &failing {
        let body = post(&app, token, &action_path(&bob.id, "endorse")).await;
        assert_eq!(
            body.status,
            StatusCode::FORBIDDEN,
            "an ungranted token is refused at the gate"
        );
        assert_eq!(body.text(), NO_WRITE_SCOPE, "the denial names the scope");

        let missing = post(&app, token, &action_path(&ghost.id, "endorse")).await;
        assert_eq!(
            missing.status,
            StatusCode::FORBIDDEN,
            "the gate runs ahead of the 404"
        );
    }
}

/// `GET /api/v1/endorsements` answers the caller's own pins, newest first,
/// as full account entities embedding the caller's relationship with
/// `endorsed: true` on it; a remote mirror pins like a local account; the
/// endorser with nothing pinned gets the empty array and no `Link` header.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn own_endorsements_lists_the_callers_pins(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_remote_user(&pool, "carol", "remote.example").await;
    follow_pair(&pool, &alice, &bob).await;
    endorsement::endorse(&pool, alice.id, bob.id).await.unwrap();
    endorsement::endorse(&pool, alice.id, carol.id)
        .await
        .unwrap();
    let stored = stored_endorsements(&pool, &alice).await;
    let mallory = common::create_local_user(&pool, "mallory").await;
    let outsider = common::create_app_and_token(&pool, &mallory, "read:accounts").await;
    let app = common::test_app(pool);

    let body = get(&app, Some(&token), "/api/v1/endorsements").await;
    assert_eq!(body.status, StatusCode::OK, "the listing answers");
    assert_eq!(
        body.header("content-type"),
        Some("application/json"),
        "the entities are JSON"
    );
    assert_eq!(
        listed_ids(&body.json_value()),
        stored
            .iter()
            .map(|(_, target)| target.to_string())
            .collect::<Vec<_>>(),
        "both pins, newest endorsement first"
    );
    assert_eq!(
        body.header("link"),
        Some(expected_link(
            "/api/v1/endorsements",
            &stored[1].0,
            &stored[0].0
        ))
        .as_deref(),
        "the Link header cursors are the endorsement row ids"
    );
    let entries = body.json_value();
    let entry = entries
        .as_array()
        .expect("two pins")
        .iter()
        .find(|account| account["id"] == json!(bob.id.to_string()))
        .expect("bob is listed");
    assert_eq!(
        entry["acct"], "bob",
        "an entry is a full account entity, not an id stub"
    );
    assert_eq!(
        entry["pleroma"]["relationship"]["endorsed"],
        json!(true),
        "the entry embeds the caller's relationship, pinned"
    );

    let empty = get(&app, Some(&outsider), "/api/v1/endorsements").await;
    assert_eq!(empty.status, StatusCode::OK, "an unpinned account lists");
    assert_eq!(empty.json_value(), json!([]), "the empty array");
    assert_eq!(empty.header("link"), None, "and no Link header");
}

/// The own listing demands a user token with `read:accounts`: the
/// extractor's 401/403 half, and the 403 of a presented user token that
/// grants something narrower.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn own_endorsements_requires_read_accounts_of_a_user(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let narrow = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app_only = common::create_app_only_token(&pool, "read:accounts").await;
    let app = common::test_app(pool);

    let anonymous = get(&app, None, "/api/v1/endorsements").await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "no token");
    assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

    let invalid = get(&app, Some("bogus-token"), "/api/v1/endorsements").await;
    assert_eq!(invalid.status, StatusCode::UNAUTHORIZED, "bad token");
    assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");

    let app_token = get(&app, Some(&app_only), "/api/v1/endorsements").await;
    assert_eq!(app_token.status, StatusCode::FORBIDDEN, "app-only");
    assert_eq!(app_token.text(), USER_REQUIRED, "the user-required body");

    let denied = get(&app, Some(&narrow), "/api/v1/endorsements").await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "scope is enforced");
    assert_eq!(denied.text(), NO_READ_SCOPE, "the denial names the scope");
}

/// The page walks by endorsement row id: `limit` cuts the newest rows,
/// the `Link` cursors continue where the page ended (`max_id` older,
/// `min_id` newer, still answered newest-first), a walk off the end is
/// the empty array, and a malformed cursor is the extractor's 400.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn own_endorsements_pages_by_endorsement_id(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "read:accounts").await;
    for nickname in ["one", "two", "three"] {
        let target = common::create_local_user(&pool, nickname).await;
        endorsement::endorse(&pool, alice.id, target.id)
            .await
            .expect("the seed pin stores (the model does not demand a follow)");
    }
    let stored = stored_endorsements(&pool, &alice).await;
    assert_eq!(stored.len(), 3, "three fixture pins");
    let app = common::test_app(pool);

    let first = get(&app, Some(&token), "/api/v1/endorsements?limit=2").await;
    assert_eq!(
        listed_ids(&first.json_value()),
        stored[..2]
            .iter()
            .map(|(_, target)| target.to_string())
            .collect::<Vec<_>>(),
        "the two newest rows"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link(
            "/api/v1/endorsements",
            &stored[1].0,
            &stored[0].0
        ))
        .as_deref(),
        "the page links its own cursors"
    );

    let older = get(
        &app,
        Some(&token),
        &format!("/api/v1/endorsements?max_id={}", stored[1].0),
    )
    .await;
    assert_eq!(
        listed_ids(&older.json_value()),
        vec![stored[2].1.to_string()],
        "max_id continues where the page ended"
    );

    let newer = get(
        &app,
        Some(&token),
        &format!("/api/v1/endorsements?min_id={}", stored[2].0),
    )
    .await;
    assert_eq!(
        listed_ids(&newer.json_value()),
        stored[..2]
            .iter()
            .map(|(_, target)| target.to_string())
            .collect::<Vec<_>>(),
        "a min_id page is answered newest-first too"
    );

    let tail = get(
        &app,
        Some(&token),
        &format!("/api/v1/endorsements?max_id={}", stored[2].0),
    )
    .await;
    assert_eq!(
        tail.status,
        StatusCode::OK,
        "walking off the end is no error"
    );
    assert_eq!(tail.json_value(), json!([]), "and answers nothing");
    assert_eq!(tail.header("link"), None, "with no cursor to link");

    let garbage = get(&app, Some(&token), "/api/v1/endorsements?max_id=nope").await;
    assert_eq!(
        garbage.status,
        StatusCode::BAD_REQUEST,
        "a malformed cursor is the extractor's 400"
    );
    assert_eq!(garbage.text(), INVALID_ID, "the shared body");
}

/// `GET /api/v1/accounts/{id}/endorsements` and its
/// `/api/v1/pleroma` twin answer the same page to anyone, including
/// without a token: full account entities, newest first, with the
/// prefix's own `Link` path.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn account_endorsements_are_public_on_both_prefixes(pool: PgPool) {
    let bob = common::create_local_user(&pool, "bob").await;
    let pinned = common::create_local_user(&pool, "pinned").await;
    endorsement::endorse(&pool, bob.id, pinned.id)
        .await
        .expect("the seed pin stores");
    let stored = stored_endorsements(&pool, &bob).await;
    let (_alice, alice_token) = fixture(&pool, "alice", "read:accounts").await;
    let bob_token = common::create_app_and_token(&pool, &bob, "read:accounts").await;
    let app = common::test_app(pool);

    for prefix in LIST_PREFIXES {
        let path = list_path(prefix, &bob.id.to_string(), "");
        for viewer in [None, Some(alice_token.as_str())] {
            let body = get(&app, viewer, &path).await;
            assert_eq!(body.status, StatusCode::OK, "auth is optional");
            assert_eq!(
                listed_ids(&body.json_value()),
                vec![pinned.id.to_string()],
                "the target's pin, `{prefix}` alike"
            );
        }
        let anonymous = get(&app, None, &path).await;
        assert_eq!(
            anonymous.json_value()[0]["pleroma"]["relationship"],
            json!({}),
            "an anonymous listing embeds no relationship"
        );
        assert_eq!(
            anonymous.header("link"),
            Some(expected_link(&path, &stored[0].0, &stored[0].0)).as_deref(),
            "the Link header targets the request's own prefix"
        );
    }

    let self_body = get(
        &app,
        Some(&bob_token),
        &list_path("/api/v1", &bob.id.to_string(), ""),
    )
    .await;
    assert_eq!(
        self_body.json_value()[0]["pleroma"]["relationship"]["endorsed"],
        json!(true),
        "the account itself sees its relationship on the entries"
    );
}

/// The miss pipeline of the account listings: a malformed, unknown or
/// deactivated id is the shared 404 on both prefixes, and a presented
/// token that does not resolve is the 401, never a silent anonymous read.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn account_endorsements_misses_are_404_and_401(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let narrow = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let ghost = common::create_deactivated_user_with_password(&pool, "ghost", "password123").await;
    let app = common::test_app(pool);

    for prefix in LIST_PREFIXES {
        let garbage = get(&app, None, &list_path(prefix, "not-an-id", "")).await;
        assert_eq!(garbage.status, StatusCode::NOT_FOUND, "a malformed id");
        assert_eq!(garbage.text(), NOT_FOUND, "the shared body");

        let missing = get(
            &app,
            None,
            &list_path(prefix, &Uuid::now_v7().to_string(), ""),
        )
        .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "an unknown id");
        assert_eq!(missing.text(), NOT_FOUND, "the shared body");

        let suspended = get(&app, None, &list_path(prefix, &ghost.id.to_string(), "")).await;
        assert_eq!(suspended.status, StatusCode::NOT_FOUND, "a ghost");
        assert_eq!(suspended.text(), NOT_FOUND, "the shared body");

        let invalid = get(
            &app,
            Some("bogus-token"),
            &list_path(prefix, &alice.id.to_string(), ""),
        )
        .await;
        assert_eq!(invalid.status, StatusCode::UNAUTHORIZED, "bad token");
        assert_eq!(invalid.text(), INVALID_TOKEN, "the 401 body");

        let denied = get(
            &app,
            Some(&narrow),
            &list_path(prefix, &alice.id.to_string(), ""),
        )
        .await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "a presented user token must grant read:accounts"
        );
        assert_eq!(denied.text(), NO_READ_SCOPE, "the denial names the scope");

        let cursors = get(
            &app,
            None,
            &list_path(prefix, &alice.id.to_string(), "?max_id=nope"),
        )
        .await;
        assert_eq!(cursors.status, StatusCode::BAD_REQUEST, "bad cursor");
        assert_eq!(cursors.text(), INVALID_ID, "the extractor's body");
    }
}

/// The privacy half: a `hide_follows` account answers the empty array to
/// everyone but itself — anonymous, another account's token — while the
/// `hide_follows_count` flag alone, and the account's own view, keep the
/// list intact. Both prefixes, since both route the same handler.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn hidden_follows_hides_another_accounts_endorsements(pool: PgPool) {
    let secret = common::create_local_user_with(&pool, "secret", |mut new| {
        new.hide_follows = Flag::new(true);
        new
    })
    .await;
    let (_viewer, viewer_token) = fixture(&pool, "viewer", "read:accounts").await;
    let owner_token = common::create_app_and_token(&pool, &secret, "read:accounts").await;
    let pinned = common::create_local_user(&pool, "pinned").await;
    endorsement::endorse(&pool, secret.id, pinned.id)
        .await
        .expect("the seed pin stores");
    // The count-hiding flag alone does not hide the list.
    let counts_only = common::create_local_user_with(&pool, "counts_only", |mut new| {
        new.hide_follows_count = Flag::new(true);
        new
    })
    .await;
    endorsement::endorse(&pool, counts_only.id, pinned.id)
        .await
        .expect("the second seed pin stores");
    let app = common::test_app(pool);

    for prefix in LIST_PREFIXES {
        let path = list_path(prefix, &secret.id.to_string(), "");
        for viewer in [None, Some(viewer_token.as_str())] {
            let hidden = get(&app, viewer, &path).await;
            assert_eq!(hidden.status, StatusCode::OK, "hidden is still 200");
            assert_eq!(hidden.json_value(), json!([]), "the empty array");
            assert_eq!(hidden.header("link"), None, "no cursor to link");
        }
        let own = get(&app, Some(&owner_token), &path).await;
        assert_eq!(
            listed_ids(&own.json_value()),
            vec![pinned.id.to_string()],
            "the account itself always sees its pins"
        );
    }

    let visible = get(
        &app,
        Some(&viewer_token),
        &list_path("/api/v1", &counts_only.id.to_string(), ""),
    )
    .await;
    assert_eq!(
        listed_ids(&visible.json_value()),
        vec![pinned.id.to_string()],
        "hiding the follower count does not hide the pins"
    );
}
