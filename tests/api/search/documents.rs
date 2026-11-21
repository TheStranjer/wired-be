//! The combined search documents over HTTP: `GET /api/v1/search` answering
//! the accounts document, the explicit `type` that fills exactly the list
//! it names, the v2 twin that matches v1's shape and rides its `following`
//! and `limit`, the malformed cursor that is the shared 400 on every route,
//! and the one `search` bucket all three routes spend.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::{hashtag, status::Visibility};

use super::support::{
    ACCOUNTS_SEARCH, INVALID_ID, THROTTLED, V1_SEARCH, V2_SEARCH, accounts_of,
    assert_all_lists_empty, assert_document, assert_list_empty, fixture, follow_pair, get, peer,
    statuses_of,
};
use crate::common;

/// `GET /api/v1/search` answers the document with the account search in
/// `accounts`; with no `type` or `type=accounts` alike.
#[tokio::test]
async fn v1_search_answers_the_accounts_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let bob = common::create_local_user(&pool, "bob").await;

    for query in ["q=bo", "q=bo&type=accounts"] {
        let found = get(&app, None, &format!("{V1_SEARCH}?{query}")).await;
        common::assert_answer(&found, StatusCode::OK, "the combined search");
        let body = found.json_value();
        assert_document(&body);
        assert_eq!(
            accounts_of(&body),
            vec![bob.id.to_string()],
            "the accounts list carries the search"
        );
        assert_eq!(body["accounts"][0]["acct"], "bob", "as account entities");
    }
}

/// An explicit `type` fills exactly that resource's list: the other two
/// stay empty even when their data matches, and an unknown `type` fills
/// nothing at all.
#[tokio::test]
async fn an_explicit_type_fills_exactly_that_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let bobby = common::create_local_user(&pool, "bobby").await;
    let _posted = common::create_status(&pool, &bobby, "brown fox bobby", Visibility::Public).await;
    // The tag row comes from the name table itself: a post carrying
    // `#brown` would also answer the *status* search for `brown`.
    let _brown = hashtag::find_or_create(&pool, "#brown")
        .await
        .expect("the tag fixture stores")
        .expect("brown is a usable name");

    let only_statuses = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q=brown&type=statuses"),
    )
    .await;
    common::assert_answer(&only_statuses, StatusCode::OK, "the statuses arm");
    let status_body = only_statuses.json_value();
    assert_document(&status_body);
    assert_eq!(
        statuses_of(&status_body).len(),
        1,
        "the one matching status answers"
    );
    assert_list_empty(&status_body, "accounts", "type=statuses");
    assert_list_empty(&status_body, "hashtags", "type=statuses");

    // `bobby` names the account AND a status word, so the accounts arm
    // proves the routing in the other direction.
    let only_accounts = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q=bobby&type=accounts"),
    )
    .await;
    let account_body = only_accounts.json_value();
    assert_document(&account_body);
    assert_eq!(
        accounts_of(&account_body),
        vec![bobby.id.to_string()],
        "the account answers to its own name"
    );
    assert_list_empty(&account_body, "statuses", "type=accounts");
    assert_list_empty(&account_body, "hashtags", "type=accounts");

    let only_tags = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q=brown&type=hashtags"),
    )
    .await;
    let tag_body = only_tags.json_value();
    assert_document(&tag_body);
    assert_eq!(
        tag_body["hashtags"],
        json!(["brown"]),
        "the v1 hashtag arm answers plain names"
    );
    assert_list_empty(&tag_body, "accounts", "type=hashtags");
    assert_list_empty(&tag_body, "statuses", "type=hashtags");

    for path in [V1_SEARCH, V2_SEARCH] {
        let none = get(&app, Some(&token), &format!("{path}?q=brown&type=potato")).await;
        common::assert_answer(&none, StatusCode::OK, "an unknown type is no error");
        assert_all_lists_empty(&none.json_value(), "type=potato");
    }
}

/// `GET /api/v2/search` answers the same document shape as its v1 twin
/// (the two differ only once hashtags exist), pages with the same `limit`,
/// and honours `following` like v1 does.
#[tokio::test]
async fn v2_search_matches_the_v1_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (viewer, token) = fixture(&pool, "finder", "read").await;
    let carol = common::create_local_user(&pool, "carol").await;
    follow_pair(&pool, &viewer, &carol).await;
    let cardu = common::create_local_user(&pool, "cardu").await;

    let v1 = get(&app, Some(&token), &format!("{V1_SEARCH}?q=car")).await;
    let v2 = get(&app, Some(&token), &format!("{V2_SEARCH}?q=car")).await;
    common::assert_answer(&v2, StatusCode::OK, "the v2 search");
    assert_document(&v2.json_value());
    assert_eq!(
        accounts_of(&v2.json_value()),
        accounts_of(&v1.json_value()),
        "with only accounts searched, v1 and v2 answer the same"
    );
    assert_eq!(
        accounts_of(&v2.json_value()),
        vec![carol.id.to_string(), cardu.id.to_string()],
        "the viewer's follow leads the page"
    );

    let restricted = get(
        &app,
        Some(&token),
        &format!("{V2_SEARCH}?q=car&following=true&limit=1"),
    )
    .await;
    assert_eq!(
        accounts_of(&restricted.json_value()),
        vec![carol.id.to_string()],
        "following and limit ride along into v2"
    );
}

/// A malformed pagination cursor is the shared 400, on every search route.
#[tokio::test]
async fn a_malformed_cursor_is_a_bad_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    for path in [ACCOUNTS_SEARCH, V1_SEARCH, V2_SEARCH] {
        let found = get(&app, None, &format!("{path}?q=bo&max_id=nope")).await;
        common::assert_answer(
            &found,
            StatusCode::BAD_REQUEST,
            &format!("{path} bad cursor"),
        );
        common::assert_error(&found, INVALID_ID, &format!("{path} bad cursor"));
    }
}

/// All three search routes count into the one `search` bucket: the budget
/// spent across two paths refuses the third.
#[tokio::test]
async fn the_three_search_routes_share_the_search_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _bob = common::create_local_user(&pool, "bob").await;
    let config = common::test_config_with_rate_limits("search=2:600");
    let app = common::test_app_with_config(pool, config);

    for path in [ACCOUNTS_SEARCH, V1_SEARCH] {
        let answer = common::TestRequest::get(&format!("{path}?q=bo"))
            .peer(peer(1))
            .send(&app)
            .await;
        common::assert_answer(&answer, StatusCode::OK, "inside the budget");
        assert_eq!(
            answer.header("x-ratelimit-limit"),
            Some("2"),
            "the bucket's budget"
        );
    }
    let refused = common::TestRequest::get(&format!("{V2_SEARCH}?q=bo"))
        .peer(peer(1))
        .send(&app)
        .await;
    common::assert_answer(&refused, StatusCode::TOO_MANY_REQUESTS, "the spent window");
    common::assert_error(&refused, THROTTLED, "the refusal body");
}
