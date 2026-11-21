//! The favourites route's cursor mistakes, page sizes, account misses and
//! scope gate.

use axum::http::StatusCode;
use uuid::Uuid;
use wired_be::models::status::Visibility;

use super::support::{
    BAD_ID, INVALID_TOKEN, NO_FAV_SCOPE, NOT_FOUND, deactivated_user, favourites, like,
    listed_status_ids, open_account, post, public_post_by, viewer,
};
use crate::common;

/// A malformed favourites cursor is the page parser's 400 with its message,
/// and the extractor still runs first: a bad token alongside a bad cursor
/// is the 401, not a silent anonymous read.
#[tokio::test]
async fn the_favourites_cursor_mistakes_answer_400_behind_the_401() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (bob, _) = open_account(&pool, "bob", "read").await;
    let (_alice, token) = viewer(&pool, "alice", "read:favourites").await;
    let target = bob.id.to_string();

    for query in ["?max_id=nope", "?min_id=01890b9a-nope"] {
        let refused = favourites(&app, Some(&token), &target, query).await;
        common::assert_answer(
            &refused,
            StatusCode::BAD_REQUEST,
            &format!("`{query}` is refused"),
        );
        assert_eq!(refused.text(), BAD_ID, "the page parser's message");
    }
    let denied = favourites(&app, Some("bogus-token"), &target, "?max_id=nope").await;
    common::assert_answer(
        &denied,
        StatusCode::UNAUTHORIZED,
        "the 401 comes before the 400",
    );
    assert_eq!(denied.text(), INVALID_TOKEN, "AuthUser's invalid body");
}

/// Pleroma's global page sizes over the favourites walk: twenty rows by
/// default, forty at most — even though every one of the forty-one seeded
/// favourites is visible to the viewer.
#[tokio::test]
async fn the_favourites_default_page_is_twenty_and_the_cap_is_forty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (bob, _) = open_account(&pool, "bob", "read").await;
    let (_alice, token) = viewer(&pool, "alice", "read:favourites").await;
    let author = common::create_local_user(&pool, "author").await;
    for n in 0..41 {
        let status = post(&pool, &author, Visibility::Public, &format!("post {n}")).await;
        like(&pool, &bob, status.id).await;
    }
    let target = bob.id.to_string();

    let default = favourites(&app, Some(&token), &target, "").await;
    common::assert_answer(&default, StatusCode::OK, "the default page answers");
    assert_eq!(
        listed_status_ids(&default.json_value()).len(),
        20,
        "Pleroma's default page"
    );
    let greedy = favourites(&app, Some(&token), &target, "?limit=100").await;
    assert_eq!(
        listed_status_ids(&greedy.json_value()).len(),
        40,
        "and the global cap, never more — 41 likes were seeded"
    );
    let between = favourites(&app, Some(&token), &target, "?limit=30").await;
    assert_eq!(
        listed_status_ids(&between.json_value()).len(),
        30,
        "a page under the cap goes through"
    );
}

/// Every account miss on the favourites route is the same 404, whoever
/// presents: an id that is not uuid text, an id that names no user, and a
/// deactivated account — token or no token.
#[tokio::test]
async fn every_favourites_account_miss_is_the_same_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = viewer(&pool, "alice", "read:favourites").await;
    let ghost = deactivated_user(&pool, "ghost").await;
    let missing = Uuid::now_v7().to_string();

    for target in [missing.as_str(), "not-an-id", &ghost.id.to_string()] {
        for who in [Some(token.as_str()), None] {
            let response = favourites(&app, who, target, "").await;
            common::assert_answer(
                &response,
                StatusCode::NOT_FOUND,
                &format!("`{target}` names no account"),
            );
            assert_eq!(response.text(), NOT_FOUND, "decision 8's 404 body");
        }
    }
}

/// The optional gate: no token and an app-only token read the open list as
/// unauthenticated viewers; a presented *user* token must grant
/// `read:favourites` — anything else is Pleroma's insufficient-permissions
/// 403, checked before the account is ever looked at or its hidden-favourites
/// flag consulted; and a token that does not resolve is the 401.
#[tokio::test]
async fn the_favourites_gate_demands_the_scope_of_a_presented_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (bob, _) = open_account(&pool, "bob", "read").await;
    let (alice, _) = viewer(&pool, "alice", "read").await;
    let status = public_post_by(&pool, "author").await;
    like(&pool, &bob, status.id).await;
    let target = bob.id.to_string();

    let anonymous = favourites(&app, None, &target, "").await;
    common::assert_answer(&anonymous, StatusCode::OK, "no token reads an open list");
    assert_eq!(
        anonymous.json_value()[0]["favourited"],
        false,
        "rendered for the anonymous eye"
    );
    let app_only = common::create_app_only_token(&pool, "read").await;
    let headless = favourites(&app, Some(&app_only), &target, "").await;
    common::assert_answer(
        &headless,
        StatusCode::OK,
        "an app-only token proceeds unauthenticated",
    );

    for scopes in ["read", "read:favourites"] {
        let good = common::create_app_and_token(&pool, &alice, scopes).await;
        let listed = favourites(&app, Some(&good), &target, "").await;
        common::assert_answer(
            &listed,
            StatusCode::OK,
            &format!("`{scopes}` reads the list"),
        );
    }
    for scopes in ["write", "write:favourites", "read:statuses"] {
        let bad = common::create_app_and_token(&pool, &alice, scopes).await;
        let refused = favourites(&app, Some(&bad), &target, "").await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("`{scopes}` does not read favourites"),
        );
        assert_eq!(
            refused.text(),
            NO_FAV_SCOPE,
            "the denial names read:favourites"
        );
    }

    // The scopes plug runs before `assign_account_by_id` and before the
    // privacy arm: a scopeless probe stays on the scope refusal whether the
    // account is missing or its favourites hidden.
    let bad_token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let missing = Uuid::now_v7().to_string();
    let probe = favourites(&app, Some(&bad_token), &missing, "").await;
    common::assert_answer(
        &probe,
        StatusCode::FORBIDDEN,
        "the scope gate precedes the 404",
    );
    assert_eq!(probe.text(), NO_FAV_SCOPE, "the scope refusal, not a 404");
    let carol = common::create_local_user(&pool, "carol").await;
    let hidden_probe = favourites(&app, Some(&bad_token), &carol.id.to_string(), "").await;
    common::assert_answer(
        &hidden_probe,
        StatusCode::FORBIDDEN,
        "the scope gate precedes privacy",
    );
    assert_eq!(
        hidden_probe.text(),
        NO_FAV_SCOPE,
        "the scope refusal, not the privacy one"
    );

    let invalid = favourites(&app, Some("bogus-token"), &target, "").await;
    common::assert_answer(
        &invalid,
        StatusCode::UNAUTHORIZED,
        "a token that does not resolve",
    );
    assert_eq!(
        invalid.text(),
        INVALID_TOKEN,
        "is the 401, never a silent anonymous read"
    );
}
