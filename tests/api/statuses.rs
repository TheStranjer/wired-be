//! The status read endpoints over HTTP (`GET /api/v1/statuses/{id}` and the
//! batch `GET /api/v1/statuses?id[]=…`, WBE-77).
//!
//! The show route answers the `Status` entity to everyone the visibility
//! model admits: a public post to its author, a stranger and the anonymous
//! caller alike; a `private` post to the author and an accepted follower
//! only (a pending request is not one); a `direct` post to the author and
//! the accounts it @-mentions only. A deleted row, a malformed id, an id
//! that names nothing and a post the viewer may not see are one
//! indistinguishable 404 `{"error":"Record not found"}`, so a probe never
//! learns that somebody's private post exists. A reblog's own id answers
//! the reblog entity — the reblogger's row, empty content, the original
//! nested in `reblog` — and a reblog of a post the viewer may not see is
//! invisible itself.
//!
//! The batch route answers the entities of the ids this viewer may see in
//! the order they were requested: it reads `id[]` and Pleroma's deprecated
//! `ids[]` (both also unwrapped, `id` winning), acts on at most a hundred
//! ids and ignores the ones past that, skips the malformed, unknown,
//! deleted and invisible ids without saying which was which, and answers
//! `[]` to a request that carries no usable id.
//!
//! Both routes run Pleroma's optional-auth pipeline: no token proceeds, a
//! presented user token must carry `read:statuses` (403 for one that does
//! not, even on a public post), an app-only token proceeds unauthenticated,
//! and a token that does not resolve is the extractor's 401.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowState},
        status::{self, NewStatus, Status, Visibility},
        user::{Flag, NewUser, User},
    },
};

use crate::common;

/// The body every miss on these routes carries (decision 8).
const NOT_FOUND: &str = "Record not found";
/// The 403 body the scope gate renders for a user token without the scope.
const NO_SCOPE: &str = "Insufficient permissions: read:statuses.";
/// The 401 body `MaybeAuthUser` renders for a token that does not resolve.
const INVALID_TOKEN: &str = "The access token is invalid";
/// The scope both routes ask a presented user token for.
const SCOPES: &str = "read:statuses";
/// The id of a status that is not stored here, for the miss probes.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

/// `GET /api/v1/statuses/{id}` as `token` (`None`: anonymous). The id stays
/// text so the miss probes can feed any spelling as the path segment.
async fn show(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}"));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `GET /api/v1/statuses?{query}` as `token` (`None`: anonymous). An empty
/// `query` is the route with no parameters at all.
async fn batch(app: &Router, token: Option<&str>, query: &str) -> common::TestResponse {
    let path = if query.is_empty() {
        "/api/v1/statuses".to_owned()
    } else {
        format!("/api/v1/statuses?{query}")
    };
    let request = common::TestRequest::get(&path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `GET …?{key}[]=…` for the batch, one param per id, in the order given.
/// Generic over the id's text form so both `&str` and `String` ids join a
/// query (the tests hold `Uuid`s they stringify inline and generated ids
/// they already own).
fn id_query<S: AsRef<str>>(key: &str, ids: &[S]) -> String {
    ids.iter()
        .map(|id| format!("{key}[]={}", id.as_ref()))
        .collect::<Vec<String>>()
        .join("&")
}

/// The `id` of every entity of a batch response, in response order.
fn entity_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the batch is an array")
        .iter()
        .map(|status| status["id"].as_str().expect("a status id").to_owned())
        .collect()
}

/// The ids of `statuses` as the wire spells them, for an expectation.
fn ids_of(statuses: &[&Status]) -> Vec<String> {
    statuses
        .iter()
        .map(|status| status.id.to_string())
        .collect()
}

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn user_with_token(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// Inserts `author`'s `direct` status addressed to `mentioned`.
async fn direct_to(pool: &PgPool, author: &User, mentioned: &User) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Direct,
            mentioned_user_ids: vec![mentioned.id],
            ..NewStatus::local(author, "psst")
        },
    )
    .await
    .expect("a direct status with a mention inserts")
}

/// Inserts `reblogger`'s public reblog of `original`.
async fn reblog_of(pool: &PgPool, reblogger: &User, original: &Status) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            reblog_of_id: Some(original.id),
            ..NewStatus::local(reblogger, "")
        },
    )
    .await
    .expect("a reblog inserts")
}

/// Records `follower`'s follow of `target` and asserts it landed accepted
/// (both fixtures are open local accounts) — the relation rule 5 grants.
async fn followed(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        FollowState::Accepted,
        "an open local account accepts on the spot"
    );
}

/// Turns `doomed` into a tombstone, asserting the delete hit the row.
async fn deleted(pool: &PgPool, doomed: &Status) {
    let tombstone = status::soft_delete(pool, doomed.id)
        .await
        .expect("the fixture status soft-deletes")
        .expect("the fixture status was live");
    assert!(tombstone.deleted_at.is_some(), "and it is a tombstone now");
}

/// A public status is the entity for its author, for a stranger's token and
/// for an anonymous request alike, carrying the stored row's keys.
#[tokio::test]
async fn a_public_status_shows_to_everyone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (_stranger, stranger_token) = user_with_token(&pool, "mallory", SCOPES).await;
    let app = common::test_app(pool.clone());
    let post = common::create_status(&pool, &alice, "hello fediverse", Visibility::Public).await;
    let id = post.id.to_string();

    let viewers = [
        ("the author", Some(alice_token.as_str())),
        ("a stranger", Some(stranger_token.as_str())),
        ("an anonymous caller", None),
    ];
    for (who, token) in viewers {
        let response = show(&app, token, &id).await;
        common::assert_answer(&response, StatusCode::OK, who);
        let body = response.json_value();
        assert_eq!(body["id"], id, "{who}: the entity carries its own id");
        assert_eq!(body["content"], "<p>hello fediverse</p>", "{who}: html");
        assert_eq!(body["visibility"], "public", "{who}: as stored");
        assert_eq!(body["account"]["acct"], "alice", "{who}: the author");
        assert_eq!(body["uri"], post.uri, "{who}: the object id");
        assert_eq!(body["url"], json!(post.url), "{who}: the human url");
        assert_eq!(body["reblog"], Value::Null, "{who}: not a reblog");
        assert_eq!(
            body["pleroma"]["conversation_id"],
            post.conversation_id.to_string(),
            "{who}: the thread id is the stored conversation"
        );
    }
}

/// A `private` status is the author's and an accepted follower's and nobody
/// else's: a stranger's token, an anonymous request and the follower of a
/// locked account whose request is still pending all get the 404 a missing
/// id gets.
#[tokio::test]
async fn a_private_status_shows_to_the_author_and_a_follower() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (follower, follower_token) = user_with_token(&pool, "bob", SCOPES).await;
    let (_stranger, stranger_token) = user_with_token(&pool, "carol", SCOPES).await;
    let app = common::test_app(pool.clone());
    let post = common::create_status(&pool, &alice, "for my followers", Visibility::Private).await;
    let id = post.id.to_string();

    followed(&pool, &follower, &alice).await;

    common::assert_answer(
        &show(&app, Some(follower_token.as_str()), &id).await,
        StatusCode::OK,
        "an accepted follower",
    );
    common::assert_answer(
        &show(&app, Some(alice_token.as_str()), &id).await,
        StatusCode::OK,
        "the author",
    );
    common::assert_all_refused(
        &[
            show(&app, Some(stranger_token.as_str()), &id).await,
            show(&app, None, &id).await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "a stranger and the anonymous caller",
    );

    // A locked account's follower holds a pending request only, which rule 5
    // does not count: the same refusal a stranger gets.
    let locked = common::create_local_user_with(&pool, "dave", |new| NewUser {
        locked: Flag::new(true),
        ..new
    })
    .await;
    let waiting = common::create_local_user(&pool, "erin").await;
    let waiting_token = common::create_app_and_token(&pool, &waiting, SCOPES).await;
    let held = common::create_status(&pool, &locked, "behind the lock", Visibility::Private).await;
    let request = follow::follow(&pool, TEST_BASE_URL, &waiting, &locked)
        .await
        .expect("the request records");
    assert_eq!(
        request.state,
        FollowState::Pending,
        "a locked account answers in its own time"
    );
    common::assert_all_refused(
        &[show(&app, Some(waiting_token.as_str()), &held.id.to_string()).await],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "a pending follower",
    );
}

/// A `direct` status is the author's and the mentioned accounts' only: a
/// follower of the author who is not addressed cannot see it, and neither
/// can the anonymous caller.
#[tokio::test]
async fn a_direct_status_shows_to_the_author_and_the_mentioned() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let (carol, carol_token) = user_with_token(&pool, "carol", SCOPES).await;
    let app = common::test_app(pool.clone());
    followed(&pool, &carol, &alice).await;
    let message = direct_to(&pool, &alice, &bob).await;
    let id = message.id.to_string();

    common::assert_answer(
        &show(&app, Some(bob_token.as_str()), &id).await,
        StatusCode::OK,
        "the mentioned account",
    );
    common::assert_answer(
        &show(&app, Some(alice_token.as_str()), &id).await,
        StatusCode::OK,
        "the author",
    );
    common::assert_all_refused(
        &[
            show(&app, Some(carol_token.as_str()), &id).await,
            show(&app, None, &id).await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "a follower who is not addressed, and the anonymous caller",
    );
}

/// A deleted status answers 404 to everyone, its own author included (rule
/// 1 hides a tombstone even from the account that posted it), and it drops
/// out of a batch that names it without taking the rest of the page down.
#[tokio::test]
async fn a_deleted_status_is_gone_for_everyone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let app = common::test_app(pool.clone());
    let post = common::create_status(&pool, &alice, "take it down", Visibility::Public).await;
    let id = post.id.to_string();
    common::assert_answer(
        &show(&app, Some(alice_token.as_str()), &id).await,
        StatusCode::OK,
        "the author, before the delete",
    );

    deleted(&pool, &post).await;

    common::assert_all_refused(
        &[
            show(&app, Some(alice_token.as_str()), &id).await,
            show(&app, None, &id).await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "a deleted status, for its author and anonymously",
    );

    let survivor = common::create_status(&pool, &alice, "still here", Visibility::Public).await;
    let after = batch(
        &app,
        None,
        &id_query("id", &[&id, &survivor.id.to_string()]),
    )
    .await;
    common::assert_answer(&after, StatusCode::OK, "the batch after a delete");
    assert_eq!(
        entity_ids(&after.json_value()),
        ids_of(&[&survivor]),
        "the tombstone is skipped and the live row answers"
    );
}

/// A malformed id, a spelling the storage never hands out and an id that
/// names nothing are the same 404 as a hidden post, so a probe learns
/// nothing about what this instance holds.
#[tokio::test]
async fn a_bad_or_unknown_id_is_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);

    let probes: Vec<(String, &str)> = vec![
        ("not-an-id".to_owned(), "text that is not a UUID"),
        (
            "01934567-89ab-7cde-f012-3456789abc".to_owned(),
            "a truncated UUID",
        ),
        (format!("{{{UNKNOWN_ID}}}"), "the braced spelling"),
        (format!("urn:uuid:{UNKNOWN_ID}"), "the urn spelling"),
        (UNKNOWN_ID.to_owned(), "a well-formed id that names nothing"),
    ];
    for (probe, who) in probes {
        common::assert_all_refused(
            &[show(&app, None, &probe).await],
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            who,
        );
    }
}

/// A reblog's own id answers the reblog entity — the reblogger's row with
/// empty content and the original nested one level deep — and deleting the
/// original takes the reblog out of the API too (rule 7).
#[tokio::test]
async fn a_reblog_answers_its_own_entity_with_the_original_nested() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let app = common::test_app(pool.clone());
    let original = common::create_status(&pool, &alice, "boost me", Visibility::Public).await;
    let announcement = reblog_of(&pool, &bob, &original).await;
    let announcement_id = announcement.id.to_string();

    let announced = show(&app, Some(bob_token.as_str()), &announcement_id).await;
    common::assert_answer(&announced, StatusCode::OK, "a reblog by its own id");
    let body = announced.json_value();
    assert_eq!(
        body["id"], announcement_id,
        "the outer entity is the reblog's own row"
    );
    assert_eq!(body["uri"], announcement.uri, "with the reblog's own uri");
    assert_eq!(body["account"]["acct"], "bob", "and the reblogger speaks");
    assert_eq!(body["content"], "", "a reblog carries no text of its own");
    assert_eq!(
        body["pleroma"]["content"],
        json!({"text/plain": ""}),
        "and no plain text either"
    );
    assert_eq!(
        body["reblog"]["id"],
        original.id.to_string(),
        "the original nests one level deep"
    );
    assert_eq!(
        body["reblog"]["content"], "<p>boost me</p>",
        "and brings its own text"
    );
    assert_eq!(
        body["reblog"]["account"]["acct"], "alice",
        "the nested account is the original's author"
    );
    assert_eq!(
        body["reblog"]["reblog"],
        Value::Null,
        "the nested entity does not nest again"
    );

    deleted(&pool, &original).await;
    common::assert_all_refused(
        &[
            show(&app, Some(bob_token.as_str()), &announcement_id).await,
            show(&app, Some(bob_token.as_str()), &original.id.to_string()).await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "a reblog whose original is gone, and the original itself",
    );
}

/// The batch answers the entities of the ids this viewer may see, in the
/// order they were requested, skipping the malformed, unknown, deleted and
/// invisible ids without saying which was which.
#[tokio::test]
async fn the_batch_answers_the_visible_ids_in_requested_order() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let app = common::test_app(pool.clone());

    let first = common::create_status(&pool, &alice, "first", Visibility::Public).await;
    let second = common::create_status(&pool, &alice, "second", Visibility::Public).await;
    let hidden = common::create_status(&pool, &alice, "hidden", Visibility::Private).await;
    let doomed = common::create_status(&pool, &alice, "doomed", Visibility::Public).await;
    deleted(&pool, &doomed).await;

    // Asked for oldest-last, with one of every kind of miss in between.
    let query = id_query(
        "id",
        &[
            &second.id.to_string(),
            "not-an-id",
            &hidden.id.to_string(),
            &doomed.id.to_string(),
            UNKNOWN_ID,
            &first.id.to_string(),
        ],
    );

    let strangers = batch(&app, Some(bob_token.as_str()), &query).await;
    common::assert_answer(&strangers, StatusCode::OK, "a stranger's batch");
    assert_eq!(
        entity_ids(&strangers.json_value()),
        ids_of(&[&second, &first]),
        "the two public posts, in the order they were requested"
    );

    let authors = batch(&app, Some(alice_token.as_str()), &query).await;
    common::assert_answer(&authors, StatusCode::OK, "the author's batch");
    assert_eq!(
        entity_ids(&authors.json_value()),
        ids_of(&[&second, &hidden, &first]),
        "the private post joins the page for the account that may see it"
    );

    let anonymous = batch(&app, None, &query).await;
    common::assert_answer(&anonymous, StatusCode::OK, "the anonymous batch");
    assert_eq!(
        entity_ids(&anonymous.json_value()),
        ids_of(&[&second, &first]),
        "anonymously the private post is skipped like every other miss"
    );
}

/// The batch reads Mastodon's `id[]`, Pleroma's deprecated `ids[]` and the
/// unwrapped single forms of both, `id` winning over `ids`, and it stops at
/// a hundred ids: every id past the hundredth is ignored, not refused.
#[tokio::test]
async fn the_batch_reads_every_id_spelling_and_caps_the_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let app = common::test_app(pool.clone());
    let one = common::create_status(&pool, &alice, "one", Visibility::Public).await;
    let two = common::create_status(&pool, &alice, "two", Visibility::Public).await;
    let three = common::create_status(&pool, &alice, "three", Visibility::Public).await;
    let (one_id, two_id, three_id) = (one.id.to_string(), two.id.to_string(), three.id.to_string());

    for (query, want, who) in [
        (
            id_query("id", &[&two_id, &one_id]),
            ids_of(&[&two, &one]),
            "Mastodon's bracketed `id[]`",
        ),
        (
            id_query("ids", &[&three_id, &one_id]),
            ids_of(&[&three, &one]),
            "Pleroma's deprecated bracketed `ids[]`",
        ),
        (
            format!("id={one_id}"),
            ids_of(&[&one]),
            "the unwrapped single `id`",
        ),
        (
            format!("ids={two_id}&id[]={one_id}"),
            ids_of(&[&one]),
            "a lone `ids=` beside an `id[]`, which wins",
        ),
    ] {
        let page = batch(&app, None, &query).await;
        common::assert_answer(&page, StatusCode::OK, who);
        assert_eq!(entity_ids(&page.json_value()), want, "{who}");
    }

    // A hundred ids are read; everything past them is ignored. `one` is the
    // first, then ninety-nine ids that name nothing, and `two` — a real
    // post — arrives as the hundred-and-first and is never looked up.
    let mut crowded: Vec<String> = vec![one_id.clone()];
    crowded.extend((0..99).map(|_| Uuid::now_v7().to_string()));
    let full = format!("{}&id[]={two_id}", id_query("id", &crowded));
    let crowded_page = batch(&app, None, &full).await;
    common::assert_answer(&crowded_page, StatusCode::OK, "a request of 101 ids");
    assert_eq!(
        entity_ids(&crowded_page.json_value()),
        ids_of(&[&one]),
        "the id past the hundredth names nothing this endpoint reads"
    );
}

/// A batch request that carries no usable id answers the empty array rather
/// than an error, and an id named twice is answered once.
#[tokio::test]
async fn the_batch_without_ids_is_empty_and_duplicates_answer_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let app = common::test_app(pool.clone());
    let post = common::create_status(&pool, &alice, "asked twice", Visibility::Public).await;
    let id = post.id.to_string();

    for (query, who) in [
        ("", "no query at all"),
        ("id=", "an empty id"),
        ("id[]=nope&other=1", "nothing usable among them"),
        ("ids[]=", "an empty alias"),
    ] {
        let page = batch(&app, None, query).await;
        common::assert_answer(&page, StatusCode::OK, who);
        assert_eq!(
            page.json_value(),
            Value::Array(Vec::new()),
            "{who} answers the empty array"
        );
    }

    let twice = batch(&app, None, &id_query("id", &[&id, &id])).await;
    common::assert_answer(&twice, StatusCode::OK, "an id named twice");
    assert_eq!(
        entity_ids(&twice.json_value()),
        ids_of(&[&post]),
        "one entity for one id, as Pleroma's `WHERE id IN` reads a repeat"
    );
}

/// The auth gate is Pleroma's `fallback: :proceed_unauthenticated`: a user
/// token that lacks `read:statuses` is the 403 on both routes even for a
/// public post, an app-only token carries no user and so proceeds as
/// anonymous, and a token that does not resolve is the extractor's 401.
#[tokio::test]
async fn the_auth_gate_runs_on_both_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (_narrow, narrow_token) = user_with_token(&pool, "bob", "read:accounts").await;
    let app = common::test_app(pool.clone());
    let post = common::create_status(&pool, &alice, "public", Visibility::Public).await;
    let hidden = common::create_status(&pool, &alice, "hidden", Visibility::Private).await;
    let id = post.id.to_string();
    let query = id_query("id", &[&id]);
    let app_only = common::create_app_only_token(&pool, "write:statuses").await;

    for (refused, who) in [
        (show(&app, Some(narrow_token.as_str()), &id).await, "show"),
        (
            batch(&app, Some(narrow_token.as_str()), &query).await,
            "the batch",
        ),
        (
            show(&app, Some(narrow_token.as_str()), &hidden.id.to_string()).await,
            "show of a private post",
        ),
    ] {
        common::assert_answer(&refused, StatusCode::FORBIDDEN, who);
        common::assert_error(&refused, NO_SCOPE, who);
    }

    // An app-only token assigns no user, so the route proceeds as
    // anonymous: the public post answers, the private one is the 404.
    common::assert_answer(
        &show(&app, Some(app_only.as_str()), &id).await,
        StatusCode::OK,
        "an app-only token, public post",
    );
    common::assert_all_refused(
        &[show(&app, Some(app_only.as_str()), &hidden.id.to_string()).await],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "an app-only token, private post",
    );
    let app_only_page = batch(
        &app,
        Some(app_only.as_str()),
        &id_query("id", &[&id, &hidden.id.to_string()]),
    )
    .await;
    common::assert_answer(&app_only_page, StatusCode::OK, "an app-only token, batch");
    assert_eq!(
        entity_ids(&app_only_page.json_value()),
        ids_of(&[&post]),
        "the batch narrows to what an anonymous viewer may see"
    );

    for (unauthorized, who) in [
        (
            show(&app, Some("token-that-was-never-minted"), &id).await,
            "show",
        ),
        (
            batch(&app, Some("token-that-was-never-minted"), &query).await,
            "the batch",
        ),
    ] {
        common::assert_answer(&unauthorized, StatusCode::UNAUTHORIZED, who);
        common::assert_error(&unauthorized, INVALID_TOKEN, who);
    }
}

/// The batch renders a reblog the same way the show route does — nested
/// original and all — and a reblog of a post this viewer may not see is
/// itself invisible, until the viewer may see the original.
#[tokio::test]
async fn the_batch_renders_reblogs_and_their_visibility() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (bob, _bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let (carol, carol_token) = user_with_token(&pool, "carol", SCOPES).await;
    let app = common::test_app(pool.clone());
    let public = common::create_status(&pool, &alice, "say it loud", Visibility::Public).await;
    let private = common::create_status(&pool, &alice, "whisper", Visibility::Private).await;
    let shout = reblog_of(&pool, &bob, &public).await;
    let whisper = reblog_of(&pool, &bob, &private).await;
    let query = id_query("id", &[&shout.id.to_string(), &whisper.id.to_string()]);

    let strangers = batch(&app, Some(carol_token.as_str()), &query).await;
    common::assert_answer(&strangers, StatusCode::OK, "a stranger's batch of reblogs");
    assert_eq!(
        entity_ids(&strangers.json_value()),
        ids_of(&[&shout]),
        "the reblog of a post the viewer may not see is invisible too"
    );

    followed(&pool, &carol, &alice).await;
    let followers = batch(&app, Some(carol_token.as_str()), &query).await;
    common::assert_answer(
        &followers,
        StatusCode::OK,
        "the follower's batch of reblogs",
    );
    assert_eq!(
        entity_ids(&followers.json_value()),
        ids_of(&[&shout, &whisper]),
        "following the author brings both reblogs onto the page"
    );
    let body = followers.json_value();
    let entries = body.as_array().expect("the batch is an array");
    assert_eq!(
        entries[0]["reblog"]["id"],
        public.id.to_string(),
        "and each reblog entity carries its original nested"
    );
    assert_eq!(
        entries[1]["reblog"]["visibility"], "private",
        "the nested original keeps its own visibility"
    );
}
