//! Integration tests for the search endpoints (WBE-115, extended by
//! WBE-128): `GET /api/v1/accounts/search` (the flat account array), and
//! `GET /api/v1/search` / `GET /api/v2/search` (the combined document
//! with all three arms live). The routes answer anonymously (Pleroma's
//! `fallback: :proceed_unauthenticated`), demand `read:accounts` from a
//! presented user token, normalise the query (leading `@`, case), page by
//! `limit` (the accounts' default 40 and cap 80, the statuses' and
//! hashtags' default 20 and cap 40) and `offset`, restrict the account
//! search to the viewer's follows on `following=true`, search statuses
//! only for a viewer unless `SEARCH_STATUSES_UNAUTHENTICATED` is on,
//! treat a `#`-leading query as a hashtag-only term, render hashtags as
//! v1 name strings and v2 tag entities, and count into the limiter's
//! `search` bucket.
//!
//! `resolve=true` is the federation arm of the same routes (WBE-192), and it
//! runs only for an authenticated viewer: an actor URL the instance has
//! never seen is fetched from a mock `ActivityPub` peer on loopback and rides
//! first in `accounts`, a status URL is fetched and rendered into `statuses`
//! with its author left out of `accounts`, a URL on this instance's own
//! origin is read from the database alone, and a peer that cannot be reached
//! answers the same empty lists an unresolving search would. `type` keeps the
//! readings to the resource it names, and a resolving request spends the
//! `search` bucket like any other.

#![cfg(test)]

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    extract::{Path, State},
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse as _, Response},
    routing::get as route_get,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};
use wired_be::config::Toggle;
use wired_be::models::{
    block, follow, hashtag,
    mute::{self, MuteOptions},
    status::{self, NewStatus, Visibility},
    user::{self, User},
};

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

/// The `statuses` array of a search document, as ids.
fn statuses_of(body: &Value) -> Vec<String> {
    listed_ids(&body["statuses"])
}

/// Whether the search document's `list` is empty, with the body shown on
/// failure.
fn assert_list_empty(body: &Value, list: &str, who: &str) {
    assert!(
        body[list].as_array().is_some_and(Vec::is_empty),
        "`{list}` is empty for {who}: {body}"
    );
}

/// A search document answers exactly the three resource lists, each as an
/// array (which of them are filled is the case at hand).
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
    for list in ["accounts", "statuses", "hashtags"] {
        assert!(body[list].is_array(), "`{list}` is an array: {body}");
    }
}

/// Whether `id_list` strictly descends, as a newest-first page must.
fn is_descending(id_list: &[String]) -> bool {
    id_list.windows(2).all(|pair| match pair {
        [newer, older] => newer > older,
        _ => false,
    })
}

/// The v1 `type=statuses` page for `query`, answered with `token`: the
/// ids of its `statuses` list, in the order they arrived.
async fn status_ids(app: &Router, token: &str, query: &str) -> Vec<String> {
    let found = get(
        app,
        Some(token),
        &format!("{V1_SEARCH}?q={query}&type=statuses"),
    )
    .await;
    common::assert_answer(&found, StatusCode::OK, "the statuses arm");
    statuses_of(&found.json_value())
}

/// A search document that answers with all three lists empty.
fn assert_all_lists_empty(body: &Value, who: &str) {
    assert_document(body);
    for list in ["accounts", "statuses", "hashtags"] {
        assert_list_empty(body, list, who);
    }
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
    let bane_row = block::block(&pool, BASE, &bane, &viewer)
        .await
        .expect("the first fixture blocks the viewer");
    assert_eq!(
        bane_row.blocked_id, viewer.id,
        "the first block is `banshee` on the viewer"
    );
    let cursed_row = block::block(&pool, BASE, &viewer, &cursed)
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

/// The `statuses` arm full-text searches the stored text: a word in the
/// body or in the content warning answers its status, newest id first;
/// reblog rows, tombstones and a query whose words no single post carries
/// together answer nothing.
#[tokio::test]
async fn statuses_search_matches_body_and_warning_words() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let both =
        common::create_status(&pool, &alice, "the quick brown fox", Visibility::Public).await;
    let warned = status::insert(
        &pool,
        BASE,
        &NewStatus {
            spoiler_text: "fox warning".to_owned(),
            ..NewStatus::local(&alice, "unrelated body")
        },
    )
    .await
    .expect("the warned fixture inserts");
    let _slow = common::create_status(&pool, &bob, "a slow brown cat", Visibility::Public).await;
    let reblog = status::insert(
        &pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(both.id),
            ..NewStatus::local(&bob, "")
        },
    )
    .await
    .expect("the reblog fixture inserts");
    let doomed = common::create_status(&pool, &alice, "doomed fox", Visibility::Public).await;
    assert!(
        status::soft_delete(&pool, doomed.id)
            .await
            .expect("the fixture delete runs")
            .is_some(),
        "one matching post is a tombstone now"
    );

    let ids = status_ids(&app, &token, "fox").await;
    assert_eq!(
        ids.len(),
        2,
        "the body match and the warning match, no reblog row, no tombstone: {ids:?}"
    );
    for id in [warned.id.to_string(), both.id.to_string()] {
        assert!(ids.contains(&id), "the {id} answers: {ids:?}");
    }
    assert!(
        !ids.contains(&reblog.id.to_string()),
        "the reblog row itself is never in: {ids:?}"
    );
    assert!(is_descending(&ids), "newest id first: {ids:?}");
}

/// A matching status renders as a full [`Status`] entity in the list: the
/// stored content and the author's entity ride along, and the conjunctive,
/// unstemmed reading of `plainto_tsquery('simple', …)` holds over HTTP —
/// words AND together, and a misspelled stem finds nothing.
#[tokio::test]
async fn statuses_search_ands_words_and_never_stems() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let both =
        common::create_status(&pool, &alice, "the quick brown fox", Visibility::Public).await;

    let conjunct = status_ids(&app, &token, "quick%20fox").await;
    assert_eq!(
        conjunct,
        vec![both.id.to_string()],
        "only the post carrying every word matches"
    );
    assert!(
        status_ids(&app, &token, "quickly").await.is_empty(),
        "`simple` does not stem: `quickly` is not the word `quick`"
    );

    let body = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q=quick&type=statuses"),
    )
    .await;
    common::assert_answer(&body, StatusCode::OK, "the single-word search");
    let entity = &body.json_value()["statuses"][0];
    assert_eq!(
        entity["content"], "<p>the quick brown fox</p>",
        "the arm renders full Status entities, not ids"
    );
    assert_eq!(entity["account"]["acct"], "alice", "the author rides along");
}

/// The `statuses` arm shows a viewer only what they may see: a private
/// post answers its author and its author's accepted followers, and
/// nobody else.
#[tokio::test]
async fn private_statuses_are_found_only_by_the_follower() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let alice_token = common::create_app_and_token(&pool, &alice, "read").await;
    let bob_token = common::create_app_and_token(&pool, &bob, "read").await;
    let carol_token = common::create_app_and_token(&pool, &carol, "read").await;
    let edge = common::create_status(&pool, &alice, "followers keyword", Visibility::Private).await;
    let open = common::create_status(&pool, &alice, "everyone keyword", Visibility::Public).await;
    let edge_id = edge.id.to_string();
    let open_id = open.id.to_string();

    assert_eq!(
        status_ids(&app, &carol_token, "keyword").await,
        vec![open_id.clone()],
        "a stranger searches only what is open to them"
    );
    assert_eq!(
        status_ids(&app, &bob_token, "keyword").await,
        vec![open_id.clone()],
        "and a not-yet-follower finds no more"
    );

    follow_pair(&pool, &bob, &alice).await;
    let followed = status_ids(&app, &bob_token, "keyword").await;
    assert!(
        followed.contains(&edge_id) && followed.contains(&open_id),
        "the follower finds the private status too: {followed:?}"
    );
    assert!(
        status_ids(&app, &alice_token, "keyword")
            .await
            .contains(&edge_id),
        "and the author always finds their own"
    );
}

/// Neither direction of a block, nor a mute, leaves an author in the
/// blocking/muting viewer's `statuses` search; an unrelated viewer still
/// finds everything public.
#[tokio::test]
async fn status_search_excludes_blocked_and_muted_authors() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (viewer, token) = fixture(&pool, "viewer", "read").await;
    let (_bystander, other_token) = fixture(&pool, "bystander", "read").await;
    let bane = common::create_local_user(&pool, "bane").await;
    let muted = common::create_local_user(&pool, "mutedpal").await;
    let blocked_post =
        common::create_status(&pool, &bane, "keyword from bane", Visibility::Public).await;
    let muted_post =
        common::create_status(&pool, &muted, "keyword from the muted", Visibility::Public).await;
    let edge = block::block(&pool, BASE, &viewer, &bane)
        .await
        .expect("the viewer blocks bane");
    assert_eq!(edge.blocked_id, bane.id, "the fixture block landed");
    let silence = mute::mute(&pool, &viewer, &muted, MuteOptions::default())
        .await
        .expect("the viewer mutes the pal");
    assert_eq!(silence.muted_id, muted.id, "the fixture mute landed");

    let mine = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q=keyword&type=statuses"),
    )
    .await;
    assert_list_empty(
        &mine.json_value(),
        "statuses",
        "the viewer's blocked and muted authors",
    );

    let theirs = get(
        &app,
        Some(&other_token),
        &format!("{V1_SEARCH}?q=keyword&type=statuses"),
    )
    .await;
    let ids = statuses_of(&theirs.json_value());
    for visible in [blocked_post.id.to_string(), muted_post.id.to_string()] {
        assert!(
            ids.contains(&visible),
            "someone else's block and mute hide nothing from a third account: {ids:?}"
        );
    }
}

/// `account_id` narrows the `statuses` arm to that one author, an id
/// nobody posts with answers the empty page, and a value that is not an
/// id is the shared 400.
#[tokio::test]
async fn status_search_account_id_narrows_the_author() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let hers = common::create_status(&pool, &alice, "keyword hers", Visibility::Public).await;
    let _his = common::create_status(&pool, &bob, "keyword his", Visibility::Public).await;

    let filtered = get(
        &app,
        Some(&token),
        &format!(
            "{V1_SEARCH}?q=keyword&type=statuses&account_id={}",
            alice.id
        ),
    )
    .await;
    assert_eq!(
        statuses_of(&filtered.json_value()),
        vec![hers.id.to_string()],
        "only that author's matching posts answer"
    );

    let nobody = get(
        &app,
        Some(&token),
        &format!(
            "{V1_SEARCH}?q=keyword&type=statuses&account_id={}",
            uuid::Uuid::now_v7()
        ),
    )
    .await;
    common::assert_answer(&nobody, StatusCode::OK, "an unknown author is no error");
    assert_list_empty(
        &nobody.json_value(),
        "statuses",
        "an author with no matches",
    );

    for path in [ACCOUNTS_SEARCH, V1_SEARCH, V2_SEARCH] {
        let junk = get(&app, Some(&token), &format!("{path}?q=bo&account_id=nope")).await;
        common::assert_answer(
            &junk,
            StatusCode::BAD_REQUEST,
            &format!("{path} bad account_id"),
        );
        common::assert_error(&junk, INVALID_ID, &format!("{path} bad account_id"));
    }
}

/// The `statuses` arm pages newest-first with `limit` (default 20, cap 40),
/// the `max_id`/`min_id` keyset cursors, and `offset`.
#[tokio::test]
async fn status_search_pages_by_limit_cursors_and_offset() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let alice = common::create_local_user(&pool, "alice").await;
    for n in 0..5 {
        let _posted = common::create_status(
            &pool,
            &alice,
            &format!("paging keyword {n}"),
            Visibility::Public,
        )
        .await;
    }

    let all = status_ids(&app, &token, "paging").await;
    assert_eq!(all.len(), 5, "the whole page answers");
    assert!(is_descending(&all), "`id DESC` throughout: {all:?}");

    let first = status_ids(&app, &token, "paging&limit=2").await;
    assert_eq!(first, all[..2], "limit cuts the page short");
    let overcap = status_ids(&app, &token, "paging&limit=200").await;
    assert_eq!(overcap, all, "the cap sizes the page, never errors");

    let middle = all.get(2).expect("the five-strong page has a middle");
    let older = status_ids(&app, &token, &format!("paging&max_id={middle}")).await;
    assert_eq!(older, all[3..], "max_id answers the strictly older rows");
    let newer = status_ids(&app, &token, &format!("paging&min_id={middle}")).await;
    assert_eq!(
        newer,
        all[..2],
        "min_id answers the newer rows, newest-first"
    );

    let stepped = status_ids(&app, &token, "paging&offset=2&limit=2").await;
    assert_eq!(stepped, all[2..4], "offset walks the page");
    assert!(
        status_ids(&app, &token, "paging&offset=99")
            .await
            .is_empty(),
        "an offset past the end answers nothing"
    );
}

/// The hashtag arm is a name-prefix search that folds the typed `#` and
/// the case; v1 answers plain name strings and v2 answers tag entities
/// whose fresh `history` is the empty list.
#[tokio::test]
async fn hashtag_search_prefix_matches_v1_names_and_v2_tag_entities() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let _tagged =
        common::create_status(&pool, &alice, "#alpha #alphabet #beta", Visibility::Public).await;

    let v1 = get(&app, None, &format!("{V1_SEARCH}?q=alp&type=hashtags")).await;
    common::assert_answer(&v1, StatusCode::OK, "the anonymous hashtag arm");
    assert_eq!(
        v1.json_value()["hashtags"],
        json!(["alpha", "alphabet"]),
        "v1 spells plain names, prefix-only and in name order"
    );

    let v2 = get(&app, None, &format!("{V2_SEARCH}?q=%23ALP&type=hashtags")).await;
    common::assert_answer(&v2, StatusCode::OK, "the typed sigil and case fold");
    assert_eq!(
        v2.json_value()["hashtags"],
        json!([
            { "name": "alpha", "url": format!("{BASE}/tags/alpha"), "history": [] },
            { "name": "alphabet", "url": format!("{BASE}/tags/alphabet"), "history": [] },
        ]),
        "v2 renders tag entities, the uncomputed history an empty list"
    );

    let none = get(&app, None, &format!("{V1_SEARCH}?q=zz&type=hashtags")).await;
    assert_list_empty(&none.json_value(), "hashtags", "a prefix no tag carries");
    let capped = get(
        &app,
        None,
        &format!("{V1_SEARCH}?q=alp&type=hashtags&limit=1"),
    )
    .await;
    assert_eq!(
        capped.json_value()["hashtags"],
        json!(["alpha"]),
        "the page size cuts the name-ordered list"
    );
}

/// With no `type`, one authenticated search fills all three lists at once
/// from the one query.
#[tokio::test]
async fn untyped_search_fills_all_three_lists() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let united = common::create_local_user(&pool, "unitedfan").await;
    let posted = common::create_status(&pool, &united, "united we stand", Visibility::Public).await;
    // The tag row is stored directly, so exactly one status matches the word.
    let _tag = hashtag::find_or_create(&pool, "#united")
        .await
        .expect("the tag fixture stores")
        .expect("united is a usable name");

    let found = get(&app, Some(&token), &format!("{V1_SEARCH}?q=united")).await;
    common::assert_answer(&found, StatusCode::OK, "the combined search");
    let body = found.json_value();
    assert_document(&body);
    assert_eq!(
        accounts_of(&body),
        vec![united.id.to_string()],
        "the account arm carries the prefix match"
    );
    assert_eq!(
        statuses_of(&body),
        vec![posted.id.to_string()],
        "the status arm carries the word match"
    );
    assert_eq!(
        body["hashtags"],
        json!(["united"]),
        "the hashtag arm carries the name match"
    );

    let v2 = get(&app, Some(&token), &format!("{V2_SEARCH}?q=united")).await;
    assert_eq!(
        v2.json_value()["hashtags"],
        json!([{ "name": "united", "url": format!("{BASE}/tags/united"), "history": [] }]),
        "the same combined search, v2's hashtag spelling"
    );
    assert_eq!(
        statuses_of(&v2.json_value()),
        vec![posted.id.to_string()],
        "statuses render identically on both versions"
    );
}

/// A query that opens with `#` is a hashtag-only search term: untyped, it
/// fills the hashtag list alone, whatever else in the corpus spells the
/// same word.
#[tokio::test]
async fn a_hashtag_query_searches_hashtags_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let united = common::create_local_user(&pool, "unitedfan").await;
    let _posted =
        common::create_status(&pool, &united, "united we stand", Visibility::Public).await;
    let _tagged = common::create_status(&pool, &united, "#united", Visibility::Public).await;

    let found = get(&app, Some(&token), &format!("{V1_SEARCH}?q=%23united")).await;
    common::assert_answer(&found, StatusCode::OK, "the sigil query");
    let body = found.json_value();
    assert_document(&body);
    assert_eq!(
        body["hashtags"],
        json!(["united"]),
        "the hashtag list answers the term"
    );
    assert_list_empty(&body, "accounts", "a `#`-query is hashtag-only");
    assert_list_empty(&body, "statuses", "a `#`-query is hashtag-only");
}

/// The statuses gate: an anonymous request searches no statuses while
/// `SEARCH_STATUSES_UNAUTHENTICATED` is off (the default) — the accounts
/// and hashtag arms answer it fine — and once an operator turns the flag
/// on, it finds the public text, with `public` still the ceiling.
#[tokio::test]
async fn anonymous_status_search_needs_the_config_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let keyworduser = common::create_local_user(&pool, "keyworduser").await;
    let open = common::create_status(
        &pool,
        &keyworduser,
        "keyword public post",
        Visibility::Public,
    )
    .await;
    let _kept = common::create_status(
        &pool,
        &keyworduser,
        "keyword private post",
        Visibility::Private,
    )
    .await;
    // The tag row is stored directly, so exactly the two posts carry the
    // word in their text and the hashtag arm still answers it.
    let _tag = hashtag::find_or_create(&pool, "#keyword")
        .await
        .expect("the tag fixture stores")
        .expect("keyword is a usable name");

    let gated = get(&app, None, &format!("{V1_SEARCH}?q=keyword&type=statuses")).await;
    common::assert_answer(
        &gated,
        StatusCode::OK,
        "the gated arm is an answer, not an error",
    );
    assert_list_empty(&gated.json_value(), "statuses", "anonymous, flag off");
    let untyped = get(&app, None, &format!("{V1_SEARCH}?q=keyword")).await;
    let body = untyped.json_value();
    assert_list_empty(&body, "statuses", "anonymous untyped, flag off");
    assert_eq!(
        body["hashtags"],
        json!(["keyword"]),
        "the ungated arms still answer the anonymous request"
    );
    assert_eq!(
        accounts_of(&body),
        vec![keyworduser.id.to_string()],
        "accounts included"
    );

    let mut config = common::test_config();
    config.search_statuses_unauthenticated = Toggle::new(true);
    let open_app = common::test_app_with_config(pool.clone(), config);
    let found = get(
        &open_app,
        None,
        &format!("{V1_SEARCH}?q=keyword&type=statuses"),
    )
    .await;
    common::assert_answer(&found, StatusCode::OK, "the flag opens the arm");
    assert_eq!(
        statuses_of(&found.json_value()),
        vec![open.id.to_string()],
        "public text answers, `public` and `unlisted` being the anonymous ceiling"
    );
}

/// An absent, empty, whitespace or bare-`#` query fills no list for an
/// authenticated viewer either — the new arms answer nothing, never the
/// whole corpus.
#[tokio::test]
async fn an_empty_query_fills_no_list_anywhere() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let _posted = common::create_status(&pool, &alice, "corpus keyword", Visibility::Public).await;
    let _tagged = common::create_status(&pool, &alice, "#corpus", Visibility::Public).await;

    for query in ["", "q=", "q=%20%20", "q=%23"] {
        for kind in ["&type=statuses", "&type=hashtags", ""] {
            let found = get(&app, Some(&token), &format!("{V1_SEARCH}?{query}{kind}")).await;
            common::assert_answer(&found, StatusCode::OK, "`{query:?}` is an answer");
            assert_all_lists_empty(
                &found.json_value(),
                &format!("`{query:?}` with `{kind}` asked nothing"),
            );
        }
    }
}

/// The public addressing a fetched note needs to ingest as the world's.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// A mock `ActivityPub` peer on loopback: `zoe`'s actor document and one
/// public note per name, with the request counter the "was this attempted at
/// all" assertions read.
struct MockPeer {
    /// The peer's `http://{addr}` origin.
    base: String,
    /// Requests it has answered.
    hits: Arc<AtomicUsize>,
    /// The task serving it, aborted when the test is done.
    server: JoinHandle<()>,
}

/// The mock's shared state: the origin its documents must name themselves
/// with, beside the hit counter.
#[derive(Clone)]
struct Peer {
    /// The `http://{addr}` origin.
    base: String,
    /// Requests answered.
    hits: Arc<AtomicUsize>,
}

/// A body labelled the way an `ActivityPub` peer labels one.
fn ap_json(document: &Value) -> Response {
    (
        [(CONTENT_TYPE, "application/activity+json")],
        document.to_string(),
    )
        .into_response()
}

/// Counts the hit and answers `zoe`'s self-identifying actor document.
async fn serve_actor(State(peer): State<Peer>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    let id = format!("{}/users/zoe", peer.base);
    ap_json(&json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": "zoe",
        "name": "Zoe",
        "inbox": format!("{id}/inbox"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    }))
}

/// Counts the hit and answers the self-identifying public note named by the
/// path, authored by this peer's `zoe`.
async fn serve_object(State(peer): State<Peer>, Path(name): Path<String>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    ap_json(&json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Note",
        "id": format!("{}/objects/{name}", peer.base),
        "attributedTo": format!("{}/users/zoe", peer.base),
        "to": [PUBLIC],
        "content": format!("<p>remote post {name}</p>"),
        "published": "2024-03-01T10:00:00.000Z",
    }))
}

impl MockPeer {
    /// Binds a port and serves `zoe` at `{base}/users/zoe` and any note at
    /// `{base}/objects/{name}`.
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock peer binds a port");
        let address = listener.local_addr().expect("a bound address");
        let peer = Peer {
            base: format!("http://{address}"),
            hits: Arc::new(AtomicUsize::new(0)),
        };
        Self {
            base: peer.base.clone(),
            hits: Arc::clone(&peer.hits),
            server: tokio::spawn(async move {
                let app = Router::new()
                    .route("/users/{nick}", route_get(serve_actor))
                    .route("/objects/{name}", route_get(serve_object))
                    .with_state(peer);
                if let Err(err) = axum::serve(listener, app).await {
                    tracing::debug!("mock peer stopped: {err}");
                }
            }),
        }
    }

    /// The actor URL this peer serves.
    fn actor_url(&self) -> String {
        format!("{}/users/zoe", self.base)
    }

    /// The object URL of fixture note `name`.
    fn object_url(&self, name: &str) -> String {
        format!("{}/objects/{name}", self.base)
    }

    /// The requests answered so far.
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for MockPeer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The origin of a port nothing listens on any more: every request to it
/// fails to connect, which is how an unreachable peer reads here without
/// waiting on any network or timer.
async fn dead_origin() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port is free to bind");
    let base = format!("http://{}", listener.local_addr().expect("a bound address"));
    drop(listener);
    base
}

/// A resolving search on an actor URL the instance has never seen runs the
/// federation round trip: the mock peer's actor is fetched, mirrored into
/// `users`, and rides first in `accounts` — on the combined document of both
/// versions and on the flat account search alike.
#[tokio::test]
async fn resolve_fetches_an_unknown_actor_by_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let peer = MockPeer::start().await;
    let url = peer.actor_url();
    let app = common::test_app(pool.clone());

    for path in [V1_SEARCH, V2_SEARCH] {
        let found = get(&app, Some(&token), &format!("{path}?q={url}&resolve=true")).await;
        common::assert_answer(&found, StatusCode::OK, "the resolving search");
        let body = found.json_value();
        assert_document(&body);
        assert_eq!(
            accounts_of(&body),
            vec![zoe_id(&pool, &url).await],
            "{path} answers the fetched account first"
        );
        assert_list_empty(&body, "statuses", "an actor URL is no post");
    }

    let flat = get(
        &app,
        Some(&token),
        &format!("{ACCOUNTS_SEARCH}?q={url}&resolve=true"),
    )
    .await;
    common::assert_answer(&flat, StatusCode::OK, "the flat account search");
    assert_eq!(
        listed_ids(&flat.json_value()),
        vec![zoe_id(&pool, &url).await],
        "and it answers the same account, as a flat array"
    );
    assert_eq!(
        flat.json_value()[0]["acct"],
        "zoe@127.0.0.1",
        "the handle decision 7 stores the mirror under"
    );
}

/// The stored mirror id of the mock peer's `zoe`.
async fn zoe_id(pool: &PgPool, ap_id: &str) -> String {
    user::get_by_ap_id(pool, ap_id)
        .await
        .unwrap()
        .expect("the resolved actor is mirrored")
        .id
        .to_string()
}

/// A resolving search on a status URL fetches the remote note, stores it, and
/// answers it rendered for the viewer in `statuses` — with its author left
/// out of `accounts`: the account arm ran on the rows stored before the
/// resolve, and the resolution adds nothing to it.
#[tokio::test]
async fn resolve_fetches_a_remote_status_by_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let peer = MockPeer::start().await;
    let url = peer.object_url("first");
    let app = common::test_app(pool.clone());

    let found = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q={url}&resolve=true"),
    )
    .await;
    common::assert_answer(&found, StatusCode::OK, "the resolving status search");
    let body = found.json_value();
    assert_document(&body);
    let resolved = status_rows_by_uri(&pool, &url).await;
    assert_eq!(
        statuses_of(&body),
        resolved,
        "the fetched note is the statuses list"
    );
    assert_list_empty(&body, "accounts", "its author is not duplicated there");
    let entity = &body["statuses"][0];
    assert_eq!(entity["uri"], url, "the entity carries the object id");
    assert_eq!(
        entity["content"], "<p>remote post first</p>",
        "and its content"
    );
    assert_eq!(
        entity["account"]["acct"], "zoe@127.0.0.1",
        "rendered with its author"
    );

    let repeat = get(
        &app,
        Some(&token),
        &format!("{V2_SEARCH}?q={url}&resolve=true"),
    )
    .await;
    assert_eq!(
        statuses_of(&repeat.json_value()),
        resolved,
        "the stored row answers the next search too"
    );
}

/// The ids of the live statuses stored under `uri`, as a list.
async fn status_rows_by_uri(pool: &PgPool, uri: &str) -> Vec<String> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT id::text FROM statuses WHERE uri = $1")
        .bind(uri)
        .fetch_all(pool)
        .await
        .unwrap();
    rows.into_iter().map(|(id,)| id).collect()
}

/// Every local URL form the ticket names resolves through the database, with
/// no peer to ask: the account behind `/users/{nick}`, and the post behind
/// `/objects/{id}`, `/notice/{id}` and `/users/{nick}/statuses/{id}`.
#[tokio::test]
async fn resolve_reads_local_urls_from_the_database() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let posted = common::create_local_user(&pool, "poster").await;
    let row =
        common::create_status(&pool, &posted, "a local post to find", Visibility::Public).await;
    let app = common::test_app(pool.clone());

    let cases = vec![
        (
            posted.ap_id.clone(),
            vec![posted.id.to_string()],
            Vec::new(),
        ),
        (
            format!("{BASE}/users/poster"),
            vec![posted.id.to_string()],
            Vec::new(),
        ),
        (row.uri.clone(), Vec::new(), vec![row.id.to_string()]),
        (
            row.url
                .clone()
                .expect("a local status carries its notice url"),
            Vec::new(),
            vec![row.id.to_string()],
        ),
        (
            format!("{BASE}/users/poster/statuses/{}", row.id),
            Vec::new(),
            vec![row.id.to_string()],
        ),
    ];
    for (url, want_accounts, want_statuses) in cases {
        let found = get(
            &app,
            Some(&token),
            &format!("{V1_SEARCH}?q={url}&resolve=true"),
        )
        .await;
        common::assert_answer(&found, StatusCode::OK, "the local resolve");
        let body = found.json_value();
        assert_eq!(accounts_of(&body), want_accounts, "`{url}` accounts");
        assert_eq!(statuses_of(&body), want_statuses, "`{url}` statuses");
    }

    let gone = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q={BASE}/users/nobody&resolve=true"),
    )
    .await;
    common::assert_answer(&gone, StatusCode::OK, "an unknown local actor");
    assert_all_lists_empty(&gone.json_value(), "a local URL with no row behind it");
}

/// A resolved status the viewer may not see is left out of the answer: the
/// same `/notice/{id}` is the author's own and nobody else's.
#[tokio::test]
async fn resolve_omits_a_status_the_viewer_may_not_see() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (author, author_token) = fixture(&pool, "author", "read").await;
    let (_other, other_token) = fixture(&pool, "other", "read").await;
    let secret = common::create_status(&pool, &author, "author only", Visibility::Private).await;
    let url = secret.url.clone().expect("a local notice url");
    let app = common::test_app(pool.clone());

    let hidden = get(
        &app,
        Some(&other_token),
        &format!("{V1_SEARCH}?q={url}&resolve=true"),
    )
    .await;
    common::assert_answer(&hidden, StatusCode::OK, "the refusing answer is an answer");
    assert_all_lists_empty(&hidden.json_value(), "a viewer the private post excludes");

    let shown = get(
        &app,
        Some(&author_token),
        &format!("{V1_SEARCH}?q={url}&resolve=true"),
    )
    .await;
    assert_eq!(
        statuses_of(&shown.json_value()),
        vec![secret.id.to_string()],
        "the author reads their own post"
    );
}

/// A peer that cannot be reached is the empty answer, not an error: both
/// readings fail quietly, and the stored search's own lists still answer
/// beside them.
#[tokio::test]
async fn resolve_is_silent_for_an_unreachable_host() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let known = common::create_local_user(&pool, "resolvable").await;
    let dead = dead_origin().await;
    let app = common::test_app(pool.clone());

    for path in ["/users/ghost", "/objects/ghost"] {
        let query = format!("{dead}{path}");
        let found = get(
            &app,
            Some(&token),
            &format!("{V1_SEARCH}?q={query}&resolve=true"),
        )
        .await;
        common::assert_answer(&found, StatusCode::OK, "an unreachable peer is no error");
        let body = found.json_value();
        assert_document(&body);
        assert_list_empty(&body, "statuses", &format!("`{query}` resolves nothing"));
        assert_list_empty(&body, "accounts", &format!("`{query}` resolves nothing"));
    }

    let flagged = get(
        &app,
        Some(&token),
        &format!("{ACCOUNTS_SEARCH}?q=resolv&resolve=true"),
    )
    .await;
    let plain = get(&app, Some(&token), &format!("{ACCOUNTS_SEARCH}?q=resolv")).await;
    let ids = listed_ids(&flagged.json_value());
    assert!(
        ids.contains(&known.id.to_string()),
        "the stored search still answers beside the flag: {ids:?}"
    );
    assert_eq!(
        ids,
        listed_ids(&plain.json_value()),
        "and a resolve that failed changes nothing about the answer"
    );
}

/// Resolution is a viewer's action: the anonymous `resolve=true` request is
/// the ordinary search, even with a live peer behind the URL, while the same
/// request with a token fetches.
#[tokio::test]
async fn resolve_needs_an_authenticated_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let peer = MockPeer::start().await;
    let url = peer.actor_url();
    let app = common::test_app(pool.clone());

    let anonymous = get(&app, None, &format!("{V1_SEARCH}?q={url}&resolve=true")).await;
    common::assert_answer(
        &anonymous,
        StatusCode::OK,
        "the anonymous search is answered",
    );
    assert_all_lists_empty(
        &anonymous.json_value(),
        "an anonymous resolve=true asks nothing of the network",
    );
    assert_eq!(peer.hits(), 0, "and the peer never heard about it");

    let signed = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q={url}&resolve=true"),
    )
    .await;
    assert_eq!(
        accounts_of(&signed.json_value()).len(),
        1,
        "the same request with a viewer fetches"
    );
    assert!(peer.hits() > 0, "and the peer answered it");
}

/// `type` keeps a resolving search to the resource it names: no account is
/// fetched for `type=statuses` (the peer is never asked), and no post for
/// `type=accounts` (the status reading never runs).
#[tokio::test]
async fn resolve_attempts_only_the_requested_type() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let peer = MockPeer::start().await;
    let app = common::test_app(pool.clone());

    let statuses_only = get(
        &app,
        Some(&token),
        &format!(
            "{V1_SEARCH}?q={}&resolve=true&type=statuses",
            peer.actor_url()
        ),
    )
    .await;
    common::assert_answer(&statuses_only, StatusCode::OK, "the typed search");
    assert_list_empty(
        &statuses_only.json_value(),
        "accounts",
        "type=statuses resolves no account",
    );
    assert!(
        user::get_by_ap_id(&pool, &peer.actor_url())
            .await
            .unwrap()
            .is_none(),
        "the actor reading never ran: no account was mirrored"
    );

    let accounts_only = get(
        &app,
        Some(&token),
        &format!(
            "{V1_SEARCH}?q={}&resolve=true&type=accounts",
            peer.object_url("second")
        ),
    )
    .await;
    let body = accounts_only.json_value();
    assert_list_empty(&body, "statuses", "type=accounts resolves no post");
    assert_list_empty(&body, "accounts", "and a note is no actor");
    assert_eq!(
        stored_notes(&pool, "second").await,
        0,
        "the status reading never ran: nothing was stored"
    );
}

/// The number of stored statuses whose uri ends in fixture note `name`.
async fn stored_notes(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri LIKE $1")
        .bind(format!("%/objects/{name}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A handle the instance already stores for exact is answered by that row:
/// `resolve=true` changes nothing about the result (and, with no peer behind
/// its domain, could not have fetched anything else).
#[tokio::test]
async fn resolve_leaves_an_exact_handle_match_as_it_is() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let eve = common::create_remote_user(&pool, "eve", "unlisted.example").await;
    let app = common::test_app(pool.clone());

    for flag in ["", "&resolve=true"] {
        let found = get(
            &app,
            Some(&token),
            &format!("{ACCOUNTS_SEARCH}?q=%40eve%40unlisted.example{flag}"),
        )
        .await;
        common::assert_answer(&found, StatusCode::OK, "the stored handle");
        assert_eq!(
            listed_ids(&found.json_value()),
            vec![eve.id.to_string()],
            "`resolve=true` is the same answer as without it"
        );
    }
}

/// A resolving request spends the `search` bucket like any other: the same
/// token's two requests inside a budget of two, the third refused before its
/// lookup ever runs.
#[tokio::test]
async fn a_resolving_request_spends_the_search_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let dead = dead_origin().await;
    let config = common::test_config_with_rate_limits("search=2:600");
    let app = common::test_app_with_config(pool, config);

    for remaining in ["1", "0"] {
        let answer =
            common::TestRequest::get(&format!("{V1_SEARCH}?q={dead}/users/ghost&resolve=true"))
                .bearer(&token)
                .send(&app)
                .await;
        common::assert_answer(&answer, StatusCode::OK, "inside the budget");
        assert_eq!(
            answer.header("x-ratelimit-remaining"),
            Some(remaining),
            "the resolving request counted down the bucket"
        );
    }

    let refused =
        common::TestRequest::get(&format!("{V1_SEARCH}?q={dead}/users/ghost&resolve=true"))
            .bearer(&token)
            .send(&app)
            .await;
    common::assert_answer(&refused, StatusCode::TOO_MANY_REQUESTS, "the spent window");
    common::assert_error(&refused, THROTTLED, "decision 8's body");
}
