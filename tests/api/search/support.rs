//! Shared consts and fixture helpers for every submodule of the `search`
//! area: the origins and route paths the tests request, the canned error
//! bodies they assert, the thin wrappers around the HTTP harness, and the
//! readers of the search document's three lists. Everything here is
//! `pub(super)`, and a submodule imports exactly the members it calls.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use wired_be::models::{follow, user::User};

use crate::common;

/// The instance origin the fixtures live on (`Config::for_tests`).
pub(super) const BASE: &str = wired_be::config::TEST_BASE_URL;
/// The flat account-array search.
pub(super) const ACCOUNTS_SEARCH: &str = "/api/v1/accounts/search";
/// The combined v1 search document.
pub(super) const V1_SEARCH: &str = "/api/v1/search";
/// The combined v2 search document.
pub(super) const V2_SEARCH: &str = "/api/v2/search";
/// The 401 body for a token that does not resolve.
pub(super) const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body of a search whose presented token lacks `read:accounts`.
pub(super) const NO_READ_ACCOUNTS: &str = "Insufficient permissions: read:accounts.";
/// The 400 body of an unreadable pagination cursor.
pub(super) const INVALID_ID: &str = "Invalid id";
/// The 429 body of the spent `search` bucket.
pub(super) const THROTTLED: &str = "Throttled";

/// A local user named `nickname` plus a bearer token granting `scopes`.
pub(super) async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A `GET` of `path` with `token` (`None` anonymous).
pub(super) async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// The socket peer number `n`: one distinct anonymous client per call, for
/// the throttle test.
pub(super) fn peer(n: u8) -> SocketAddr {
    SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, n)),
        40_000 + u16::from(n),
    )
}

/// Records `follower`'s accepted follow of `target`.
pub(super) async fn follow_pair(pool: &PgPool, follower: &User, target: &User) {
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
pub(super) fn listed_ids(body: &Value) -> Vec<String> {
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
pub(super) fn accounts_of(body: &Value) -> Vec<String> {
    listed_ids(&body["accounts"])
}

/// The `statuses` array of a search document, as ids.
pub(super) fn statuses_of(body: &Value) -> Vec<String> {
    listed_ids(&body["statuses"])
}

/// Whether the search document's `list` is empty, with the body shown on
/// failure.
pub(super) fn assert_list_empty(body: &Value, list: &str, who: &str) {
    assert!(
        body[list].as_array().is_some_and(Vec::is_empty),
        "`{list}` is empty for {who}: {body}"
    );
}

/// A search document answers exactly the three resource lists, each as an
/// array (which of them are filled is the case at hand).
pub(super) fn assert_document(body: &Value) {
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
pub(super) fn is_descending(id_list: &[String]) -> bool {
    id_list.windows(2).all(|pair| match pair {
        [newer, older] => newer > older,
        _ => false,
    })
}

/// The v1 `type=statuses` page for `query`, answered with `token`: the
/// ids of its `statuses` list, in the order they arrived.
pub(super) async fn status_ids(app: &Router, token: &str, query: &str) -> Vec<String> {
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
pub(super) fn assert_all_lists_empty(body: &Value, who: &str) {
    assert_document(body);
    for list in ["accounts", "statuses", "hashtags"] {
        assert_list_empty(body, list, who);
    }
}
