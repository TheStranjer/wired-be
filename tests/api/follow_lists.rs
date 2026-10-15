//! Integration tests for `GET /api/v1/accounts/{id}/followers` and
//! `GET /api/v1/accounts/{id}/following`: the accepted followers and
//! followees as `Account` entities, newest follow first; keyset
//! pagination whose cursors are `follows` row ids, with Pleroma's
//! `Link` header over them and no header on an empty page; the default
//! page of twenty capped at forty; the privacy `cond` — a
//! `hide_followers` (resp. `hide_follows`) account answers the empty
//! array to everyone but itself, while the count-hiding flags alone
//! leave the list intact; the optional auth with its `read:accounts`
//! gate (403 for a user token that lacks it, anonymous and app-only
//! requests proceed, a bad token is the 401); pending follows excluded
//! from every listing; remote mirrors listed alongside local accounts;
//! locked accounts' lists as visible as any other's; every account miss
//! the same 404 and a malformed cursor the extractor's 400; and each
//! entry embedding the viewer's `pleroma.relationship`.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, Follow, FollowState},
        user::{Flag, User},
    },
};

use crate::common;

/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = "https://test.example";
/// The body every account miss on these routes carries (decision 8).
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 401 body `MaybeAuthUser` renders for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body the scope gate renders for a token without the scope.
const NO_SCOPE: &str = r#"{"error":"Insufficient permissions: read:accounts."}"#;
/// The incoming side of the relationship, one of the two listing routes.
const FOLLOWERS: &str = "followers";
/// The outgoing side of the relationship, the counterpart route.
const FOLLOWING: &str = "following";

/// One fixture relationship: the listed account's id as the entity shows
/// it, next to the id of the `follows` row that put them on the page.
type Relationship = (String, Uuid);

/// The listing path of account segment `account`'s `side`, with `query`
/// appended (the segment stays text so the miss tests can feed garbage).
fn list_path(account: &str, side: &str, query: &str) -> String {
    format!("/api/v1/accounts/{account}/{side}{query}")
}

/// `GET` of `account`'s `side` listing as `token` (`None` anonymous).
async fn list(
    app: &Router,
    token: Option<&str>,
    account: &Uuid,
    side: &str,
    query: &str,
) -> common::TestResponse {
    raw_list(app, token, &account.to_string(), side, query).await
}

/// [`list`] with the account path segment left exactly as given.
async fn raw_list(
    app: &Router,
    token: Option<&str>,
    account: &str,
    side: &str,
    query: &str,
) -> common::TestResponse {
    let request = common::TestRequest::get(&list_path(account, side, query));
    let request = match token {
        Some(found) => request.bearer(found),
        None => request,
    };
    request.send(app).await
}

/// Records `follower`'s follow of `target`, asserts this instance owns
/// the answer (an open local target accepts on the spot), and returns the
/// row the listing keys on.
async fn granted(pool: &PgPool, follower: &User, target: &User) -> Follow {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        FollowState::Accepted,
        "an open local account answers on the spot"
    );
    row
}

/// The pair of the listed account `user` behind the follow row `row`.
fn pair(user: &User, row: &Follow) -> Relationship {
    (user.id.to_string(), row.id)
}

/// Records `follower`'s follow of the remote or locked `target` — which
/// waits — and grants it through [`follow::accept`], the way an `Accept`
/// activity from the remote instance would.
async fn accepted_later(pool: &PgPool, follower: &User, target: &User) -> Follow {
    let requested = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture request records");
    assert_eq!(
        requested.state,
        FollowState::Pending,
        "a locked or remote target answers in its own time"
    );
    let granted_row = follow::accept(pool, requested.id)
        .await
        .expect("the request is granted");
    assert_eq!(
        granted_row.state,
        FollowState::Accepted,
        "and the listed row is the accepted one"
    );
    granted_row
}

/// `pairs` sorted newest follow row first — the order the listings answer
/// in. `UUIDv7` is only millisecond precise, so the expectation is sorted
/// from the stored ids, never from the order the fixtures were written
/// in (decision 4).
fn sorted_newest_first(pairs: &[Relationship]) -> Vec<Relationship> {
    let mut sorted = pairs.to_vec();
    sorted.sort_by_key(|(_, follow_id)| std::cmp::Reverse(*follow_id));
    sorted
}

/// The account ids of `pairs`, newest follow first.
fn wanted_accounts(pairs: &[Relationship]) -> Vec<String> {
    sorted_newest_first(pairs)
        .into_iter()
        .map(|(user, _)| user)
        .collect()
}

/// The follow row ids of `pairs`, newest first — the cursors the `Link`
/// header and the walking requests must carry.
fn wanted_follows(pairs: &[Relationship]) -> Vec<Uuid> {
    sorted_newest_first(pairs)
        .into_iter()
        .map(|(_, follow_id)| follow_id)
        .collect()
}

/// The `id` of every entity of a listing response, in response order.
fn listed_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the listing is an array")
        .iter()
        .map(|account| account["id"].as_str().expect("an account id").to_owned())
        .collect()
}

/// The `pleroma.relationship` of every entity, keyed by the entry's
/// account id, so each assertion names the account whose flags it reads.
fn embedded_relationships(body: &Value) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    for entry in body.as_array().expect("the listing is an array") {
        let id = entry["id"].as_str().expect("an account id").to_owned();
        let value = entry["pleroma"]["relationship"].clone();
        let previous = map.insert(id, value);
        assert!(previous.is_none(), "one entry per account");
    }
    map
}

/// The `Link` header Pleroma's `add_link_headers` writes for a page of
/// `path`: `next` walks to the oldest row, `prev` back to the newest.
fn expected_link(path: &str, oldest: &Uuid, newest: &Uuid) -> String {
    format!(
        "<{BASE}{path}?max_id={oldest}>; rel=\"next\", \
         <{BASE}{path}?min_id={newest}>; rel=\"prev\""
    )
}

/// Both routes list the accepted relationships newest follow first without
/// any credentials, and take remote mirrors in like any other account: a
/// remote follower is granted here (this instance owns the target's
/// answer) and joins the follower list, and a remote followee joins the
/// outgoing list once the remote instance has answered.
#[tokio::test]
async fn both_listings_answer_newest_follow_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;

    let incoming = [
        pair(&bob, &granted(&pool, &bob, &alice).await),
        pair(&carol, &granted(&pool, &carol, &alice).await),
        pair(&mallory, &granted(&pool, &mallory, &alice).await),
    ];
    let outgoing = [
        pair(&bob, &granted(&pool, &alice, &bob).await),
        pair(&carol, &granted(&pool, &alice, &carol).await),
        pair(&mallory, &accepted_later(&pool, &alice, &mallory).await),
    ];

    let app = common::test_app(pool);
    for (side, pairs) in [(FOLLOWERS, &incoming[..]), (FOLLOWING, &outgoing[..])] {
        let response = list(&app, None, &alice.id, side, "").await;
        assert_eq!(response.status, StatusCode::OK, "auth is optional");
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the entities are JSON"
        );
        assert_eq!(
            listed_ids(&response.json_value()),
            wanted_accounts(pairs),
            "the whole {side} list, newest follow first"
        );
    }
}

/// An entry is a full `Account` entity, not an id stub: the remote
/// follower keeps the handle and actor id federation stored, and an
/// anonymous listing embeds no relationship.
#[tokio::test]
async fn entries_render_as_full_account_entities() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let _incoming: Follow = granted(&pool, &mallory, &alice).await;

    let app = common::test_app(pool);
    let body = list(&app, None, &alice.id, FOLLOWERS, "")
        .await
        .json_value();
    let entries = body.as_array().expect("one follower");
    assert_eq!(entries.len(), 1, "the remote mirror is listed");
    let entry = &entries[0];
    assert_eq!(
        entry["id"],
        mallory.id.to_string(),
        "the id is the uuid text"
    );
    assert_eq!(
        entry["acct"], "mallory@remote.example",
        "a remote mirror keeps its stored handle"
    );
    assert_eq!(entry["uri"], mallory.ap_id, "and its actor id");
    assert_eq!(
        entry["pleroma"]["relationship"],
        json!({}),
        "an anonymous listing embeds no relationship"
    );
}

/// The page walks by `follows` row id: the `Link` header's cursors, the
/// `max_id` continuation, the `min_id` page (answered newest-first
/// again), and the empty ends of the walk, which carry no header.
#[tokio::test]
async fn pages_key_on_follow_row_ids_and_link() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let target = common::create_local_user(&pool, "target").await;
    let mut pairs = Vec::new();
    for n in 0..5 {
        let follower = common::create_local_user(&pool, &format!("follower{n}")).await;
        let row = granted(&pool, &follower, &target).await;
        pairs.push(pair(&follower, &row));
    }
    let path = list_path(&target.id.to_string(), FOLLOWERS, "");
    let app = common::test_app(pool);

    let wanted_users = wanted_accounts(&pairs);
    let wanted_rows = wanted_follows(&pairs);

    let first = list(&app, None, &target.id, FOLLOWERS, "?limit=2").await;
    assert_eq!(
        listed_ids(&first.json_value()),
        wanted_users[..2],
        "two newest first"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link(&path, &wanted_rows[1], &wanted_rows[0]).as_str()),
        "the Link cursors are the page's follow row ids, not user ids"
    );

    let older = list(
        &app,
        None,
        &target.id,
        FOLLOWERS,
        &format!("?limit=2&max_id={}", wanted_rows[1]),
    )
    .await;
    assert_eq!(
        listed_ids(&older.json_value()),
        wanted_users[2..4],
        "max_id continues where the page ended"
    );

    let newer = list(
        &app,
        None,
        &target.id,
        FOLLOWERS,
        &format!("?min_id={}", wanted_rows[3]),
    )
    .await;
    assert_eq!(
        listed_ids(&newer.json_value()),
        wanted_users[..3],
        "min_id answers the newest-first page just after the cursor"
    );

    let polled = list(
        &app,
        None,
        &target.id,
        FOLLOWERS,
        &format!("?since_id={}", wanted_rows[0]),
    )
    .await;
    assert_eq!(
        polled.json_value(),
        json!([]),
        "nothing is newer than the head"
    );
    assert_eq!(
        polled.header("link"),
        None,
        "and an empty page sends no Link header at all"
    );

    let past = list(
        &app,
        None,
        &target.id,
        FOLLOWERS,
        &format!("?max_id={}", wanted_rows[4]),
    )
    .await;
    assert_eq!(past.status, StatusCode::OK, "walking off the end is a 200");
    assert_eq!(past.json_value(), json!([]), "with the empty array");
    assert_eq!(past.header("link"), None, "and no header");
}

/// The listing caps like Pleroma's: twenty rows by default, forty at most.
#[tokio::test]
async fn the_default_page_is_twenty_and_the_cap_is_forty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let target = common::create_local_user(&pool, "target").await;
    for n in 0..45 {
        let follower = common::create_local_user(&pool, &format!("follower{n}")).await;
        let _incoming: Follow = granted(&pool, &follower, &target).await;
    }
    let app = common::test_app(pool);

    let default = list(&app, None, &target.id, FOLLOWERS, "").await;
    assert_eq!(
        listed_ids(&default.json_value()).len(),
        20,
        "Pleroma's @default_limit"
    );
    let greedy = list(&app, None, &target.id, FOLLOWERS, "?limit=100").await;
    assert_eq!(
        listed_ids(&greedy.json_value()).len(),
        40,
        "and @max_limit, never more"
    );
}

/// A locked account's follower list is visible as usual — anonymous and
/// to the owner alike — and what stays out is the request nobody has
/// answered yet, not the whole list.
#[tokio::test]
async fn locked_accounts_list_visible_and_pending_excluded() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_local_user_with(&pool, "carol", |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await;
    let own_token = common::create_app_and_token(&pool, &carol, "read:accounts").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let waiting = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
        .await
        .expect("the request records");
    assert_eq!(
        waiting.state,
        FollowState::Pending,
        "a locked account answers its own requests"
    );

    let app = common::test_app(pool.clone());
    for token in [None, Some(own_token.as_str())] {
        let empty = list(&app, token, &carol.id, FOLLOWERS, "").await;
        assert_eq!(empty.status, StatusCode::OK, "the list is visible");
        assert_eq!(
            empty.json_value(),
            json!([]),
            "a waiting request is not a follower yet"
        );
    }

    let granted_row = follow::accept(&pool, waiting.id).await.unwrap();
    assert_eq!(
        granted_row.state,
        FollowState::Accepted,
        "carol answers the request"
    );
    let after = list(&app, None, &carol.id, FOLLOWERS, "").await;
    assert_eq!(
        listed_ids(&after.json_value()),
        vec![alice.id.to_string()],
        "and the answered one joins the list for everyone"
    );
}

/// The privacy `cond`: `hide_followers` and `hide_follows` answer the
/// empty array to everyone but the account itself — anonymous, another
/// account's token and an app-only token alike, 200 with no `Link`
/// header — while the owner always sees the full list.
#[tokio::test]
async fn hidden_lists_answer_empty_to_everyone_but_the_owner() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.hide_followers = Flag::new(true);
        new.hide_follows = Flag::new(true);
        new
    })
    .await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _incoming: Follow = granted(&pool, &bob, &alice).await;
    let _outgoing: Follow = granted(&pool, &alice, &bob).await;
    let mallory = common::create_local_user(&pool, "mallory").await;
    let stranger_token = common::create_app_and_token(&pool, &mallory, "read:accounts").await;
    let own_token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "read:accounts").await;
    let app = common::test_app(pool);

    for side in [FOLLOWERS, FOLLOWING] {
        for token in [None, Some(stranger_token.as_str()), Some(app_only.as_str())] {
            let hidden = list(&app, token, &alice.id, side, "").await;
            assert_eq!(hidden.status, StatusCode::OK, "hidden is still a 200");
            assert_eq!(hidden.json_value(), json!([]), "the empty array, {side}");
            assert_eq!(hidden.header("link"), None, "and no Link header");
        }
        let shown = list(&app, Some(&own_token), &alice.id, side, "").await;
        assert_eq!(
            listed_ids(&shown.json_value()),
            vec![bob.id.to_string()],
            "the account itself always sees its own list, {side}"
        );
    }
}

/// Hiding the counts does not hide the lists: with only
/// `hide_followers_count` and `hide_follows_count` set, the rows are
/// still listed for everyone.
#[tokio::test]
async fn hidden_counts_do_not_hide_the_lists() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_local_user_with(&pool, "bob", |mut new| {
        new.hide_followers_count = Flag::new(true);
        new.hide_follows_count = Flag::new(true);
        new
    })
    .await;
    let carol = common::create_local_user(&pool, "carol").await;
    let _incoming: Follow = granted(&pool, &carol, &bob).await;
    let _outgoing: Follow = granted(&pool, &bob, &carol).await;
    let app = common::test_app(pool);

    for side in [FOLLOWERS, FOLLOWING] {
        let listed = list(&app, None, &bob.id, side, "").await;
        assert_eq!(
            listed_ids(&listed.json_value()),
            vec![carol.id.to_string()],
            "the count flags hide counts, not rows, {side}"
        );
    }
}

/// The gate: a presented *user* token must grant `read:accounts` — 403
/// with Pleroma's wording if it does not, whoever's list is asked for —
/// while the broad `read` covers it, no token at all proceeds, and a
/// token that does not resolve is the extractor's 401.
#[tokio::test]
async fn the_listings_gate_on_read_accounts_when_authenticated() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    // Mutual follows, so both listings of alice hold exactly bob and the
    // per-side assertions below share one expectation.
    let _incoming: Follow = granted(&pool, &bob, &alice).await;
    let _outgoing: Follow = granted(&pool, &alice, &bob).await;
    let own_narrow = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let own_broad = common::create_app_and_token(&pool, &alice, "read").await;
    let stranger_narrow = common::create_app_and_token(&pool, &bob, "write:accounts").await;
    let app_only = common::create_app_only_token(&pool, "write").await;
    let app = common::test_app(pool);

    for side in [FOLLOWERS, FOLLOWING] {
        let denied = list(&app, Some(&own_narrow), &alice.id, side, "").await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "a user token outside the scope is refused, {side}"
        );
        assert_eq!(denied.text(), NO_SCOPE, "the denial names the scope");

        let stranger_denied = list(&app, Some(&stranger_narrow), &alice.id, side, "").await;
        assert_eq!(
            stranger_denied.status,
            StatusCode::FORBIDDEN,
            "it is refused for someone else's token too, {side}"
        );

        let broad = list(&app, Some(&own_broad), &alice.id, side, "").await;
        assert_eq!(broad.status, StatusCode::OK, "the broad read grants");
        assert_eq!(
            listed_ids(&broad.json_value()),
            vec![bob.id.to_string()],
            "and answers the list"
        );

        let anonymous = list(&app, None, &alice.id, side, "").await;
        assert_eq!(anonymous.status, StatusCode::OK, "no token, no gate");
        let app_passes = list(&app, Some(&app_only), &alice.id, side, "").await;
        assert_eq!(
            app_passes.status,
            StatusCode::OK,
            "an app-only token names no user, so it proceeds unauthenticated, {side}"
        );

        let broken = list(&app, Some("bogus-token"), &alice.id, side, "").await;
        assert_eq!(
            broken.status,
            StatusCode::UNAUTHORIZED,
            "a token that resolves to nothing is refused"
        );
        assert_eq!(broken.text(), INVALID_TOKEN, "MaybeAuthUser's 401 body");
    }
}

/// Every account miss is Pleroma's 404 on both routes: an id that parses
/// but names no row, an id that is not uuid text, and a deactivated
/// account. A malformed cursor is the page extractor's 400, which runs
/// ahead of the handler's lookup.
#[tokio::test]
async fn unknown_accounts_are_404_and_bad_cursors_400() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let ghost = common::create_local_user_with(&pool, "ghost", |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await;
    let alice = common::create_local_user(&pool, "alice").await;
    let missing = Uuid::now_v7().to_string();
    let app = common::test_app(pool);

    let ghosts = ghost.id.to_string();
    for side in [FOLLOWERS, FOLLOWING] {
        for account in [missing.as_str(), "not-an-id", ghosts.as_str()] {
            let response = raw_list(&app, None, account, side, "").await;
            assert_eq!(
                response.status,
                StatusCode::NOT_FOUND,
                "`{account}` names no listable account"
            );
            assert_eq!(response.text(), NOT_FOUND, "decision 8's 404 body");
        }

        let bad_page = list(&app, None, &alice.id, side, "?max_id=nope").await;
        assert_eq!(
            bad_page.status,
            StatusCode::BAD_REQUEST,
            "a malformed cursor is Pleroma's 400, {side}"
        );
        assert_eq!(
            bad_page.json_value(),
            json!({"error": "Invalid id"}),
            "with its message"
        );

        let unknown_and_bad = raw_list(&app, None, &missing, side, "?since_id=nope").await;
        assert_eq!(
            unknown_and_bad.status,
            StatusCode::BAD_REQUEST,
            "and the cursor wins: the page extractor runs before the handler"
        );
    }
}

/// Entries embed the requesting viewer's relationship to each listed
/// account, batched: a stranger sees its own flags on every row, the
/// account owner sees its relationship to its followers too, and an
/// anonymous listing keeps Pleroma's empty object.
#[tokio::test]
async fn entries_embed_the_viewers_relationship() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let erin = common::create_local_user_with(&pool, "erin", |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await;
    let carol = common::create_local_user(&pool, "carol").await;
    // Alice's followers: bob and erin, both accepted.
    let _bob_row: Follow = granted(&pool, &bob, &alice).await;
    let _erin_row: Follow = granted(&pool, &erin, &alice).await;
    // Carol follows bob; her request to locked erin still waits.
    let _carol_bob: Follow = granted(&pool, &carol, &bob).await;
    let waiting = follow::follow(&pool, TEST_BASE_URL, &carol, &erin)
        .await
        .expect("the request to a locked account records");
    assert_eq!(waiting.state, FollowState::Pending, "and waits");
    let carol_token = common::create_app_and_token(&pool, &carol, "read:accounts").await;
    let alice_token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);

    let strangers = embedded_relationships(
        &list(&app, Some(&carol_token), &alice.id, FOLLOWERS, "")
            .await
            .json_value(),
    );
    let carol_on_bob = &strangers[&bob.id.to_string()];
    assert_eq!(
        carol_on_bob["id"],
        bob.id.to_string(),
        "the row is about bob"
    );
    assert_eq!(carol_on_bob["following"], true, "carol follows bob");
    assert_eq!(
        carol_on_bob["followed_by"], false,
        "bob does not follow carol"
    );
    let carol_on_erin = &strangers[&erin.id.to_string()];
    assert_eq!(
        carol_on_erin["following"], false,
        "carol does not follow erin yet"
    );
    assert_eq!(
        carol_on_erin["requested"], true,
        "her request to erin waits"
    );

    let own = embedded_relationships(
        &list(&app, Some(&alice_token), &alice.id, FOLLOWERS, "")
            .await
            .json_value(),
    );
    let alice_on_bob = &own[&bob.id.to_string()];
    assert_eq!(alice_on_bob["followed_by"], true, "bob follows alice");
    assert_eq!(
        alice_on_bob["following"], false,
        "alice does not follow bob back"
    );

    let anonymous = embedded_relationships(
        &list(&app, None, &alice.id, FOLLOWERS, "")
            .await
            .json_value(),
    );
    assert_eq!(
        anonymous[&bob.id.to_string()],
        json!({}),
        "an anonymous listing embeds nothing"
    );
}
