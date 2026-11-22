//! The fixtures, request helpers and assertions the list member tests share.
//! The test modules are its children, so they reach every helper through
//! `use super::*`.

mod gates;
mod listing;
mod writes;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowState},
        list::{self, List as Stored, NewList},
        user::{Flag, NewUser, User},
    },
    pagination::MAX_LIMIT,
};

use crate::common;

/// The list route the member routes hang under.
const ROOT: &str = "/api/v1/lists";
/// The instance `test_config` serves as: what the `Link` header spells its
/// targets with.
const BASE: &str = "https://test.example";
/// The members route's page cap, [`MAX_LIMIT`] spelled for a `Vec::len`.
const CAP: usize = 40;
/// 422 body for an account the caller does not follow (the ticket's text).
const NOT_FOLLOWED: &str = "Validation failed: Account is not followed";

/// The fixture every membership test starts from: `alice` with a token for
/// both halves of the scope pair and one empty list of her own, and the three
/// accounts she follows (so any of them is a member she may add).
#[derive(Debug)]
struct Scene {
    /// The owner of the list, and the caller every request authenticates as.
    alice: User,
    /// Her bearer token, granting `read:lists write:lists`.
    token: String,
    /// Her empty list.
    list: Stored,
    /// The first account she follows.
    bob: User,
    /// The second.
    carol: User,
    /// The third.
    dave: User,
}

impl Scene {
    /// Builds it: four local users, alice following the other three, alice's
    /// list titled `Friends`.
    async fn new(pool: &PgPool) -> Self {
        let alice = user(pool, "alice").await;
        let bob = user(pool, "bob").await;
        let carol = user(pool, "carol").await;
        let dave = user(pool, "dave").await;
        for followed in [&bob, &carol, &dave] {
            follow_user(pool, &alice, followed).await;
        }
        Self {
            token: token(pool, &alice, "read:lists write:lists").await,
            list: seed(pool, &alice, "Friends").await,
            alice,
            bob,
            carol,
            dave,
        }
    }

    /// The three followed accounts' ids, in the order a batch adds them.
    fn ids(&self) -> Vec<Uuid> {
        vec![self.bob.id, self.carol.id, self.dave.id]
    }
}

/// A local user named `nickname`.
async fn user(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user(pool, nickname).await
}

/// A local user named `nickname` and a bearer token granting `scopes`.
async fn user_and_token(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = user(pool, nickname).await;
    let bearer = token(pool, &created, scopes).await;
    (created, bearer)
}

/// A bearer token for `who` granting `scopes`.
async fn token(pool: &PgPool, who: &User, scopes: &str) -> String {
    common::create_app_and_token(pool, who, scopes).await
}

/// A local user named `nickname` whose account is locked, so a follow of it
/// waits for an answer instead of being granted.
async fn locked_user(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user_with(pool, nickname, |new| NewUser {
        locked: Flag::new(true),
        ..new
    })
    .await
}

/// Creates a list titled `title` for `owner` through the model.
async fn seed(pool: &PgPool, owner: &User, title: &str) -> Stored {
    list::create(
        pool,
        owner.id,
        &NewList {
            title: title.to_owned(),
            ..NewList::default()
        },
    )
    .await
    .expect("fixture list creates")
}

/// Grants `follower` a follow of `target`: the state the membership rule reads.
async fn follow_user(pool: &PgPool, follower: &User, target: &User) {
    let granted = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("fixture follow is granted");
    assert_eq!(
        granted.state,
        FollowState::Accepted,
        "the fixture target is unlocked and local"
    );
}

/// The members path of one list.
fn members_path(list_id: Uuid) -> String {
    format!("{ROOT}/{list_id}/accounts")
}

/// `GET /api/v1/lists/{id}/accounts?{query}` as `bearer`.
async fn members(app: &Router, bearer: &str, list_id: Uuid, query: &str) -> common::TestResponse {
    let path = if query.is_empty() {
        members_path(list_id)
    } else {
        format!("{}?{query}", members_path(list_id))
    };
    common::TestRequest::get(&path)
        .bearer(bearer)
        .send(app)
        .await
}

/// `GET …/accounts` as `bearer`, asserted 200 and parsed.
async fn listed(app: &Router, bearer: &str, list_id: Uuid, query: &str) -> Vec<Value> {
    let response = members(app, bearer, list_id, query).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the members answer 200 (query {query:?})"
    );
    response.json()
}

/// The `acct` of every element of `rows`, in order.
fn accts(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .map(|row| {
            row["acct"]
                .as_str()
                .expect("every account carries its acct")
        })
        .collect()
}

/// The `id` of every element of `rows`, in order.
fn ids(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .map(|row| row["id"].as_str().expect("every account carries its id"))
        .collect()
}

/// `POST /api/v1/lists/{id}/accounts` with the JSON `body` as `bearer`.
async fn add_json(app: &Router, bearer: &str, list_id: Uuid, body: Value) -> common::TestResponse {
    common::TestRequest::post(&members_path(list_id))
        .bearer(bearer)
        .json(&body)
        .send(app)
        .await
}

/// `POST …/accounts` naming `members`, asserted 200 with the empty object.
async fn added(app: &Router, bearer: &str, list_id: Uuid, members: &[Uuid]) {
    let response = add_json(app, bearer, list_id, account_ids(members)).await;
    assert_eq!(response.status, StatusCode::OK, "the fixture add succeeds");
    assert_eq!(
        response.json_value(),
        json!({}),
        "and answers the empty object"
    );
}

/// `POST …/accounts` with the same batch as an
/// `application/x-www-form-urlencoded` body.
async fn add_form(
    app: &Router,
    bearer: &str,
    list_id: Uuid,
    members: &[Uuid],
) -> common::TestResponse {
    let texts = account_ids_text(members);
    common::TestRequest::post(&members_path(list_id))
        .bearer(bearer)
        .form(&account_fields(&texts))
        .send(app)
        .await
}

/// `DELETE …/accounts` with the JSON `body` as `bearer`.
async fn remove_json(
    app: &Router,
    bearer: &str,
    list_id: Uuid,
    body: Value,
) -> common::TestResponse {
    common::TestRequest::delete(&members_path(list_id))
        .bearer(bearer)
        .json(&body)
        .send(app)
        .await
}

/// `DELETE …/accounts?{query}`, with no body at all, as `bearer`.
async fn remove_query(
    app: &Router,
    bearer: &str,
    list_id: Uuid,
    query: &str,
) -> common::TestResponse {
    common::TestRequest::delete(&format!("{}?{query}", members_path(list_id)))
        .bearer(bearer)
        .send(app)
        .await
}

/// The `account_ids` JSON body naming `members`.
fn account_ids(members: &[Uuid]) -> Value {
    json!({"account_ids": account_ids_text(members)})
}

/// The `account_ids` of `members` as the strings a client sends.
fn account_ids_text(members: &[Uuid]) -> Vec<String> {
    members.iter().map(ToString::to_string).collect()
}

/// The form fields an `account_ids[]` array of `texts` is spelled with.
fn account_fields(texts: &[String]) -> Vec<(&'static str, &str)> {
    texts
        .iter()
        .map(|text| ("account_ids[]", text.as_str()))
        .collect()
}

/// The `Link` header's cursor of a `rel` under a query `key`, so the page-walk
/// assertions read as the header they came from.
fn cursor(link: &str, rel: &str, key: &str) -> String {
    let marker = format!("rel=\"{rel}\"");
    let entry = link
        .split(',')
        .find(|entry| entry.contains(&marker))
        .unwrap_or_else(|| panic!("the link header names {rel}: {link}"));
    // The url is what sits between the angle brackets of that entry.
    let url = entry
        .split(['<', '>'])
        .nth(1)
        .unwrap_or_else(|| panic!("a bracketed url in {entry}"));
    let Some((_, query)) = url.split_once('?') else {
        panic!("the link carries a query: {url}");
    };
    let pair = query
        .split('&')
        .find(|pair| pair.starts_with(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key} cursor in {query}"));
    let Some((_, value)) = pair.split_once('=') else {
        panic!("a cursor is a key and a value: {pair}");
    };
    value.to_owned()
}

/// The `Account` entity keys a member entry must render.
const ACCOUNT_KEYS: [&str; 6] = [
    "url",
    "acct",
    "display_name",
    "created_at",
    "followers_count",
    "pleroma",
];

/// One member entry is a rendered `Account`: every key of the entity there.
fn assert_account_keys(first: &Value) {
    for key in ACCOUNT_KEYS {
        assert!(
            first.get(key).is_some(),
            "and every `Account` entity key `{key}` is rendered"
        );
    }
}

/// The cursor semantics over the same list: `max_id` walks to the older
/// membership, `min_id` and `since_id` walk back from there, newest first.
async fn assert_cursor_walk(app: &Router, token: &str, list: Uuid, next: &str) {
    let older = members(app, token, list, &format!("limit=2&max_id={next}")).await;
    assert_eq!(
        accts(&older.json::<Vec<Value>>()),
        ["bob"],
        "max_id walks to the membership added before the cursor"
    );
    let back = cursor(
        older
            .header("link")
            .expect("one row is a page, and a page links"),
        "prev",
        "min_id",
    );
    let back_again = listed(app, token, list, &format!("limit=2&min_id={back}")).await;
    assert_eq!(
        accts(&back_again),
        ["dave", "carol"],
        "min_id walks back from the older page, still newest first"
    );
    let forward = listed(app, token, list, &format!("limit=2&since_id={back}")).await;
    assert_eq!(
        accts(&forward),
        ["dave", "carol"],
        "and since_id reads everything newer than the cursor it polls with"
    );
}

/// The `Link` header a members page carries: Pleroma's `add_link_headers`
/// over the position ordinals of the page, on the request's own `path`, with
/// nothing else in the query.
fn assert_members_link(link: &str, path: &str, next: &str, newest: &str) {
    let expected = format!(
        "<{BASE}{path}?max_id={next}>; rel=\"next\", <{BASE}{path}?min_id={newest}>; rel=\"prev\""
    );
    assert_eq!(
        link, expected,
        "the Link header is the two cursors and nothing else"
    );
}

/// `DELETE`s the members of `scene`'s list, naming `ids`, through the JSON
/// body.
async fn removed(app: &Router, scene: &Scene, ids: &[Uuid]) -> common::TestResponse {
    let texts: Vec<String> = ids.iter().map(ToString::to_string).collect();
    remove_json(
        app,
        &scene.token,
        scene.list.id,
        json!({"account_ids": texts}),
    )
    .await
}

/// The members `scene`'s list still holds, by nickname, in page order.
async fn assert_members(app: &Router, scene: &Scene, want: &[&str], why: &str) {
    let listed = listed(app, &scene.token, scene.list.id, "limit=0").await;
    assert_eq!(accts(&listed), want.to_vec(), "{why}");
}
