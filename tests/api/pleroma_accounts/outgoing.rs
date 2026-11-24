//! The outgoing follow-request listing over HTTP: its entries, its paging
//! on follow row ids and its default and capped page sizes.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::follow::Follow;

use super::support::{
    ask, asking, expected_link, following, listed_entry, locked_user, outgoing, wanted_follows,
    wanted_targets,
};
use crate::common;

/// The outgoing listing answers the accounts the caller asked to follow and
/// that have not answered — full `Account` entities, newest request first,
/// local and remote alike — while granted follows, other accounts' queues and
/// the caller's incoming requests stay out; an empty list is the empty array
/// with no `Link` header, and the entries carry Pleroma's empty embedded
/// relationship.
#[tokio::test]
async fn the_outgoing_list_answers_the_callers_own_asks() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let carol = locked_user(&pool, "carol").await;
    let dave = locked_user(&pool, "dave").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let erin = locked_user(&pool, "erin").await;

    let asks = [
        ask(&carol, &asking(&pool, &alice, &carol).await),
        ask(&mallory, &asking(&pool, &alice, &mallory).await),
    ];
    // bob is granted, so he is a follow and not an ask …
    let bob = common::create_local_user(&pool, "bob").await;
    let _granted: Follow = following(&pool, &alice, &bob).await;
    // … and dave's request to erin is erin's incoming queue, not alice's.
    let _other: Follow = asking(&pool, &dave, &erin).await;

    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let bob_token = common::create_app_and_token(&pool, &bob, "read:follows").await;
    let app = common::test_app(pool);

    let listed = outgoing(&app, Some(&token), "").await;
    assert_eq!(
        listed.status,
        StatusCode::OK,
        "the caller sees their own asks"
    );
    assert_eq!(
        listed.header("content-type"),
        Some("application/json"),
        "the entities are JSON"
    );
    let body = listed.json_value();
    assert_eq!(
        common::ids_of(&body),
        wanted_targets(&asks),
        "the two waiting asks, newest row first; the granted follow and the \
         other account's request stay out"
    );
    let carol_entry = listed_entry(&body, &carol.id);
    assert_eq!(
        carol_entry["acct"], "carol",
        "the local target renders as a full account with its handle"
    );
    let mallory_entry = listed_entry(&body, &mallory.id);
    assert_eq!(
        mallory_entry["acct"], "mallory@remote.example",
        "the remote mirror keeps what federation stored"
    );
    assert_eq!(mallory_entry["uri"], mallory.ap_id, "and its actor id");
    for entry in body.as_array().expect("the array") {
        assert_eq!(
            entry["pleroma"]["relationship"],
            json!({}),
            "Pleroma's outgoing index embeds no relationship"
        );
    }

    let empty = outgoing(&app, Some(&bob_token), "").await;
    assert_eq!(empty.status, StatusCode::OK, "an empty list is a 200");
    assert_eq!(
        empty.json_value(),
        json!([]),
        "bob was asked and asked nobody — alice's granted follow of him and \
         dave's ask to erin are not his outgoing requests"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// The list walks by `follows` row id: the `Link` header's cursors, the
/// `max_id` continuation, the `min_id` page (answered newest-first again) and
/// the empty ends of the walk, which carry no header.
#[tokio::test]
async fn the_outgoing_list_pages_key_on_follow_row_ids_and_link() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mut asks = Vec::new();
    for n in 0..5 {
        let target = locked_user(&pool, &format!("target{n}")).await;
        let row = asking(&pool, &alice, &target).await;
        asks.push(ask(&target, &row));
    }
    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app = common::test_app(pool);

    let wanted_users = wanted_targets(&asks);
    let wanted_rows = wanted_follows(&asks);

    let first = outgoing(&app, Some(&token), "?limit=2").await;
    assert_eq!(
        common::ids_of(&first.json_value()),
        wanted_users[..2],
        "two newest first"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link(&wanted_rows[1], &wanted_rows[0]).as_str()),
        "the Link cursors are the page's follow row ids, not account ids"
    );

    let older = outgoing(
        &app,
        Some(&token),
        &format!("?limit=2&max_id={}", wanted_rows[1]),
    )
    .await;
    assert_eq!(
        common::ids_of(&older.json_value()),
        wanted_users[2..4],
        "max_id continues where the page ended"
    );

    let newer = outgoing(&app, Some(&token), &format!("?min_id={}", wanted_rows[3])).await;
    assert_eq!(
        common::ids_of(&newer.json_value()),
        wanted_users[..3],
        "min_id answers the newest-first page just after the cursor"
    );

    let polled = outgoing(&app, Some(&token), &format!("?since_id={}", wanted_rows[0])).await;
    assert_eq!(
        polled.json_value(),
        json!([]),
        "nothing is newer than the head"
    );
    assert_eq!(
        polled.header("link"),
        None,
        "and an empty page sends no header"
    );

    let past = outgoing(&app, Some(&token), &format!("?max_id={}", wanted_rows[4])).await;
    assert_eq!(past.status, StatusCode::OK, "walking off the end is a 200");
    assert_eq!(past.json_value(), json!([]), "with the empty array");
    assert_eq!(past.header("link"), None, "and no header");
}

/// The listing pages with Pleroma's global pair, which is what the plain
/// [`PageParams`] extractor gives: twenty rows by default, forty at most.
#[tokio::test]
async fn the_outgoing_default_page_is_twenty_and_the_cap_is_forty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    for n in 0..45 {
        let target = locked_user(&pool, &format!("target{n}")).await;
        let _waiting: Follow = asking(&pool, &alice, &target).await;
    }
    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app = common::test_app(pool);

    let default = outgoing(&app, Some(&token), "").await;
    assert_eq!(
        common::ids_of(&default.json_value()).len(),
        20,
        "Pleroma's default page"
    );
    let greedy = outgoing(&app, Some(&token), "?limit=100").await;
    assert_eq!(
        common::ids_of(&greedy.json_value()).len(),
        40,
        "and the global cap, never more"
    );
    let between = outgoing(&app, Some(&token), "?limit=30").await;
    assert_eq!(
        common::ids_of(&between.json_value()).len(),
        30,
        "a page under the cap goes through"
    );
}
