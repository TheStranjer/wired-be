//! The scope gates of the chat routes (WBE-146): every reading route
//! demands `read:chats`, creating a chat demands `write:chats`, and the plain
//! `read` and `write` roots cover both sub-scopes.

use axum::http::StatusCode;

use super::support::{
    READ_SCOPE, ROOT, SCOPE, V2_ROOT, assert_refused, error_message, fixture, get_anonymous,
    get_as, open_path, post_empty, thread,
};
use crate::common;

/// The three reading routes — both lists and the show — demand a user token
/// granting `read:chats`: no token is the 401 "Invalid credentials", a bad
/// one the 401 "The access token is invalid", an app-only token the 403
/// demanding a user, and a `write:chats` or `read:accounts` token the 403
/// naming the missing scope; the plain `read` root covers the sub-scope.
#[tokio::test]
async fn every_reading_route_demands_read_chats() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let started = thread(&pool).await;
    let read_token = common::create_app_and_token(&pool, &started.bob, READ_SCOPE).await;
    let broad_token = common::create_app_and_token(&pool, &started.bob, "read").await;
    let (_, narrow) = fixture(&pool, "erin", "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, READ_SCOPE).await;
    let app = common::test_app(pool.clone());
    let paths = [
        ROOT.to_owned(),
        V2_ROOT.to_owned(),
        format!("{ROOT}/{}", started.chat_id()),
    ];

    for path in paths {
        assert_refused(
            &get_anonymous(&app, &path).await,
            "no token at all",
            &path,
            StatusCode::UNAUTHORIZED,
            "Invalid credentials",
        );
        assert_refused(
            &get_as(&app, "not-a-real-token", &path).await,
            "a bogus token",
            &path,
            StatusCode::UNAUTHORIZED,
            "The access token is invalid",
        );
        assert_refused(
            &get_as(&app, &app_only, &path).await,
            "an app-only token",
            &path,
            StatusCode::FORBIDDEN,
            "This API requires an authenticated user",
        );
        assert_refused(
            &get_as(&app, &narrow, &path).await,
            "`read:accounts`",
            &path,
            StatusCode::FORBIDDEN,
            "Insufficient permissions: read:chats.",
        );

        let write_only = get_as(&app, &started.token, &path).await;
        assert_eq!(
            write_only.status,
            StatusCode::FORBIDDEN,
            "`write:chats` reads nothing on {path}"
        );
    }

    let owned = format!("{ROOT}/{}", started.chat_id());
    assert_eq!(
        get_as(&app, &read_token, ROOT).await.status,
        StatusCode::OK,
        "bob lists with `read:chats`"
    );
    assert_eq!(
        get_as(&app, &read_token, V2_ROOT).await.status,
        StatusCode::OK,
        "on v2 alike"
    );
    assert_eq!(
        get_as(&app, &read_token, &owned).await.status,
        StatusCode::OK,
        "and shows his own chat"
    );
    assert_eq!(
        get_as(&app, &broad_token, ROOT).await.status,
        StatusCode::OK,
        "the plain `read` root covers `read:chats`"
    );
}

/// `POST …/by-account-id/{id}` demands `write:chats` the same way — the
/// read scopes cannot open chats — and the plain `write` root covers the
/// sub-scope on the token's own chats.
#[tokio::test]
async fn creating_a_chat_demands_write_chats() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read_token) = fixture(&pool, "alice", READ_SCOPE).await;
    let (erin, _) = fixture(&pool, "erin", "read").await;
    let erin_write = common::create_app_and_token(&pool, &erin, "write").await;
    let app_only = common::create_app_only_token(&pool, SCOPE).await;
    let app = common::test_app(pool.clone());
    let path = open_path(&alice.id.to_string());

    let reader = post_empty(&app, &read_token, &path).await;
    assert_eq!(
        reader.status,
        StatusCode::FORBIDDEN,
        "a reader may not open chats: {}",
        reader.text()
    );
    assert_eq!(
        error_message(&reader).as_deref(),
        Some("Insufficient permissions: write:chats."),
        "the scope refusal wording"
    );

    let anonymous = common::TestRequest::post(&path).send(&app).await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token is a 401 on create too"
    );
    assert_eq!(
        error_message(&anonymous).as_deref(),
        Some("Invalid credentials"),
        "the missing-token wording"
    );

    let wrong_kind = post_empty(&app, &app_only, &path).await;
    assert_eq!(
        wrong_kind.status,
        StatusCode::FORBIDDEN,
        "an app-only token may not open chats"
    );
    assert_eq!(
        error_message(&wrong_kind).as_deref(),
        Some("This API requires an authenticated user"),
        "the user refusal wording"
    );

    let broad = post_empty(&app, &erin_write, &path).await;
    assert_eq!(
        broad.status,
        StatusCode::OK,
        "the plain `write` root covers `write:chats`: {}",
        broad.text()
    );
    assert_eq!(
        broad.json_value()["account"]["id"],
        alice.id.to_string(),
        "and opens erin's own chat with alice"
    );
}
