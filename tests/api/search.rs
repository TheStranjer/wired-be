//! Integration tests for the account-search endpoints (WBE-115):
//! `GET /api/v1/accounts/search` (the flat account array), and
//! `GET /api/v1/search` / `GET /api/v2/search` (the combined document,
//! whose accounts arm is this ticket and whose `statuses` and `hashtags`
//! lists are empty). The routes answer anonymously (Pleroma's
//! `fallback: :proceed_unauthenticated`), demand `read:accounts` from a
//! presented user token, normalise the query (leading `@`, case), page by
//! `limit` (default 40, cap 80) and `offset`, restrict to the viewer's
//! follows on `following=true`, and count into the limiter's `search`
//! bucket.

#![cfg(test)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::Router;
use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;
use wired_be::models::{follow, user::User};

use crate::common;

/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = wired_be::config::TEST_BASE_URL;
/// The flat account-array search.
const ACCOUNTS_SEARCH: &str = "/api/v1/accounts/search";
/// The combined v1 search document.
const V1_SEARCH: &str = "/api/v1/search";
/// The combined v2 search document.
const V2_SEARCH: &str = "/api/v2/search";
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body of a search whose presented token lacks `read:accounts`.
const NO_READ_ACCOUNTS: &str = "Insufficient permissions: read:accounts.";
/// The 400 body of an unreadable pagination cursor.
const INVALID_ID: &str = "Invalid id";
/// The 429 body of the spent `search` bucket.
const THROTTLED: &str = "Throttled";

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A `GET` of `path` with `token` (`None` anonymous).
async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// The socket peer number `n`: one distinct anonymous client per call, for
/// the throttle test.
fn peer(n: u8) -> SocketAddr {
    SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, n)),
        40_000 + u16::from(n),
    )
}

/// Records `follower`'s accepted follow of `target`.
async fn follow_pair(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, BASE, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        follow::FollowState::Accepted,
        "an open local account answers on the spot"
    );
}

/// The `id` of every entity of an array response, in order.
fn listed_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the body is a JSON array")
        .iter()
        .map(|entry| {
            entry["id"]
                .as_str()
                .expect("an entity carries its id")
                .to_owned()
        })
        .collect()
}

/// The `accounts` array of a search document, as ids.
fn accounts_of(body: &Value) -> Vec<String> {
    listed_ids(&body["accounts"])
}

/// A search document answers exactly the three resource lists.
fn assert_document(body: &Value) {
    let mut keys: Vec<String> = body
        .as_object()
        .expect("the search answer is a JSON object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["accounts", "hashtags", "statuses"],
        "exactly the three lists in every reply"
    );
    assert!(
        body["statuses"].as_array().is_some_and(Vec::is_empty),
        "statuses are not searched yet: {}",
        body["statuses"]
    );
    assert!(
        body["hashtags"].as_array().is_some_and(Vec::is_empty),
        "hashtags are not searched yet: {}",
        body["hashtags"]
    );
}

/// The v1 accounts search answers prefix matches, anonymously, as
/// rendered [`Account`] entities.
#[tokio::test]
async fn accounts_search_answers_prefix_matches_anonymously() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let bob = common::create_local_user_with(&pool, "bob", |mut new| {
        new.name = "Bobby Brown".to_owned();
        new
    })
    .await;
    let _alice = common::create_local_user(&pool, "alice").await;

    let found = get(&app, None, &format!("{ACCOUNTS_SEARCH}?q=bo")).await;
    common::assert_answer(&found, StatusCode::OK, "an anonymous prefix search");
    assert_eq!(listed_ids(&found.json_value()), vec![bob.id.to_string()]);
    let entity = &found.json_value()[0];
    assert_eq!(entity["acct"], "bob", "the handle clients know");
    assert_eq!(entity["username"], "bob", "the nickname part");
    assert_eq!(entity["display_name"], "Bobby Brown", "the stored name");

    let by_name = get(&app, None, &format!("{ACCOUNTS_SEARCH}?q=bobby")).await;
    assert_eq!(
        listed_ids(&by_name.json_value()),
        vec![bob.id.to_string()],
        "the display-name prefix finds the account the nickname prefix misses"
    );
}

/// The v1 accounts search matches trigram typos beyond the prefix and
/// normalises the query's sigil and case.
#[tokio::test]
async fn accounts_search_matches_typos_and_normalises_the_query() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let eve = common::create_remote_user(&pool, "eve", "remote.example").await;

    let typo = get(&app, None, &format!("{ACCOUNTS_SEARCH}?q=ALCE")).await;
    assert_eq!(
        listed_ids(&typo.json_value()),
        vec![alice.id.to_string()],
        "the trigram arm finds the misspelled nickname"
    );
    let typed = get(
        &app,
        None,
        &format!("{ACCOUNTS_SEARCH}?q=%40eve%40remote.example"),
    )
    .await;
    assert_eq!(
        typed.json_value()[0]["acct"],
        "eve@remote.example",
        "the typed handle matches the remote mirror's stored nickname first"
    );
    assert_eq!(
        listed_ids(&typed.json_value()),
        vec![eve.id.to_string()],
        "and nothing else answers it"
    );
}

/// The v1 accounts search ranks exact nicknames above the viewer's follows,
/// follows above other local accounts, and local above remote; `limit` and
/// `offset` walk that order.
#[tokio::test]
async fn accounts_search_ranks_and_pages() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (viewer, token) = fixture(&pool, "viewer", "read").await;
    let followed = common::create_local_user(&pool, "someread").await;
    follow_pair(&pool, &viewer, &followed).await;
    let exact = common::create_local_user(&pool, "so").await;
    let local = common::create_local_user(&pool, "someone").await;
    let mirror = common::create_remote_user(&pool, "someuser", "remote.example").await;

    let ranked = get(&app, Some(&token), &format!("{ACCOUNTS_SEARCH}?q=%40SO")).await;
    assert_eq!(
        listed_ids(&ranked.json_value()),
        vec![
            exact.id.to_string(),
            followed.id.to_string(),
            local.id.to_string(),
            mirror.id.to_string(),
        ],
        "exact, followed, local, remote"
    );

    let first = get(
        &app,
        Some(&token),
        &format!("{ACCOUNTS_SEARCH}?q=so&limit=2"),
    )
    .await;
    assert_eq!(
        listed_ids(&first.json_value()),
        vec![exact.id.to_string(), followed.id.to_string()],
        "the caller's limit sizes the page"
    );
    let tail = get(
        &app,
        Some(&token),
        &format!("{ACCOUNTS_SEARCH}?q=so&limit=2&offset=2"),
    )
    .await;
    assert_eq!(
        listed_ids(&tail.json_value()),
        vec![local.id.to_string(), mirror.id.to_string()],
        "and the offset walks the ranking"
    );
    let past = get(
        &app,
        Some(&token),
        &format!("{ACCOUNTS_SEARCH}?q=so&offset=99"),
    )
    .await;
    assert!(
        past.json_value().as_array().is_some_and(Vec::is_empty),
        "past the end is the empty array, never an error"
    );
}

/// `following=true` restricts the search to the viewer's follows; without
/// a viewer the restriction searches an empty list and answers nothing.
#[tokio::test]
async fn accounts_search_following_filter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (viewer, token) = fixture(&pool, "finder", "read:accounts").await;
    let carol = common::create_local_user(&pool, "carol").await;
    follow_pair(&pool, &viewer, &carol).await;
    let _cardu = common::create_local_user(&pool, "cardu").await;

    let open = get(&app, Some(&token), &format!("{ACCOUNTS_SEARCH}?q=car")).await;
    assert_eq!(
        listed_ids(&open.json_value()).len(),
        2,
        "unrestricted, both car-accounts answer (the followed one first)"
    );
    assert_eq!(
        open.json_value()[0]["id"],
        carol.id.to_string(),
        "and the follow leads its rank class"
    );

    let restricted = get(
        &app,
        Some(&token),
        &format!("{ACCOUNTS_SEARCH}?q=car&following=true"),
    )
    .await;
    assert_eq!(
        listed_ids(&restricted.json_value()),
        vec![carol.id.to_string()],
        "the restriction keeps only the followed account"
    );

    let anonymous = get(&app, None, &format!("{ACCOUNTS_SEARCH}?q=car&following=1")).await;
    common::assert_answer(
        &anonymous,
        StatusCode::OK,
        "anonymous, so nobody is followed",
    );
    assert!(
        anonymous.json_value().as_array().is_some_and(Vec::is_empty),
        "an anonymous following search answers nothing"
    );
}

/// The blocked and deactivated exclusions hold over HTTP: neither shows in
/// the viewer's search, and an unrelated viewer still sees the accounts
/// the viewer merely has a block with.
#[tokio::test]
async fn accounts_search_excludes_blocks_and_deactivated() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (viewer, token) = fixture(&pool, "viewer", "read").await;
    let bane = common::create_local_user(&pool, "banshee").await;
    let cursed = common::create_local_user(&pool, "banished").await;
    let blithe = common::create_local_user(&pool, "blithe").await;
    let gone = common::create_local_user(&pool, "blunder").await;
    let bane_row = wired_be::models::block::block(&pool, BASE, &bane, &viewer)
        .await
        .expect("the first fixture blocks the viewer");
    assert_eq!(
        bane_row.blocked_id, viewer.id,
        "the first block is `banshee` on the viewer"
    );
    let cursed_row = wired_be::models::block::block(&pool, BASE, &viewer, &cursed)
        .await
        .expect("the second fixture is blocked by the viewer");
    assert_eq!(
        cursed_row.blocked_id, cursed.id,
        "the second block is the viewer on `banished`"
    );
    let deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(gone.id)
        .execute(&pool)
        .await
        .expect("the fixture deactivates");
    assert_eq!(deactivated.rows_affected(), 1, "and it was one row");

    let seen = get(&app, Some(&token), &format!("{ACCOUNTS_SEARCH}?q=b")).await;
    assert_eq!(
        listed_ids(&seen.json_value()),
        vec![blithe.id.to_string()],
        "the account that blocked the viewer, the one the viewer blocked, \
         and the deactivated one are all out"
    );
    let stranger = get(&app, None, &format!("{ACCOUNTS_SEARCH}?q=b")).await;
    for visible in [
        bane.id.to_string(),
        cursed.id.to_string(),
        blithe.id.to_string(),
    ] {
        assert!(
            listed_ids(&stranger.json_value()).contains(&visible),
            "an anonymous request sees every live match whatever the viewer's blocks are"
        );
    }
}

/// Auth on the search routes: anonymous passes, a `read`-scoped token
/// passes, a token without `read:accounts` is the 403, an invalid token
/// the 401, and an app-only token proceeds like an anonymous request.
#[tokio::test]
async fn search_auth_gates() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let _bob = common::create_local_user(&pool, "bob").await;
    let (_, write_token) = fixture(&pool, "writer", "write:accounts").await;
    let (_, read_token) = fixture(&pool, "reader", "read").await;
    let app_only = common::create_app_only_token(&pool, "read").await;

    for path in [ACCOUNTS_SEARCH, V1_SEARCH, V2_SEARCH] {
        let target = format!("{path}?q=bo");
        let allowed = get(&app, None, &target).await;
        common::assert_answer(&allowed, StatusCode::OK, &format!("{path} anonymous"));
        let granted = get(&app, Some(&read_token), &target).await;
        common::assert_answer(&granted, StatusCode::OK, &format!("{path} with read"));
        let shared = get(&app, Some(&app_only), &target).await;
        common::assert_answer(&shared, StatusCode::OK, &format!("{path} app-only"));

        let refused = get(&app, Some(&write_token), &target).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("{path} wrong scope"),
        );
        common::assert_error(&refused, NO_READ_ACCOUNTS, &format!("{path} wrong scope"));
        let unknown = get(&app, Some("no-such-token"), &target).await;
        common::assert_answer(
            &unknown,
            StatusCode::UNAUTHORIZED,
            &format!("{path} invalid token"),
        );
        common::assert_error(&unknown, INVALID_TOKEN, &format!("{path} invalid token"));
    }
}

/// An absent, empty or sigil-only `q` answers no accounts on any route,
/// without touching the corpus.
#[tokio::test]
async fn an_empty_query_answers_no_accounts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let _bob = common::create_local_user(&pool, "bob").await;

    for query in ["", "q=", "q=%40", "q=%20%20"] {
        let flat = get(&app, None, &format!("{ACCOUNTS_SEARCH}?{query}")).await;
        common::assert_answer(&flat, StatusCode::OK, "an empty query is an answer");
        assert!(
            flat.json_value().as_array().is_some_and(Vec::is_empty),
            "`{query:?}` searches nothing: {}",
            flat.text()
        );
        let document = get(&app, None, &format!("{V1_SEARCH}?{query}")).await;
        common::assert_answer(&document, StatusCode::OK, "and the document too");
        assert!(
            document.json_value()["accounts"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "`{query:?}` answers no accounts in the document"
        );
    }
}

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

/// A `type` other than `accounts` answers the empty document — statuses
/// and hashtags are searched by later tickets.
#[tokio::test]
async fn other_search_types_answer_empty_documents() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let _bob = common::create_local_user(&pool, "bob").await;

    for kind in ["statuses", "hashtags", "potato"] {
        for path in [V1_SEARCH, V2_SEARCH] {
            let found = get(&app, None, &format!("{path}?q=bo&type={kind}")).await;
            common::assert_answer(&found, StatusCode::OK, "a known resource is no error");
            let body = found.json_value();
            assert_document(&body);
            assert!(
                body["accounts"].as_array().is_some_and(Vec::is_empty),
                "`type={kind}` searches no accounts"
            );
        }
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
