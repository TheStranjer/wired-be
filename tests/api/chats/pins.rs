//! Pinning over HTTP (WBE-148): `POST …/pin` and `…/unpin` toggling `pinned`
//! and moving what the lists' `pinned` filter sees, the owner-only reads, and
//! the gate matrix of every `write:chats` route.

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    models::chat::{self, ChatFilter},
    util::id::new_id,
};

use super::support::{
    CHAT_KEYS, ROOT, SCOPE, all_page, assert_refused, fixture, marker_body, message_read_path,
    parse, post_as, post_empty, post_json, read_path, stored_unread, thread,
};
use crate::common;

/// The stored `pinned` flag of `user_id`'s chat `chat_id`.
async fn stored_pinned(pool: &PgPool, user_id: Uuid, chat_id: Uuid) -> bool {
    chat::get_owned(pool, user_id, chat_id)
        .await
        .expect("the chat read works")
        .expect("the chat exists")
        .pinned
}

/// The ids of `user_id`'s chats under the `pinned` filter — the list query
/// (`page_for_user` with [`ChatFilter`]) the chats list endpoint runs.
async fn pinned_ids(pool: &PgPool, user_id: Uuid, pinned: Option<bool>) -> Vec<Uuid> {
    chat::page_for_user(
        pool,
        user_id,
        all_page(),
        ChatFilter {
            pinned,
            with_muted: false,
        },
    )
    .await
    .expect("the list reads")
    .into_iter()
    .map(|row| row.id)
    .collect()
}

/// Foreign, unknown and malformed chat ids are one 404 on every route, and
/// nothing behind them moves: the other participant's side of the same
/// conversation is a different id that their token cannot use.
#[tokio::test]
async fn chats_are_only_their_owners_to_read_and_pin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let started = thread(&pool).await;
    // alice's own side of the same conversation.
    let page = chat::page_for_user(&pool, started.alice.id, all_page(), ChatFilter::default())
        .await
        .expect("alice's list reads");
    let alices_chat = page.first().expect("alice has one chat").id;
    let app = common::test_app(pool.clone());

    for foreign in [
        alices_chat.to_string(),
        new_id().to_string(),
        "not-a-uuid".to_owned(),
    ] {
        let read = post_json(
            &app,
            &started.token,
            &read_path(&foreign),
            marker_body(started.refs[2]),
        )
        .await;
        assert_eq!(
            read.status,
            StatusCode::NOT_FOUND,
            "chat {foreign} is not bob's to read-mark"
        );
        assert_eq!(
            read.error_field().as_deref(),
            Some("Record not found"),
            "the 404 wording on {foreign}"
        );
        let pin = post_empty(&app, &started.token, &format!("{ROOT}/{foreign}/pin")).await;
        assert_eq!(pin.status, StatusCode::NOT_FOUND, "nor to pin");
        let message = post_empty(
            &app,
            &started.token,
            &message_read_path(&foreign, &started.refs[0].to_string()),
        )
        .await;
        assert_eq!(
            message.status,
            StatusCode::NOT_FOUND,
            "not even his own messages through it"
        );
    }

    assert!(
        !stored_pinned(&pool, started.alice.id, alices_chat).await,
        "alice's row was never pinned through bob's token"
    );
    assert_eq!(
        stored_unread(&pool, parse(&started.chat_id())).await,
        3,
        "bob's chat stayed unread too — every attempt was a 404"
    );
}

/// `POST …/pin` and `POST …/unpin` toggle the caller's own flag, answer the
/// chat entity with it, and move what the list's `pinned` filter sees; the
/// other side of the conversation keeps its own flag, and an already-pinned
/// chat stays pinned under a second pin.
#[tokio::test]
async fn pin_and_unpin_toggle_the_flag_and_the_list_filter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let started = thread(&pool).await;
    let app = common::test_app(pool.clone());
    let chat = started.chat_id();
    let own = parse(&chat);
    let token = started.token.clone();

    let pinned = post_empty(&app, &token, &format!("{ROOT}/{chat}/pin")).await;
    assert_eq!(
        pinned.status,
        StatusCode::OK,
        "pinning answers 200: {}",
        pinned.text()
    );
    let value = pinned.json_value();
    assert_eq!(
        common::sorted_keys(&value),
        CHAT_KEYS,
        "the full Chat entity"
    );
    assert_eq!(value["id"], chat, "bob's chat");
    assert_eq!(value["pinned"], true, "now pinned");
    assert_eq!(value["unread"], 3, "pinning marks nothing read");
    assert_eq!(value["last_message"]["id"], started.refs[2].to_string());
    assert!(
        stored_pinned(&pool, started.bob.id, own).await,
        "the flag is stored"
    );

    let again = post_empty(&app, &token, &format!("{ROOT}/{chat}/pin")).await;
    assert_eq!(again.json_value()["pinned"], true, "pinning twice holds");

    assert_eq!(
        pinned_ids(&pool, started.bob.id, Some(true)).await,
        vec![own],
        "the pinned filter names the chat"
    );
    assert!(
        pinned_ids(&pool, started.bob.id, Some(false))
            .await
            .is_empty(),
        "and the unpinned half is empty"
    );

    let unpinned = post_empty(&app, &token, &format!("{ROOT}/{chat}/unpin")).await;
    assert_eq!(unpinned.status, StatusCode::OK, "unpinning answers 200");
    assert_eq!(
        unpinned.json_value()["pinned"],
        false,
        "and clears the flag"
    );
    assert!(
        pinned_ids(&pool, started.bob.id, Some(true))
            .await
            .is_empty(),
        "the pinned filter lets go"
    );
    assert_eq!(
        pinned_ids(&pool, started.bob.id, Some(false)).await,
        vec![own],
        "the chat is back in the unpinned half"
    );

    // bob's toggle is his own ordering choice: alice's side never changes.
    assert!(
        !stored_pinned(&pool, started.alice.id, {
            let rows =
                chat::page_for_user(&pool, started.alice.id, all_page(), ChatFilter::default())
                    .await
                    .expect("alice lists");
            rows.first().expect("alice has one chat").id
        })
        .await,
        "alice's row is untouched by bob's pin and unpin"
    );
}

/// Every `write:chats` route refuses what it must: no token at all the
/// 401 "Invalid credentials", a bad one the 401 "The access token is
/// invalid", an app-only token and a `read:chats` user token the 403 — and a
/// plain `write` token covers the sub-scope through the Mastodon hierarchy.
#[tokio::test]
async fn every_route_demands_write_chats() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let started = thread(&pool).await;
    let app = common::test_app(pool.clone());
    let chat = started.chat_id();
    let routes = [
        (read_path(&chat), Some(marker_body(started.refs[2]))),
        (message_read_path(&chat, &started.refs[0].to_string()), None),
        (format!("{ROOT}/{chat}/pin"), None),
        (format!("{ROOT}/{chat}/unpin"), None),
    ];
    let (_, narrow) = fixture(&pool, "dave", "read:chats").await;
    let app_only = common::create_app_only_token(&pool, SCOPE).await;

    for (path, body) in routes {
        assert_refused(
            &post_as(&app, &path, Some("not-a-real-token"), body.as_ref()).await,
            "a bogus token",
            &path,
            StatusCode::UNAUTHORIZED,
            "The access token is invalid",
        );
        assert_refused(
            &post_as(&app, &path, None, body.as_ref()).await,
            "no token at all",
            &path,
            StatusCode::UNAUTHORIZED,
            "Invalid credentials",
        );
        assert_refused(
            &post_as(&app, &path, Some(&app_only), body.as_ref()).await,
            "an app-only token",
            &path,
            StatusCode::FORBIDDEN,
            "This API requires an authenticated user",
        );
        assert_refused(
            &post_as(&app, &path, Some(&narrow), body.as_ref()).await,
            "`read:chats`",
            &path,
            StatusCode::FORBIDDEN,
            "Insufficient permissions: write:chats.",
        );
    }

    // The scope hierarchy: a plain `write` grant covers `write:chats` — on
    // the token's own chat (bob's stays out of erin's reach).
    let (erin, _) = fixture(&pool, "erin", "read").await;
    let erin_token = common::create_app_and_token(&pool, &erin, "write").await;
    let erins_chat = chat::get_or_create(&pool, &erin, &started.alice)
        .await
        .expect("erin's chat with alice opens");
    let pinned = post_empty(&app, &erin_token, &format!("{ROOT}/{}/pin", erins_chat.id)).await;
    assert_eq!(
        pinned.status,
        StatusCode::OK,
        "`write` grants `write:chats`: {}",
        pinned.text()
    );
    assert_eq!(pinned.json_value()["pinned"], true, "and the flag is set");
    let stranger = post_empty(&app, &erin_token, &format!("{ROOT}/{chat}/pin")).await;
    assert_eq!(
        stranger.status,
        StatusCode::NOT_FOUND,
        "still only through the caller's own chats"
    );
}
