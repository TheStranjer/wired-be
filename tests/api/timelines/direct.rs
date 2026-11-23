//! The direct timeline over HTTP (`GET /api/v1/timelines/direct`): the
//! viewer's own `direct` conversations — sent and @-mentioned, newest
//! first, never another pair's DM — the `Link`-keyed id cursor with its
//! empty page past the oldest row, `with_muted` lifting the viewer's own
//! block, and the user-token gate demanding `read:statuses`. Owns the
//! `direct` fixture every test here seeds through [`wired_be::models::status`].

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;
use wired_be::models::{
    block,
    status::{self, NewStatus, Status, Visibility},
    user::User,
};

use super::support::{BASE, DIRECT, get, id_of, ids, newest_first};
use crate::common;

/// Inserts `text` as `author`'s `direct` status @-mentioning everyone in
/// `mentioned` (empty for none), straight through
/// [`wired_be::models::status`] — the read the visibility clause judges
/// by the stored mention rows.
async fn direct(pool: &PgPool, author: &User, mentioned: &[&User], text: &str) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            visibility: Visibility::Direct,
            mentioned_user_ids: mentioned.iter().map(|member| member.id).collect(),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the direct fixture inserts")
}

/// The direct timeline answers the viewer's own conversations — what they
/// sent and what @-mentioned them, newest first — and nothing else: no
/// DM between other accounts, no `public` or `private` post of the
/// viewer's own or anyone else's.
#[tokio::test]
async fn direct_answers_the_conversations_the_viewer_is_in() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let a_to_b = direct(&pool, &alice, &[&bob], "alice to bob").await;
    let b_to_a = direct(&pool, &bob, &[&alice], "bob to alice").await;
    let a_to_c = direct(&pool, &alice, &[&carol], "alice to carol").await;
    let c_to_b = direct(&pool, &carol, &[&bob], "carol to bob").await;
    let _alice_public = common::create_status(&pool, &alice, "not a dm", Visibility::Public).await;
    let _alice_private =
        common::create_status(&pool, &alice, "circle only", Visibility::Private).await;
    let _bob_unlisted =
        common::create_status(&pool, &bob, "quiet post", Visibility::Unlisted).await;
    let alice_token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let bob_token = common::create_app_and_token(&pool, &bob, "read:statuses").await;
    let app = common::test_app(pool);

    for (token, expected) in [
        (&alice_token, newest_first(&[&a_to_b, &b_to_a, &a_to_c])),
        (&bob_token, newest_first(&[&a_to_b, &b_to_a, &c_to_b])),
    ] {
        let page = get(&app, Some(token), DIRECT).await;
        assert_eq!(page.status, StatusCode::OK, "a direct page is a 200");
        assert_eq!(
            page.header("content-type"),
            Some("application/json"),
            "the page is JSON"
        );
        assert_eq!(
            ids(&page.json_value()),
            expected,
            "each viewer's page is their own conversations, newest first"
        );
    }
    let alice_page = get(&app, Some(&alice_token), DIRECT).await;
    assert!(
        !ids(&alice_page.json_value()).contains(&id_of(&c_to_b)),
        "a DM between other accounts never leaks into someone else's direct timeline"
    );
}

/// The direct page walks the id cursor like every other timeline: `limit`
/// cuts it, the `Link` header names the next and prev targets over the
/// page's oldest and newest ids, `max_id` continues from there, a `min_id`
/// page still comes back newest-first, and past the oldest row the page is
/// empty and carries no `Link` header.
#[tokio::test]
async fn direct_pages_walk_the_id_cursor_under_link_headers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let oldest = direct(&pool, &alice, &[&bob], "dm one").await;
    let middle = direct(&pool, &alice, &[&bob], "dm two").await;
    let newest = direct(&pool, &bob, &[&alice], "dm three").await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let first = get(&app, Some(&token), &format!("{DIRECT}?limit=2")).await;
    assert_eq!(
        ids(&first.json_value()),
        newest_first(&[&middle, &newest]),
        "the first page is the two newest rows"
    );
    let expected_link = format!(
        "<{BASE}{DIRECT}?limit=2&max_id={}>; rel=\"next\", \
         <{BASE}{DIRECT}?limit=2&min_id={}>; rel=\"prev\"",
        id_of(&middle),
        id_of(&newest),
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link.as_str()),
        "next walks to the page's oldest id, prev back to its newest, `limit` repeated"
    );

    let second = get(
        &app,
        Some(&token),
        &format!("{DIRECT}?limit=2&max_id={}", id_of(&middle)),
    )
    .await;
    assert_eq!(
        ids(&second.json_value()),
        newest_first(&[&oldest]),
        "the next target answers the row older than the cursor"
    );

    let forward = get(
        &app,
        Some(&token),
        &format!("{DIRECT}?min_id={}", id_of(&oldest)),
    )
    .await;
    assert_eq!(
        ids(&forward.json_value()),
        newest_first(&[&middle, &newest]),
        "a min_id page is read ascending and answered newest-first"
    );

    let empty = get(
        &app,
        Some(&token),
        &format!("{DIRECT}?max_id={}", id_of(&oldest)),
    )
    .await;
    assert_eq!(
        empty.json::<Vec<Value>>(),
        Vec::<Value>::new(),
        "past the oldest row the page is empty"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// A DM from an account the viewer blocked stays off their direct
/// timeline, `with_muted=true` or not — the toggle lifts mutes only.
#[tokio::test]
async fn direct_hides_blocked_senders_even_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let own = direct(&pool, &alice, &[&bob], "alice to bob").await;
    let blocked = block::block(&pool, BASE, &alice, &bob)
        .await
        .expect("alice blocks bob");
    assert_eq!(blocked.blocked_id, bob.id, "the block is live");
    let from_bob = direct(&pool, &bob, &[&alice], "bob to alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let filtered = get(&app, Some(&token), DIRECT).await;
    assert_eq!(
        ids(&filtered.json_value()),
        newest_first(&[&own]),
        "bob's DM is blocked away from alice's direct timeline"
    );
    let shown = get(&app, Some(&token), &format!("{DIRECT}?with_muted=true")).await;
    let shown_ids = ids(&shown.json_value());
    assert_eq!(
        shown_ids,
        newest_first(&[&own]),
        "`with_muted=true` never lifts alice's block"
    );
    assert!(
        !shown_ids.contains(&from_bob.id.to_string()),
        "bob's DM stays off the page"
    );
}

/// The direct route is gated on a user token: no token is the 401
/// "Invalid credentials" and a broken one the 401 "The access token is
/// invalid"; an app-only token and a user token without `read:statuses`
/// are the 403s; a granted token reads its page.
#[tokio::test]
async fn direct_requires_a_user_token_granting_read_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let dm = direct(&pool, &alice, &[], "to nobody").await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let noscope = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "read:statuses").await;
    let app = common::test_app(pool);

    let anonymous = get(&app, None, DIRECT).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, "Invalid credentials", "no token");

    let broken = get(&app, Some("not-a-real-token"), DIRECT).await;
    common::assert_answer(&broken, StatusCode::UNAUTHORIZED, "a broken token");
    common::assert_error(&broken, "The access token is invalid", "a broken token");

    let client = get(&app, Some(&app_only), DIRECT).await;
    common::assert_answer(&client, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(
        &client,
        "This API requires an authenticated user",
        "an app-only token",
    );

    let unscoped = get(&app, Some(&noscope), DIRECT).await;
    common::assert_answer(
        &unscoped,
        StatusCode::FORBIDDEN,
        "a token without read:statuses",
    );
    common::assert_error(
        &unscoped,
        "Insufficient permissions: read:statuses.",
        "a token without read:statuses",
    );

    let granted = get(&app, Some(&reader), DIRECT).await;
    assert_eq!(
        granted.status,
        StatusCode::OK,
        "a granted user token reads its page"
    );
    assert_eq!(
        ids(&granted.json_value()),
        newest_first(&[&dm]),
        "and sees the unaddressed DM she authored"
    );
}
