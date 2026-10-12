//! List members over HTTP (`/api/v1/lists/{id}/accounts`, WBE-104): the
//! Mastodon v1 membership routes — read a list's members, add accounts to it,
//! remove them again.
//!
//! The tests pin what the ticket promises. `GET` answers the members as
//! `Account` entities, newest membership first, paged over
//! `list_members.position`: default page 20, `limit=0` every member, `max_id`
//! walking to the older memberships and `min_id` back to the newer ones, a
//! `Link` header whose cursors are those position ordinals and no header at
//! all for an empty page. `POST` takes `account_ids[]` as JSON or as a form,
//! refuses an account the caller does not follow with the exact
//! `{"error":"Validation failed: Account is not followed"}` 422 — writing
//! nothing of a refused batch — 404s on an id that names no account, keeps a
//! member who is already there in the place they hold, and answers `{}`.
//! `DELETE` takes the ids in the body or in the query, answers `{}`, and
//! ignores ids that are not members of that list. Every route 404s on a list
//! that is malformed, unknown or another account's — a member of it included —
//! and every one of them runs the `AuthUser` + `read:lists`/`write:lists`
//! pipeline. Unfollowing an account prunes the unfollowing account's own
//! lists, through [`wired_be::models::follow::unfollow`], the write the
//! unfollow endpoint makes.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

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

/// A list with nobody on it answers the empty array, with no `Link` header:
/// there is no membership to key a page on.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_empty_list_answers_the_empty_array(pool: PgPool) {
    let scene = Scene::new(&pool).await;
    let app = common::test_app(pool);

    let response = members(&app, &scene.token, scene.list.id, "").await;
    assert_eq!(response.status, StatusCode::OK, "200 for its owner");
    assert_eq!(response.text(), "[]", "no members, the empty array");
    assert_eq!(response.header("link"), None, "and nothing to link to");
}

/// The page walk: newest membership first, `max_id` older, `min_id` newer,
/// `since_id` forward, the `Link` header keyed on the position ordinals of the
/// page it describes, and `limit=0` every member of the list.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn members_are_answered_newest_membership_first(pool: PgPool) {
    let scene = Scene::new(&pool).await;
    let app = common::test_app(pool.clone());
    added(&app, &scene.token, scene.list.id, &scene.ids()).await;

    let every = listed(&app, &scene.token, scene.list.id, "limit=0").await;
    assert_eq!(
        accts(&every),
        ["dave", "carol", "bob"],
        "the batch added bob, carol, dave, so dave's membership is the newest \
         and `limit=0` answers every member"
    );
    assert_eq!(
        ids(&every),
        [
            scene.dave.id.to_string(),
            scene.carol.id.to_string(),
            scene.bob.id.to_string()
        ],
        "the entities carry the members' own ids"
    );
    let first = every.first().expect("three members are on the page");
    for key in [
        "url",
        "acct",
        "display_name",
        "created_at",
        "followers_count",
        "pleroma",
    ] {
        assert!(
            first.get(key).is_some(),
            "and every `Account` entity key `{key}` is rendered"
        );
    }
    assert_eq!(
        listed(&app, &scene.token, scene.list.id, "").await.len(),
        3,
        "the default page of 20 holds all three"
    );

    let page = members(&app, &scene.token, scene.list.id, "limit=2").await;
    assert_eq!(page.status, StatusCode::OK, "a page of two reads");
    assert_eq!(
        accts(&page.json::<Vec<Value>>()),
        ["dave", "carol"],
        "the two newest memberships"
    );
    let link = page
        .header("link")
        .expect("a page that leaves a member out links to the rest")
        .to_owned();
    let path = members_path(scene.list.id);
    let next = cursor(&link, "next", "max_id");
    let newest = cursor(&link, "prev", "min_id");
    assert_eq!(
        link,
        format!(
            "<{BASE}{path}?max_id={next}>; rel=\"next\", \
             <{BASE}{path}?min_id={newest}>; rel=\"prev\""
        ),
        "Pleroma's `add_link_headers` over the position ordinals, the \
         request's own path, and nothing else in the query"
    );

    let older = members(
        &app,
        &scene.token,
        scene.list.id,
        &format!("limit=2&max_id={next}"),
    )
    .await;
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
    assert_eq!(
        accts(
            &listed(
                &app,
                &scene.token,
                scene.list.id,
                &format!("limit=2&min_id={back}")
            )
            .await
        ),
        ["dave", "carol"],
        "min_id walks back from the older page, still newest first"
    );
    assert_eq!(
        accts(
            &listed(
                &app,
                &scene.token,
                scene.list.id,
                &format!("limit=2&since_id={back}")
            )
            .await
        ),
        ["dave", "carol"],
        "and since_id reads everything newer than the cursor it polls with"
    );

    let ahead = members(
        &app,
        &scene.token,
        scene.list.id,
        &format!("since_id={newest}"),
    )
    .await;
    assert_eq!(
        ahead.status,
        StatusCode::OK,
        "nothing is newer than the newest membership"
    );
    assert_eq!(ahead.text(), "[]", "the empty array,");
    assert_eq!(ahead.header("link"), None, "and no Link header at all");

    let past = members(&app, &scene.token, scene.list.id, "limit=2&max_id=0").await;
    assert_eq!(
        past.status,
        StatusCode::OK,
        "and a cursor before every membership reads the empty page"
    );
    assert_eq!(past.text(), "[]", "the same empty answer");

    let second_list = seed(&pool, &scene.alice, "Work").await;
    assert!(
        listed(&app, &scene.token, second_list.id, "limit=0")
            .await
            .is_empty(),
        "memberships are per list: her other list holds nobody"
    );
}

/// The default page is 20 members, an explicit `limit` above the cap falls to
/// the cap, and `limit=0` answers all of them.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_default_page_is_twenty_members(pool: PgPool) {
    assert_eq!(
        CAP,
        usize::try_from(MAX_LIMIT).expect("the cap is a page size"),
        "the cap this test pins is the pagination layer's own"
    );
    let scene = Scene::new(&pool).await;
    let mut members = vec![scene.bob.id, scene.carol.id, scene.dave.id];
    for index in 0..42 {
        let followed = user(&pool, &format!("friend{index}")).await;
        follow_user(&pool, &scene.alice, &followed).await;
        members.push(followed.id);
    }
    let app = common::test_app(pool);
    added(&app, &scene.token, scene.list.id, &members).await;

    assert_eq!(
        listed(&app, &scene.token, scene.list.id, "limit=0")
            .await
            .len(),
        members.len(),
        "45 members, and `limit=0` answers every one of them"
    );
    assert_eq!(
        listed(&app, &scene.token, scene.list.id, "").await.len(),
        20,
        "the default page is 20 of them"
    );
    assert_eq!(
        listed(&app, &scene.token, scene.list.id, "limit=100")
            .await
            .len(),
        CAP,
        "and an over-cap request falls to the cap"
    );
}

/// Both body spellings add; a member who is already there keeps the place they
/// hold instead of moving to the end; an empty batch is accepted and changes
/// nothing.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn adding_is_idempotent_in_either_body_spelling(pool: PgPool) {
    let scene = Scene::new(&pool).await;
    let app = common::test_app(pool);

    let form = add_form(&app, &scene.token, scene.list.id, &[scene.bob.id]).await;
    assert_eq!(form.status, StatusCode::OK, "the form spelling adds");
    assert_eq!(form.json_value(), json!({}), "with the same answer");
    assert_eq!(
        accts(&listed(&app, &scene.token, scene.list.id, "limit=0").await),
        ["bob"],
        "and bob is the member"
    );

    added(
        &app,
        &scene.token,
        scene.list.id,
        &[scene.bob.id, scene.bob.id, scene.carol.id],
    )
    .await;
    let after = listed(&app, &scene.token, scene.list.id, "limit=0").await;
    assert_eq!(
        accts(&after),
        ["carol", "bob"],
        "re-adding bob left him first in the list's own order, and carol \
         joined behind him"
    );
    assert_eq!(after.len(), 2, "one membership each, not two for bob");

    let nobody = add_json(
        &app,
        &scene.token,
        scene.list.id,
        json!({"account_ids": []}),
    )
    .await;
    assert_eq!(nobody.status, StatusCode::OK, "an empty batch is accepted");
    assert_eq!(
        nobody.json_value(),
        json!({}),
        "and answers the empty object"
    );
    assert_eq!(
        listed(&app, &scene.token, scene.list.id, "limit=0")
            .await
            .len(),
        2,
        "adding nobody changed nobody"
    );
}

/// Only a followed account may be added: one the caller never followed, and
/// one whose request is still waiting, are the ticket's 422 — and a batch that
/// mixes the two writes nothing at all. Accepting the request is what lets the
/// same account in.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn only_followed_accounts_can_be_added(pool: PgPool) {
    let scene = Scene::new(&pool).await;
    let erin = user(&pool, "erin").await;
    let frank = locked_user(&pool, "frank").await;
    let waiting = follow::follow(&pool, TEST_BASE_URL, &scene.alice, &frank)
        .await
        .expect("the request to a locked account is recorded");
    assert_eq!(
        waiting.state,
        FollowState::Pending,
        "and it waits, unanswered"
    );
    let app = common::test_app(pool.clone());

    for (what, members) in [
        ("an account the caller never followed", vec![erin.id]),
        (
            "a batch of one followed and one not",
            vec![scene.bob.id, erin.id],
        ),
        ("an account whose follow request is pending", vec![frank.id]),
    ] {
        let refused = add_json(&app, &scene.token, scene.list.id, account_ids(&members)).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{what} is a 422"
        );
        assert_eq!(
            refused.json_value(),
            json!({"error": NOT_FOLLOWED}),
            "with exactly the ticket's body for `{what}`"
        );
        assert!(
            listed(&app, &scene.token, scene.list.id, "limit=0")
                .await
                .is_empty(),
            "and the refused batch for `{what}` wrote nothing"
        );
    }

    let granted = follow::accept(&pool, waiting.id)
        .await
        .expect("the request is answered");
    assert_eq!(granted.state, FollowState::Accepted, "now a follow");
    added(&app, &scene.token, scene.list.id, &[frank.id]).await;
    assert_eq!(
        accts(&listed(&app, &scene.token, scene.list.id, "limit=0").await),
        ["frank"],
        "which is the fact the add needed"
    );
}

/// An account id that is malformed or names no account is the 404, and it
/// outranks the follow rule: the phantom in a mixed batch is answered before
/// the followed one is ever considered. A body without `account_ids` anywhere
/// is the parameter 422.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn unknown_account_ids_answer_404(pool: PgPool) {
    let scene = Scene::new(&pool).await;
    let phantom = Uuid::now_v7();
    let app = common::test_app(pool);

    for (what, texts) in [
        ("never minted", vec![phantom.to_string()]),
        ("not an id", vec!["not-a-uuid".to_owned()]),
        (
            "one real id and one phantom in one batch",
            vec![scene.bob.id.to_string(), phantom.to_string()],
        ),
        (
            "a followed account and a number that is no id",
            vec![scene.bob.id.to_string(), "12".to_owned()],
        ),
    ] {
        let refused = add_json(
            &app,
            &scene.token,
            scene.list.id,
            json!({"account_ids": texts}),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::NOT_FOUND,
            "a {what} id is a 404"
        );
        assert_eq!(
            refused.json_value()["error"],
            "Record not found",
            "Pleroma's wording for `{what}`"
        );
        assert!(
            listed(&app, &scene.token, scene.list.id, "limit=0")
                .await
                .is_empty(),
            "and `{what}` added nothing"
        );
    }

    let missing = add_json(&app, &scene.token, scene.list.id, json!({})).await;
    assert_eq!(
        missing.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no `account_ids` at all is the parameter 422"
    );
    assert!(
        missing.json_value()["error"]
            .as_str()
            .unwrap_or_default()
            .contains("missing field `account_ids`"),
        "and it says what is missing: {}",
        missing.text()
    );
}

/// `DELETE` answers `{}` for every batch: a member goes, a non-member is
/// ignored, the ids arrive in the body or in the query, and neither an unknown
/// account nor a malformed id is an error here.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn removing_members_answers_the_empty_object(pool: PgPool) {
    let scene = Scene::new(&pool).await;
    let erin = user(&pool, "erin").await;
    let app = common::test_app(pool);
    added(&app, &scene.token, scene.list.id, &scene.ids()).await;

    let gone = remove_json(
        &app,
        &scene.token,
        scene.list.id,
        account_ids(&[scene.bob.id]),
    )
    .await;
    assert_eq!(gone.status, StatusCode::OK, "a body removes a member");
    assert_eq!(gone.json_value(), json!({}), "and answers the empty object");
    assert_eq!(
        accts(&listed(&app, &scene.token, scene.list.id, "limit=0").await),
        ["dave", "carol"],
        "one membership went"
    );

    let twice = remove_json(
        &app,
        &scene.token,
        scene.list.id,
        account_ids(&[scene.bob.id]),
    )
    .await;
    assert_eq!(
        twice.status,
        StatusCode::OK,
        "removing a non-member is not an error"
    );
    assert_eq!(
        accts(&listed(&app, &scene.token, scene.list.id, "limit=0").await),
        ["dave", "carol"],
        "and it changed nothing"
    );

    let by_query = remove_query(
        &app,
        &scene.token,
        scene.list.id,
        &format!("account_ids[]={}", scene.dave.id),
    )
    .await;
    assert_eq!(
        by_query.status,
        StatusCode::OK,
        "the query spelling removes"
    );
    assert_eq!(
        by_query.json_value(),
        json!({}),
        "and answers the empty object"
    );
    assert_eq!(
        accts(&listed(&app, &scene.token, scene.list.id, "limit=0").await),
        ["carol"],
        "dave is gone"
    );

    let texts: Vec<String> = vec![
        erin.id.to_string(),
        "not-a-uuid".to_owned(),
        scene.carol.id.to_string(),
    ];
    let mixed = remove_json(
        &app,
        &scene.token,
        scene.list.id,
        json!({"account_ids": texts}),
    )
    .await;
    assert_eq!(
        mixed.status,
        StatusCode::OK,
        "an account that is on nobody's list and a malformed id ride along"
    );
    assert!(
        listed(&app, &scene.token, scene.list.id, "limit=0")
            .await
            .is_empty(),
        "the real member went and the rest was ignored"
    );

    let nothing = common::TestRequest::delete(&members_path(scene.list.id))
        .bearer(&scene.token)
        .send(&app)
        .await;
    assert_eq!(
        nothing.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "`account_ids` is required here too"
    );
}

/// Unfollowing an account takes it out of every list of the account that
/// stopped following it, while another owner's membership of the same account
/// is not hers to prune.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn unfollowing_prunes_the_followers_lists(pool: PgPool) {
    let scene = Scene::new(&pool).await;
    let carol_token = token(&pool, &scene.carol, "read:lists").await;
    let carols = seed(&pool, &scene.carol, "carol's own").await;
    let seeded = list::add_members(&pool, carols.id, &[scene.bob.id])
        .await
        .expect("carol's fixture membership adds");
    assert_eq!(seeded, 1, "carol has bob on her own list");
    let app = common::test_app(pool.clone());
    added(
        &app,
        &scene.token,
        scene.list.id,
        &[scene.bob.id, scene.carol.id],
    )
    .await;

    let removed = follow::unfollow(&pool, &scene.alice, &scene.bob)
        .await
        .expect("alice unfollows bob");
    assert!(removed, "the relationship was there to remove");

    assert_eq!(
        accts(&listed(&app, &scene.token, scene.list.id, "limit=0").await),
        ["carol"],
        "bob is out of her list and carol stayed"
    );
    let bob_lists = common::TestRequest::get(&format!("/api/v1/accounts/{}/lists", scene.bob.id))
        .bearer(&scene.token)
        .send(&app)
        .await;
    assert_eq!(
        bob_lists.json::<Vec<Value>>(),
        Vec::<Value>::new(),
        "and bob is on none of her lists any more"
    );
    assert_eq!(
        accts(&listed(&app, &carol_token, carols.id, "limit=0").await),
        ["bob"],
        "while carol's membership of bob is alice's not to prune"
    );
}

/// A list that is malformed, unknown or another account's is one and the same
/// 404 on all three member routes — for a stranger and for an account that is
/// a member of it — and the owner's rows survive every attempt.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_list_that_is_not_the_callers_is_404(pool: PgPool) {
    let scene = Scene::new(&pool).await;
    let (erin, erin_token) = user_and_token(&pool, "erin", "read:lists write:lists").await;
    let bob_token = token(&pool, &scene.bob, "read:lists write:lists").await;
    let app = common::test_app(pool.clone());
    added(&app, &scene.token, scene.list.id, &[scene.bob.id]).await;
    let bods_own = seed(&pool, &scene.bob, "bob's own").await;
    assert_eq!(bods_own.user_id, scene.bob.id, "bob owns his own list");

    let body = account_ids(&[erin.id]);
    let unknown = Uuid::now_v7();
    for (who, bearer, what, id) in [
        ("a stranger", &erin_token, "alice's list", scene.list.id),
        ("a stranger", &erin_token, "bob's list", bods_own.id),
        ("a stranger", &erin_token, "a never-minted list", unknown),
        ("a member of it", &bob_token, "alice's list", scene.list.id),
        ("a member of it", &bob_token, "a never-minted list", unknown),
    ] {
        let read = members(&app, bearer, id, "").await;
        assert_eq!(read.status, StatusCode::NOT_FOUND, "{who} reads {what}");
        assert_eq!(
            read.json_value()["error"],
            "Record not found",
            "Pleroma's wording on GET, {who} / {what}"
        );
        let write = add_json(&app, bearer, id, body.clone()).await;
        assert_eq!(write.status, StatusCode::NOT_FOUND, "{who} adds to {what}");
        assert_eq!(
            write.json_value()["error"],
            "Record not found",
            "and the 404 outranks the follow rule {who} would fail too"
        );
        let dropped = remove_json(&app, bearer, id, body.clone()).await;
        assert_eq!(
            dropped.status,
            StatusCode::NOT_FOUND,
            "{who} removes from {what}"
        );
    }

    for bearer in [&erin_token, &bob_token] {
        let malformed = common::TestRequest::get(&format!("{ROOT}/not-a-uuid/accounts"))
            .bearer(bearer)
            .send(&app)
            .await;
        assert_eq!(
            malformed.status,
            StatusCode::NOT_FOUND,
            "a malformed list id is the same 404"
        );
    }

    let untouched = listed(&app, &scene.token, scene.list.id, "limit=0").await;
    assert_eq!(
        accts(&untouched),
        ["bob"],
        "while alice's membership of bob survived every attempt on it"
    );
    assert!(
        listed(&app, &bob_token, bods_own.id, "limit=0")
            .await
            .is_empty(),
        "and bob's own list is still his and still empty"
    );
}

/// A page cursor that is not a position is the pagination 400: the members
/// page keys on ordinals, so an account id is no cursor here.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_cursor_that_is_not_a_position_is_a_bad_request(pool: PgPool) {
    let scene = Scene::new(&pool).await;
    let app = common::test_app(pool.clone());
    added(&app, &scene.token, scene.list.id, &[scene.bob.id]).await;

    let uuid_cursor = scene.bob.id.to_string();
    for key in ["max_id", "min_id", "since_id"] {
        for value in ["nope", "1.5", uuid_cursor.as_str()] {
            let response =
                members(&app, &scene.token, scene.list.id, &format!("{key}={value}")).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "`{key}={value}` is a 400"
            );
            assert_eq!(
                response.json_value()["error"],
                "Invalid id",
                "the pagination wording for `{key}={value}`"
            );
        }
    }
}

/// The scope pipeline of all three routes: no token is the 401, a bogus one
/// the 401 that names it, an app-only token the 403 that names the missing
/// user, and after that `read:lists` to read and `write:lists` to change, with
/// the broad `read`/`write` trees passing on both sides and an unrelated
/// sub-scope refused on all three.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_scope_pipeline_guards_the_member_routes(pool: PgPool) {
    let scene = Scene::new(&pool).await;
    let (reader, reader_token) = user_and_token(&pool, "reader", "read:lists").await;
    let readers_own = seed(&pool, &reader, "reader's own").await;
    let (mutator, mutator_token) = user_and_token(&pool, "mutator", "write:lists").await;
    let (_narrow, wrong_scope) =
        user_and_token(&pool, "narrow", "read:statuses write:statuses").await;
    let (broad_reader, broad_read) = user_and_token(&pool, "broad_r", "read").await;
    let broads_own = seed(&pool, &broad_reader, "broad read's own").await;
    let (_broad_write, broad_write) = user_and_token(&pool, "broad_w", "write").await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool.clone());

    // Mutator may write lists but has followed nobody: give her a list of her
    // own and someone she follows to put on it.
    let theirs = seed(&pool, &mutator, "mutator's own").await;
    follow_user(&pool, &mutator, &scene.bob).await;

    let anonymous = common::TestRequest::get(&members_path(scene.list.id))
        .send(&app)
        .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token is a 401"
    );
    assert_eq!(
        anonymous.json_value()["error"],
        "Invalid credentials",
        "Pleroma's wording for an absent token"
    );
    let bogus = add_json(
        &app,
        "not-a-real-token",
        scene.list.id,
        json!({"account_ids": []}),
    )
    .await;
    assert_eq!(
        bogus.status,
        StatusCode::UNAUTHORIZED,
        "a bogus token is a 401"
    );
    assert_eq!(
        bogus.json_value()["error"],
        "The access token is invalid",
        "naming the token as the problem"
    );
    let as_app = members(&app, &app_only, scene.list.id, "").await;
    assert_eq!(
        as_app.status,
        StatusCode::FORBIDDEN,
        "an app token is a 403"
    );
    assert_eq!(
        as_app.json_value()["error"],
        "This API requires an authenticated user",
        "naming what is missing"
    );

    for (who, bearer) in [
        ("`read:statuses write:statuses`", &wrong_scope),
        ("`write:lists`", &mutator_token),
    ] {
        let read = members(&app, bearer, scene.list.id, "").await;
        assert_eq!(
            read.status,
            StatusCode::FORBIDDEN,
            "{who} may not read members"
        );
        assert_eq!(
            read.json_value()["error"],
            "Insufficient permissions: read:lists.",
            "the denial names the missing scope for {who}"
        );
    }

    for (who, bearer) in [
        ("`read:statuses write:statuses`", &wrong_scope),
        ("`read:lists`", &reader_token),
        ("the broad `read`", &broad_read),
    ] {
        let add = add_json(&app, bearer, scene.list.id, account_ids(&[scene.bob.id])).await;
        assert_eq!(
            add.status,
            StatusCode::FORBIDDEN,
            "{who} may not add members"
        );
        assert_eq!(
            add.json_value()["error"],
            "Insufficient permissions: write:lists.",
            "the denial names the missing scope for {who}"
        );
        let remove = remove_json(&app, bearer, scene.list.id, account_ids(&[scene.bob.id])).await;
        assert_eq!(
            remove.status,
            StatusCode::FORBIDDEN,
            "{who} may not remove members"
        );
        assert_eq!(
            remove.json_value()["error"],
            "Insufficient permissions: write:lists.",
            "and says the same thing for {who}"
        );
    }

    for (who, bearer, own) in [
        ("`read:lists`", &reader_token, readers_own.id),
        ("the broad `read`", &broad_read, broads_own.id),
    ] {
        assert_eq!(
            members(&app, bearer, own, "").await.status,
            StatusCode::OK,
            "{who} reads the members of a list of her own"
        );
        assert_eq!(
            members(&app, bearer, scene.list.id, "").await.status,
            StatusCode::NOT_FOUND,
            "...and the 404 on alice's, which is the scope check passing first"
        );
    }
    added(&app, &mutator_token, theirs.id, &[scene.bob.id]).await;
    let mutator_read = token(&pool, &mutator, "read:lists").await;
    assert_eq!(
        accts(&listed(&app, &mutator_read, theirs.id, "limit=0").await),
        ["bob"],
        "`write:lists` adds to her own list"
    );
    let foreign = remove_json(&app, &broad_write, theirs.id, account_ids(&[scene.bob.id])).await;
    assert_eq!(
        foreign.status,
        StatusCode::NOT_FOUND,
        "the broad `write` may write, but only to its own lists: this one is \
         mutator's, so the 404 answers it"
    );
    assert_eq!(
        accts(&listed(&app, &mutator_read, theirs.id, "limit=0").await),
        ["bob"],
        "and mutator's member stayed"
    );
}
