//! Integration tests for `POST /api/v1/accounts/{id}/note` (WBE-62, the
//! account-notes ticket): set, overwrite and clear round-trip through the
//! answered [`Relationship`]; a blank comment deletes the stored note; a
//! 2001-character comment is Mastodon's 422 while exactly 2000 still
//! stores; the note rides on the relationship the batch endpoint and the
//! account embed answer, and on nothing anyone else's reads are — notes
//! are per viewer; a self-note is allowed; unknown ids are the shared
//! 404; and every route half of the `AuthUser` + `write:accounts`
//! pipeline is pinned.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::user::User;

/// The endpoint's path for `target`.
fn note_path(target: &Uuid) -> String {
    format!("/api/v1/accounts/{target}/note")
}

/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body `require` renders for a token without the scope.
const NO_SCOPE: &str = r#"{"error":"Insufficient permissions: write:accounts."}"#;
/// The 422 body for a comment over the limit (Mastodon's changeset wording).
const TOO_LONG: &str = "Validation failed: Comment is too long (maximum is 2000 characters)";
/// The 404 body every account route shares.
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;

/// The full relationship JSON for `target` with nothing recorded about
/// them: every flag at the documented default — `showing_reblogs` true,
/// `note` empty, the rest false.
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

/// [`expected_default`] with the flags in `computed` overwritten: the
/// tests name exactly what the write changed.
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

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// `POST …/note` with `comment` as a JSON body (Mastodon clients may also
/// send it form-encoded; [`note_form`] covers that side).
async fn note_json(
    app: &Router,
    token: &str,
    target: &Uuid,
    comment: &str,
) -> common::TestResponse {
    common::TestRequest::post(&note_path(target))
        .bearer(token)
        .json(&json!({ "comment": comment }))
        .send(app)
        .await
}

/// `POST …/note` with `comment` as an urlencoded form body.
async fn note_form(
    app: &Router,
    token: &str,
    target: &Uuid,
    comment: &str,
) -> common::TestResponse {
    common::TestRequest::post(&note_path(target))
        .bearer(token)
        .form(&[("comment", comment)])
        .send(app)
        .await
}

/// Fire a seeding note write and drop the response; the write itself is the
/// fixture, and the assertions live on the later reads.
async fn seed_note(app: &Router, token: &str, target: &Uuid, comment: &str) {
    let seeded = note_json(app, token, target, comment).await;
    assert_eq!(seeded.status, StatusCode::OK, "the seed write answers 200");
}

/// The note the caller's relationship batch reports for `target`.
async fn noted_relationship(app: &Router, token: &str, target: &Uuid) -> Value {
    let body =
        common::TestRequest::get(&format!("/api/v1/accounts/relationships?id%5B%5D={target}"))
            .bearer(token)
            .send(app)
            .await;
    assert_eq!(body.status, StatusCode::OK, "the batch answers");
    body.json_value()
        .get(0)
        .cloned()
        .expect("the known target answers one row")
}

/// Set, overwrite and clear: each write answers the relationship with the
/// note it left stored, and the next read agrees.
#[tokio::test]
async fn note_round_trips_set_overwrite_and_clear() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    // `read write` so the same token can also read the relationship back
    // through `GET …/relationships` at the end.
    let (_alice, token) = fixture(&pool, "alice", "read write").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool);

    let set = note_json(&app, &token, &bob.id, "met at a conference").await;
    assert_eq!(set.status, StatusCode::OK, "the first write answers");
    assert_eq!(
        set.header("content-type"),
        Some("application/json"),
        "the relationship is JSON"
    );
    assert_eq!(
        set.json_value(),
        expected(&bob.id, &json!({"note": "met at a conference"})),
        "the full entity with the stored note on it"
    );

    let form = note_form(&app, &token, &bob.id, "team lead").await;
    assert_eq!(form.status, StatusCode::OK, "the form body writes too");
    assert_eq!(
        form.json_value(),
        expected(&bob.id, &json!({"note": "team lead"})),
        "the overwrite replaces the comment in place"
    );

    let cleared = note_json(&app, &token, &bob.id, "").await;
    assert_eq!(cleared.status, StatusCode::OK, "the clear answers");
    assert_eq!(
        cleared.json_value(),
        expected_default(&bob.id),
        "the empty comment deleted the row"
    );
    assert_eq!(
        noted_relationship(&app, &token, &bob.id).await["note"],
        json!(""),
        "and a later read still sees no note"
    );
}

/// A blank (all-whitespace) comment clears like the empty string, and an
/// over-long comment is 422 — before the account lookup, and without
/// touching the stored note. Exactly 2000 characters store.
#[tokio::test]
async fn blank_clears_and_too_long_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read write").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool);

    seed_note(&app, &token, &bob.id, "keep this").await;

    let blank = note_json(&app, &token, &bob.id, " \n\t ").await;
    assert_eq!(blank.status, StatusCode::OK, "a whitespace note writes");
    assert_eq!(
        blank.json_value(),
        expected_default(&bob.id),
        "and blanked the stored comment away"
    );

    seed_note(&app, &token, &bob.id, "keep this").await;
    let over = "x".repeat(2001);
    let rejected = note_json(&app, &token, &bob.id, &over).await;
    assert_eq!(
        rejected.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "2001 is too long"
    );
    assert_eq!(
        rejected.text(),
        json!({"error": TOO_LONG}).to_string(),
        "Mastodon's wording"
    );
    assert_eq!(
        noted_relationship(&app, &token, &bob.id).await["note"],
        json!("keep this"),
        "the rejected write left the stored note alone"
    );

    let unknown = Uuid::now_v7();
    let misplaced = note_json(&app, &token, &unknown, &over).await;
    assert_eq!(
        misplaced.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation runs ahead of the lookup: an over-long comment to an \
         unknown id is still the 422"
    );

    let edge = "é".repeat(2000);
    let accepted = note_json(&app, &token, &bob.id, &edge).await;
    assert_eq!(accepted.status, StatusCode::OK, "exactly 2000 stores");
    assert_eq!(
        accepted.json_value()["note"],
        json!(edge),
        "the cap counts characters, not bytes"
    );

    let missing = common::TestRequest::post(&note_path(&bob.id))
        .bearer(&token)
        .json(&json!({}))
        .send(&app)
        .await;
    assert_eq!(
        missing.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an absent comment is the extractor's 422"
    );
}

/// The note the account endpoints render: `…/relationships` answers it,
/// `GET /api/v1/accounts/{id}` embeds it in `pleroma.relationship`, and
/// another user reading the same account sees the empty default — notes
/// are per viewer.
#[tokio::test]
async fn relationship_and_embed_show_the_note_per_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, alice_token) = fixture(&pool, "alice", "read write").await;
    let (_carol, carol_token) = fixture(&pool, "carol", "read write").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool);

    seed_note(&app, &alice_token, &bob.id, "alice's view").await;

    assert_eq!(
        noted_relationship(&app, &alice_token, &bob.id).await,
        expected(&bob.id, &json!({"note": "alice's view"})),
        "the caller's own batch carries the note"
    );

    let embed = common::TestRequest::get(&format!("/api/v1/accounts/{}", bob.id))
        .bearer(&alice_token)
        .send(&app)
        .await;
    assert_eq!(embed.status, StatusCode::OK, "the account answers");
    assert_eq!(
        embed.json_value()["pleroma"]["relationship"],
        expected(&bob.id, &json!({"note": "alice's view"})),
        "and embeds the viewer's note"
    );

    assert_eq!(
        noted_relationship(&app, &carol_token, &bob.id).await,
        expected_default(&bob.id),
        "another viewer of the same target sees the empty note"
    );
}

/// Noting yourself is allowed: the answered relationship is the default
/// flags with the self-note on them.
#[tokio::test]
async fn a_self_note_is_allowed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let app = common::test_app(pool);

    let body = note_json(&app, &token, &alice.id, "own reminder").await;
    assert_eq!(body.status, StatusCode::OK, "the self write answers");
    assert_eq!(
        body.json_value(),
        expected(&alice.id, &json!({"note": "own reminder"})),
        "with the note set and every other flag at its default"
    );
}

/// An id that is malformed, unknown or a deactivated account is the 404
/// every account route shares.
#[tokio::test]
async fn note_requires_a_live_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let ghost = common::create_deactivated_user_with_password(&pool, "ghost", "password123").await;
    let app = common::test_app(pool);

    let malformed = common::TestRequest::post("/api/v1/accounts/not-an-id/note")
        .bearer(&token)
        .json(&json!({"comment": "x"}))
        .send(&app)
        .await;
    assert_eq!(
        malformed.status,
        StatusCode::NOT_FOUND,
        "a malformed id is a 404"
    );
    assert_eq!(malformed.text(), NOT_FOUND, "the shared body");

    let unknown = Uuid::now_v7();
    let missing = note_json(&app, &token, &unknown, "x").await;
    assert_eq!(
        missing.status,
        StatusCode::NOT_FOUND,
        "an unknown id is a 404"
    );
    assert_eq!(missing.text(), NOT_FOUND, "the shared body");

    let suspended = note_json(&app, &token, &ghost.id, "x").await;
    assert_eq!(
        suspended.status,
        StatusCode::NOT_FOUND,
        "and a deactivated account is a 404"
    );
    assert_eq!(suspended.text(), NOT_FOUND, "the shared body");
}

/// The endpoint demands a user token: no token is the 401, an invalid one
/// the 401, and an app-only credential the 403.
#[tokio::test]
async fn note_requires_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_local_user(&pool, "bob").await;
    let app_only = common::create_app_only_token(&pool, "write:accounts").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::post(&note_path(&bob.id))
        .json(&json!({"comment": "x"}))
        .send(&app)
        .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token cannot write a note"
    );
    assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

    let invalid = note_json(&app, "bogus-token", &bob.id, "x").await;
    assert_eq!(
        invalid.status,
        StatusCode::UNAUTHORIZED,
        "an invalid token is not treated as anonymous"
    );
    assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");

    let app_token = note_json(&app, &app_only, &bob.id, "x").await;
    assert_eq!(
        app_token.status,
        StatusCode::FORBIDDEN,
        "an app-only token is not a user"
    );
    assert_eq!(app_token.text(), USER_REQUIRED, "the user-required body");
}

/// The scope gate is Pleroma's `write:accounts`: the root `write` and the
/// exact sub-scope pass; anything else is the insufficient-permissions 403.
#[tokio::test]
async fn note_requires_the_write_accounts_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_local_user(&pool, "bob").await;
    let alice = common::create_local_user(&pool, "alice").await;
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
        let body = note_json(&app, token, &bob.id, "granted").await;
        assert_eq!(
            body.status,
            StatusCode::OK,
            "a broader or exact grant passes"
        );
    }
    for token in &failing {
        let body = note_json(&app, token, &bob.id, "denied").await;
        assert_eq!(
            body.status,
            StatusCode::FORBIDDEN,
            "a narrower or unrelated grant does not"
        );
        assert_eq!(body.text(), NO_SCOPE, "the denial names the scope");
    }
}
