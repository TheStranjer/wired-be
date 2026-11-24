//! `GET /api/v1/accounts/lookup`: handle resolution, the `q` alias, the
//! `resolve` arm and the auth rules.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::user::{Flag, User};

use super::support::{
    BASE, INVALID_TOKEN, MISSING_ACCT, NO_SCOPE, NOT_FOUND, assert_identity, lookup,
};
use crate::common;

/// `lookup` renders for its viewer exactly as `show` does: the same token
/// reveals the private state, another account's token does not.
#[tokio::test]
async fn lookup_renders_for_its_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.followers_count = 5;
        new.hide_followers_count = Flag::new(true);
        new.settings_store = json!({"note": "mine"});
        new
    })
    .await;
    let own_token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let mallory = common::create_local_user(&pool, "mallory").await;
    let stranger_token = common::create_app_and_token(&pool, &mallory, "read:accounts").await;
    let app = common::test_app(pool);

    let own = common::TestRequest::get("/api/v1/accounts/lookup?acct=alice")
        .bearer(&own_token)
        .send(&app)
        .await;
    assert_eq!(own.status, StatusCode::OK, "the owner may look up herself");
    let own_body = own.json_value();
    assert_eq!(
        own_body["followers_count"], 5,
        "lookup keeps the owner's counters visible"
    );
    assert_eq!(
        own_body["pleroma"]["settings_store"],
        json!({"note": "mine"}),
        "the owner sees the stored settings blob"
    );

    let stranger = common::TestRequest::get("/api/v1/accounts/lookup?acct=alice")
        .bearer(&stranger_token)
        .send(&app)
        .await;
    assert_eq!(
        stranger.status,
        StatusCode::OK,
        "so may another account look her up"
    );
    let stranger_body = stranger.json_value();
    assert_eq!(
        stranger_body["followers_count"], 0,
        "and the hidden count stays hidden"
    );
    assert!(
        stranger_body["pleroma"].get("settings_store").is_none(),
        "the settings blob is not public"
    );
    assert!(
        stranger_body["pleroma"].get("birthday").is_none(),
        "and neither is a birthday the account does not show"
    );
}

/// `acct` without a domain finds the local account by stored nickname.
#[tokio::test]
async fn lookup_by_plain_nickname() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);
    let response = lookup(&app, "?acct=alice").await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the stored nickname resolves"
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
}

/// A local handle qualified with this instance's own domain resolves to
/// the bare stored nickname, the domain compared case-insensitively.
#[tokio::test]
async fn lookup_strips_our_own_domain() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);
    for acct in ["alice@test.example", "alice@TEST.example"] {
        let response = lookup(&app, &format!("?acct={acct}")).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "`{acct}` names the local alice"
        );
        assert_eq!(
            response.json_value()["id"],
            alice.id.to_string(),
            "`{acct}` resolves to alice's row"
        );
    }
}

/// The nickname column is citext: any casing of the handle finds the row,
/// with or without the local domain.
#[tokio::test]
async fn lookup_is_case_insensitive() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "Alice").await;
    let app = common::test_app(pool);
    for acct in ["ALICE", "alice", "AlIce@TEST.example"] {
        let response = lookup(&app, &format!("?acct={acct}")).await;
        assert_eq!(response.status, StatusCode::OK, "`{acct}` folds case");
        assert_eq!(
            response.json_value()["id"],
            alice.id.to_string(),
            "`{acct}` finds the row stored as Alice"
        );
    }
}

/// An already-mirrored remote user resolves by its stored `nick@host`
/// handle (federation to fetch unseen ones is a later ticket).
#[tokio::test]
async fn lookup_finds_a_stored_remote_user() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool);
    let response = lookup(&app, "?acct=bob@remote.example").await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a mirrored remote handle resolves"
    );
    let body = response.json_value();
    assert_identity(
        &body,
        &bob,
        "bob@remote.example",
        "bob",
        "https://remote.example/users/bob",
    );
}

/// An absent, empty or blank `acct` is the 400 naming the parameter.
#[tokio::test]
async fn lookup_without_a_usable_acct_is_a_bad_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    for query in ["", "?", "?acct=", "?acct=%20%20"] {
        let response = lookup(&app, query).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "`{query:?}` carries no acct"
        );
        assert_eq!(
            response.text(),
            MISSING_ACCT,
            "the body names the missing parameter, `{query:?}`"
        );
    }
}

/// A handle that names no stored local or mirrored account is the plain
/// record-not-found 404.
#[tokio::test]
async fn lookup_an_unresolvable_acct_is_a_record_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice: User = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);
    for acct in [
        "nobody",
        // Stripped to a bare `nobody`, still absent.
        "nobody@test.example",
        // A foreign domain nobody is mirrored from.
        "alice@other.example",
        // Strips to an empty nickname.
        "@test.example",
    ] {
        let response = lookup(&app, &format!("?acct={acct}")).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{acct}` names no account"
        );
        assert_eq!(
            response.text(),
            NOT_FOUND,
            "an unknown acct is the plain 404 body, `{acct}`"
        );
    }
}

/// Lookup hides deactivated accounts too (Pleroma's `visible_for` check):
/// the row exists, but no acct finds it.
#[tokio::test]
async fn lookup_a_deactivated_account_is_a_record_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let ghost = common::create_local_user_with(&pool, "ghost", |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await;
    let app = common::test_app(pool);
    for query in ["?acct=ghost", "?acct=ghost@test.example"] {
        let response = lookup(&app, query).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "the deactivated {} is not served via {query}",
            ghost.nickname
        );
        assert_eq!(response.text(), NOT_FOUND, "like any other miss, {query}");
    }
}

/// `q` is the same handle parameter under its newer spelling: it answers
/// the stored rows exactly as `acct` does, and a blank `acct` beside a
/// filled `q` still lets the alias speak — but a usable `acct` outranks it.
#[tokio::test]
async fn lookup_reads_q_as_the_acct_alias() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool);

    let by_q = lookup(&app, "?q=alice").await;
    assert_eq!(by_q.status, StatusCode::OK, "q finds the local nickname");
    assert_eq!(by_q.json_value()["id"], alice.id.to_string());

    let remote_by_q = lookup(&app, "?q=bob@remote.example").await;
    assert_eq!(
        remote_by_q.status,
        StatusCode::OK,
        "q finds the mirrored handle"
    );
    assert_eq!(remote_by_q.json_value()["id"], bob.id.to_string());

    let blank_acct = lookup(&app, "?acct=%20&q=alice").await;
    assert_eq!(
        blank_acct.status,
        StatusCode::OK,
        "a blank acct defers to the alias"
    );
    assert_eq!(blank_acct.json_value()["id"], alice.id.to_string());

    let ranked = lookup(&app, "?acct=bob@remote.example&q=alice").await;
    assert_eq!(
        ranked.json_value()["id"],
        bob.id.to_string(),
        "a usable acct outranks the alias"
    );

    let neither = lookup(&app, "?acct=&q=").await;
    assert_eq!(
        neither.status,
        StatusCode::BAD_REQUEST,
        "no handle in either spelling is the 400"
    );
    assert_eq!(neither.text(), MISSING_ACCT, "naming acct, as before");
}

/// `resolve=true` turns a stored miss into a federation lookup, and that
/// arm runs only for an authenticated caller with a flag that spells true:
/// the anonymous request, the `resolve=false` request and the unflagged
/// one all keep the plain 404 for a handle this instance has never seen.
#[tokio::test]
async fn lookup_resolve_refuses_anonymous_and_unflagged_calls() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read").await;
    let app = common::test_app(pool);

    let anonymous = lookup(&app, "?q=nobody@other.example&resolve=true").await;
    assert_eq!(
        anonymous.status,
        StatusCode::NOT_FOUND,
        "an anonymous caller does not trigger a federation fetch"
    );
    assert_eq!(anonymous.text(), NOT_FOUND);

    for query in [
        "?q=nobody@other.example",
        "?q=nobody@other.example&resolve=false",
        "?q=nobody@other.example&resolve=",
    ] {
        let response = common::TestRequest::get(&format!("/api/v1/accounts/lookup{query}"))
            .bearer(&token)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{query}` is not the resolving arm"
        );
        assert_eq!(response.text(), NOT_FOUND);
    }
}

/// The same optional-auth rule guards lookup: an invalid token is a 401.
#[tokio::test]
async fn lookup_rejects_an_invalid_bearer_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/api/v1/accounts/lookup?acct=alice")
        .bearer("bogus-token")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "the same extractor guards lookup"
    );
    assert_eq!(response.text(), INVALID_TOKEN, "MaybeAuthUser's 401 body");
}

/// A user token without `read:accounts` may not look an account up, nor
/// trigger `resolve=true`: 403 with Pleroma's wording, while `read` and
/// anonymous requests answer 200.
#[tokio::test]
async fn lookup_requires_read_accounts_on_a_presented_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let narrow = common::create_app_and_token(&pool, &alice, "write:statuses").await;
    let broad = common::create_app_and_token(&pool, &alice, "read").await;
    let app = common::test_app(pool);

    for query in ["?acct=alice", "?acct=nobody@remote.example&resolve=true"] {
        let refused = common::TestRequest::get(&format!("/api/v1/accounts/lookup{query}"))
            .bearer(&narrow)
            .send(&app)
            .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "`{query}` needs read:accounts"
        );
        assert_eq!(refused.text(), NO_SCOPE, "Pleroma's wording");
    }

    let allowed = common::TestRequest::get("/api/v1/accounts/lookup?acct=alice")
        .bearer(&broad)
        .send(&app)
        .await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "`read` covers read:accounts"
    );
    let anonymous = lookup(&app, "?acct=alice").await;
    assert_eq!(anonymous.status, StatusCode::OK, "anonymous still proceeds");
}
