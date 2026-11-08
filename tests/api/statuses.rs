//! The status endpoints over HTTP: the reads (`GET /api/v1/statuses/{id}`
//! and the batch `GET /api/v1/statuses?id[]=…`, WBE-77) and the write
//! (`POST /api/v1/statuses`, WBE-76).
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
//! Both reads run Pleroma's optional-auth pipeline: no token proceeds, a
//! presented user token must carry `read:statuses` (403 for one that does
//! not, even on a public post), an app-only token proceeds unauthenticated,
//! and a token that does not resolve is the extractor's 401.
//!
//! The write runs the strict one — `AuthUser` plus `write:statuses` plus the
//! `statuses_actions` throttle — and the ticket's own refusals: 422 for a
//! post with nothing to say, one over `MAX_STATUS_CHARS` (counted with every
//! URL at 23 characters), an unknown `visibility` or `content_type`, an
//! over-long `spoiler_text` or `language`. What passes is stored as a `local`
//! row with its mentions in `status_mentions` and its tags in
//! `status_hashtags`, the posting client recorded as its application, the
//! author's `statuses_count` stepped, and the whole thing answered as the
//! `Status` entity the author themselves sees — from a JSON or a form body
//! alike.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowState},
        hashtag, mention,
        oauth_app::{self, NewOauthApp},
        status::{self, NewStatus, Status, Visibility},
        user::{self, Flag, NewUser, User},
    },
    util::{
        entities::{hashtag_html, mention_html},
        format::status_length,
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

// ────────────────────────────  POST /api/v1/statuses  ────────────────────────

/// The scope the write route asks for.
const WRITE: &str = "write:statuses";
/// The 403 body the scope gate renders for a user token without it.
const NO_WRITE: &str = "Insufficient permissions: write:statuses.";
/// The 401 body `AuthUser` renders when no credential is presented at all.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body `AuthUser` renders for a token that names no user.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The route every creation test posts to.
const STATUSES: &str = "/api/v1/statuses";
/// 422 body for a post with nothing to say (Pleroma's
/// `validate_character_limit`).
const EMPTY_STATUS: &str = "Cannot post an empty status without attachments";
/// 422 body for a post over `MAX_STATUS_CHARS`.
const OVER_LIMIT: &str = "The status is over the character limit";
/// 422 body for a `visibility` outside the five names.
const BAD_VISIBILITY: &str = "Invalid visibility";
/// 422 body for a `content_type` outside the three advertised formats.
const BAD_CONTENT_TYPE: &str = "Invalid content_type";

/// `POST /api/v1/statuses` with the JSON `body` as `token` (`None`: no
/// credential at all).
async fn publish(app: &Router, token: Option<&str>, body: Value) -> common::TestResponse {
    let request = common::TestRequest::post(STATUSES).json(&body);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// A creation, asserted 200, answered as JSON.
async fn posted(app: &Router, token: &str, body: Value) -> Value {
    let response = publish(app, Some(token), body).await;
    common::assert_answer(&response, StatusCode::OK, "a post that passes");
    response.json_value()
}

/// The id a creation answered with, as the row's own `UUIDv7` text.
fn id_of(value: &Value) -> Uuid {
    let text = value["id"].as_str().expect("the entity carries its id");
    Uuid::parse_str(text).expect("the entity id is an id")
}

/// The stored row behind a creation's answer. A refusal never reaches this:
/// no answer, no id, nothing stored.
async fn created_row(pool: &PgPool, value: &Value) -> Status {
    status::get_by_id(pool, id_of(value))
        .await
        .unwrap()
        .expect("the created status is stored live")
}

/// How many statuses this database holds: what a refusal never adds to and
/// every post adds exactly one to.
async fn stored_count(pool: &PgPool) -> i64 {
    sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM statuses")
        .fetch_one(pool)
        .await
        .expect("the count reads")
        .0
}

/// The account `id` as it stands now, for the counters a post moves.
async fn author_now(pool: &PgPool, id: Uuid) -> User {
    user::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the author exists")
}

/// The `status_mentions` links of one status, as the mention model reads
/// them, in position order.
async fn mention_links(pool: &PgPool, id: Uuid) -> Vec<Uuid> {
    let links = mention::for_statuses(pool, std::slice::from_ref(&id))
        .await
        .expect("the mention links read");
    links
        .get(&id)
        .map(|users| users.iter().map(|account| account.id).collect())
        .unwrap_or_default()
}

/// The names `status_hashtags` links to one status, as the hashtag model
/// reads them, in name order.
async fn tag_links(pool: &PgPool, id: Uuid) -> Vec<String> {
    let links = hashtag::for_statuses(pool, std::slice::from_ref(&id))
        .await
        .expect("the tag links read");
    links
        .get(&id)
        .map(|tags| tags.iter().map(|tag| tag.name.clone()).collect())
        .unwrap_or_default()
}

/// A minimal post is created, answered as the [`Status`] entity and stored
/// as the row that entity describes — local, public, its urls derived from
/// its own minted id.
#[tokio::test]
async fn a_minimal_post_creates_the_status_it_answers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let value = posted(&app, &token, json!({"status": "hello fediverse"})).await;
    let row = created_row(&pool, &value).await;

    assert_eq!(value["id"], row.id.to_string(), "the entity is the row");
    assert_eq!(
        value["uri"],
        format!("{TEST_BASE_URL}/objects/{}", row.id),
        "a local status owns this instance's object id (decision 6)"
    );
    assert_eq!(
        value["url"],
        json!(format!("{TEST_BASE_URL}/notice/{}", row.id)),
        "and the human URL derived from the same id"
    );
    assert_eq!(value["content"], "<p>hello fediverse</p>", "rendered HTML");
    assert_eq!(
        value["pleroma"]["content"],
        json!({ "text/plain": "hello fediverse" }),
        "the source text the author wrote"
    );
    assert_eq!(value["visibility"], "public", "the default scope");
    assert_eq!(value["sensitive"], false, "the default flag");
    assert_eq!(value["spoiler_text"], "", "no content warning");
    assert_eq!(value["language"], Value::Null, "no stated language");
    assert_eq!(value["in_reply_to_id"], Value::Null, "a root post");
    assert_eq!(value["reblog"], Value::Null, "and not a reblog");
    assert_eq!(value["mentions"], json!([]), "nothing mentioned");
    assert_eq!(value["tags"], json!([]), "nothing tagged");
    assert_eq!(value["media_attachments"], json!([]), "nothing attached");
    assert_eq!(value["reblogs_count"], 0, "every counter starts at zero");
    assert_eq!(value["favourites_count"], 0, "all four of them");
    assert_eq!(value["replies_count"], 0, "including replies");
    assert_eq!(value["pleroma"]["quotes_count"], 0, "and quotes");
    assert_eq!(value["account"]["acct"], "alice", "rendered as its author");
    assert_eq!(value["pleroma"]["local"], true, "a posted status is local");
    assert_eq!(
        value["pleroma"]["conversation_id"],
        row.conversation_id.to_string(),
        "a root status is its own conversation"
    );
    assert_eq!(
        (
            row.user_id,
            row.raw_text.as_str(),
            row.content_type.as_str(),
            row.visibility,
        ),
        (
            alice.id,
            "hello fediverse",
            "text/plain",
            Visibility::Public
        ),
        "and the row holds what the body said"
    );
}

/// The author's `statuses_count` moves with every post that passes, and not
/// with the ones that do not.
#[tokio::test]
async fn a_post_counts_for_its_author_and_a_refusal_does_not() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        0,
        "a fresh account has not posted"
    );
    let first = posted(&app, &token, json!({"status": "one"})).await;
    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        1,
        "the first post is counted"
    );
    let second = posted(&app, &token, json!({"status": "two"})).await;
    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        2,
        "and the second is counted on top of it"
    );
    assert_ne!(
        id_of(&first),
        id_of(&second),
        "two posts are two rows, each its own id"
    );

    let refused = publish(&app, Some(token.as_str()), json!({"status": "   "})).await;
    common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, "a blank post");
    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        2,
        "a refusal counts nothing"
    );
    assert_eq!(
        stored_count(&pool).await,
        2,
        "and stores no row, not even a tombstone"
    );
    assert!(
        author_now(&pool, alice.id).await.last_status_at.is_some(),
        "and the author knows when they last posted"
    );
}

/// All five visibilities round trip, `local` answering its own name exactly
/// as Pleroma's renderer emits it.
#[tokio::test]
async fn every_visibility_is_stored_and_answered() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for name in ["public", "unlisted", "private", "direct", "local"] {
        let value = posted(
            &app,
            &token,
            json!({"status": "said it", "visibility": name}),
        )
        .await;
        assert_eq!(value["visibility"], name, "the entity answers {name}");
        assert_eq!(
            value["content"], "<p>said it</p>",
            "the same text in every scope, {name}"
        );
        assert_eq!(
            created_row(&pool, &value).await.visibility.as_str(),
            name,
            "and the row stores {name}"
        );
    }
}

/// The account's own settings are the defaults: its `default_scope` decides
/// the visibility and its `sensitive_default` the flag, until the body names
/// either.
#[tokio::test]
async fn the_author_s_settings_are_the_default_visibility_and_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let shy = common::create_local_user_with(&pool, "shy", |new| NewUser {
        default_scope: "private".to_owned(),
        sensitive_default: Flag::new(true),
        ..new
    })
    .await;
    let shy_token = common::create_app_and_token(&pool, &shy, WRITE).await;
    let (_open, open_token) = user_with_token(&pool, "open", WRITE).await;
    let app = common::test_app(pool.clone());

    let own = posted(&app, &shy_token, json!({"status": "as I always post"})).await;
    assert_eq!(
        own["visibility"], "private",
        "the account's default_scope decides a bare post"
    );
    assert_eq!(own["sensitive"], true, "and its sensitive_default flags it");

    let chosen = posted(
        &app,
        &shy_token,
        json!({"status": "otherwise", "visibility": "unlisted", "sensitive": false}),
    )
    .await;
    assert_eq!(chosen["visibility"], "unlisted", "and the body wins");
    assert_eq!(
        chosen["sensitive"], false,
        "including against a `true` default"
    );

    let plain = posted(&app, &open_token, json!({"status": "stock account"})).await;
    assert_eq!(
        plain["visibility"], "public",
        "an account on the stock default posts public"
    );
    assert_eq!(plain["sensitive"], false, "and never sensitive");
}

/// The three advertised content types render their own way: markdown
/// becomes the markup it means, submitted HTML is reduced to the allowlist,
/// and plain text is escaped so it can say those characters at all.
#[tokio::test]
async fn each_content_type_renders_in_its_own_format() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let markdown = posted(
        &app,
        &token,
        json!({"status": "a **bold** claim", "content_type": "text/markdown"}),
    )
    .await;
    assert_eq!(
        markdown["content"], "<p>a <strong>bold</strong> claim</p>",
        "markdown bold becomes <strong>"
    );
    assert_eq!(
        markdown["pleroma"]["content"],
        json!({ "text/plain": "a **bold** claim" }),
        "while the source stays what was typed"
    );

    let html = posted(
        &app,
        &token,
        json!({"status": "<b>rich</b><script>steal()</script>", "content_type": "text/html"}),
    )
    .await;
    assert_eq!(
        html["content"], "<b>rich</b>",
        "the allowed tag survives, the script element goes with its text"
    );

    let plain = posted(&app, &token, json!({"status": "<b>rich</b>"})).await;
    assert_eq!(
        plain["content"], "<p>&lt;b&gt;rich&lt;/b&gt;</p>",
        "no content_type is plain text, and plain text is escaped"
    );
    assert_eq!(
        created_row(&pool, &plain).await.content_type,
        "text/plain",
        "and is stored as such"
    );
}

/// The name and website of the app behind the token are stored as the
/// status's application and answer in the entity's `application` block;
/// an app that has no website answers that key as null.
#[tokio::test]
async fn the_application_behind_the_token_is_recorded() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, stock_token) = user_with_token(&pool, "alice", WRITE).await;
    let client = common::create_app_named(&pool, "Wired Client", Some(&alice)).await;
    let named_token = common::create_token(&pool, &alice, &client, WRITE).await;
    // The same registration the fixture does, plus the one column it leaves
    // out: an app that names its homepage.
    let sited = oauth_app::create(
        &pool,
        &NewOauthApp {
            client_name: "Sited Client".to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: WRITE.to_owned(),
            website: Some("https://client.example".to_owned()),
            user_id: Some(alice.id),
        },
    )
    .await
    .expect("the website-carrying app registers");
    let sited_token = common::create_token(&pool, &alice, &sited, WRITE).await;
    let app = common::test_app(pool.clone());

    let stock = posted(
        &app,
        &stock_token,
        json!({"status": "from the fixture app"}),
    )
    .await;
    assert_eq!(
        stock["application"],
        json!({ "name": "Test Client", "website": Value::Null }),
        "the token's app is named, and a website it does not have is null"
    );

    let value = posted(
        &app,
        &named_token.token,
        json!({"status": "from a named client"}),
    )
    .await;
    assert_eq!(
        value["application"],
        json!({ "name": "Wired Client", "website": Value::Null }),
        "the entity answers the client that posted"
    );
    let row = created_row(&pool, &value).await;
    assert_eq!(
        (
            row.application_name.as_deref(),
            row.application_website.as_deref(),
        ),
        (Some("Wired Client"), None),
        "and the row stores its name, and no website"
    );

    let with_site = posted(
        &app,
        &sited_token.token,
        json!({"status": "from a client with a homepage"}),
    )
    .await;
    assert_eq!(
        with_site["application"],
        json!({ "name": "Sited Client", "website": "https://client.example" }),
        "a client that names a homepage answers both"
    );
    let sited_row = created_row(&pool, &with_site).await;
    assert_eq!(
        (
            sited_row.application_name.as_deref(),
            sited_row.application_website.as_deref(),
        ),
        (Some("Sited Client"), Some("https://client.example")),
        "and the row stores the pair"
    );
}

/// A mention of a local account becomes its `h-card` anchor in the content,
/// a row in `status_mentions`, and an entry in the entity's `mentions`; an
/// unknown name is left as the text it was written as.
#[tokio::test]
async fn a_local_mention_is_linked_stored_and_answered() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let value = posted(&app, &token, json!({"status": "hi @bob, and @nobody"})).await;
    let row = created_row(&pool, &value).await;

    assert_eq!(
        value["content"],
        format!("<p>hi {}, and @nobody</p>", mention_html(&bob.ap_id, "bob")),
        "the known account becomes its h-card, the unknown one stays text"
    );
    assert_eq!(
        mention_links(&pool, row.id).await,
        vec![bob.id],
        "exactly the account this instance knows is linked in status_mentions"
    );
    assert_eq!(
        value["mentions"],
        json!([{ "id": bob.id.to_string(), "username": "bob",
                 "url": bob.ap_id, "acct": "bob" }]),
        "and it answers in the entity's `mentions`"
    );
}

/// Hashtags are extracted and deduplicated into one stored link each, linked
/// at every spelling in the content, and answered in the entity's `tags`.
#[tokio::test]
async fn hashtags_are_stored_linked_and_answered() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let value = posted(
        &app,
        &token,
        json!({"status": "hello #Rust and #rust, also #123"}),
    )
    .await;
    let row = created_row(&pool, &value).await;

    assert_eq!(
        tag_links(&pool, row.id).await,
        vec!["rust"],
        "one stored tag for both spellings, folded to lowercase, and #123 is \
         no tag at all"
    );
    assert_eq!(
        value["tags"],
        json!([{ "name": "rust", "url": format!("{TEST_BASE_URL}/tags/rust") }]),
        "the tag answers in the entity's `tags`"
    );
    assert_eq!(
        value["content"],
        format!(
            "<p>hello {tag} and {tag}, also #123</p>",
            tag = hashtag_html(TEST_BASE_URL, "rust")
        ),
        "and every spelling of it is linked in the content"
    );
}

/// The length limit is `MAX_STATUS_CHARS` counted by `status_length`, which
/// charges every bare URL 23 characters: a text the raw count would refuse
/// posts, and one the weighted count refuses does not.
#[tokio::test]
async fn the_length_limit_charges_every_url_twenty_three_characters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let mut config = common::test_config();
    config.max_status_chars = 25;
    let app = common::test_app_with_config(pool.clone(), config);

    let url = "https://example.com/a/path/longer/than/twenty-three";
    assert!(
        url.chars().count() > 25,
        "the fixture URL is on its own past the cap this test sets"
    );

    let text = format!("x {url}");
    let value = posted(&app, &token, json!({"status": &text})).await;
    assert_eq!(
        value["pleroma"]["content"],
        json!({ "text/plain": text }),
        "2 characters plus one URL weighted at 23 is the 25 this instance \
         allows, so the post is stored with the text as written"
    );

    let over = format!("{url} tail");
    refused(&app, &token, json!({"status": over}), OVER_LIMIT).await;
    assert_eq!(
        stored_count(&pool).await,
        1,
        "23 + 5 is over the same limit, and nothing was written for it"
    );
}

/// The same charge at the production default of `MAX_STATUS_CHARS`: two
/// hundred long bare URLs — ten thousand raw characters — weigh 200 × 23
/// plus their separators and post, while three hundred of the same URLs are
/// the 422. The pair is what proves the limit is measured by that weighting,
/// not by the length the body has as written.
#[tokio::test]
async fn the_default_limit_charges_each_url_its_weight_not_its_length() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    assert_eq!(
        common::test_config().max_status_chars,
        5000,
        "this test posts under the default limit, not a shrunk one"
    );

    let url = "https://example.com/a/very/long/path?query=string";
    let fit = vec![url; 200].join(" ");
    let over = vec![url; 300].join(" ");
    assert!(
        fit.chars().count() > 5000 && over.chars().count() > 5000,
        "both bodies are far more than 5000 characters as written: {} and {}",
        fit.chars().count(),
        over.chars().count()
    );
    assert!(
        status_length(&fit, 23) < 200 * 24 && status_length(&over, 23) > 5000,
        "the first measures well under 200 × 24 and so fits, the second's \
         300 charges alone pass the cap"
    );

    let value = posted(&app, &token, json!({"status": &fit})).await;
    assert_eq!(
        created_row(&pool, &value).await.raw_text,
        fit,
        "two hundred URLs post, and the row holds every one verbatim"
    );

    refused(&app, &token, json!({"status": over}), OVER_LIMIT).await;
    assert_eq!(
        stored_count(&pool).await,
        1,
        "three hundred of the same URLs are refused, and wrote nothing"
    );
}

/// A post with nothing to say — no `status`, an empty one, a blank one — is
/// 422 in the wording Pleroma uses for an empty payload, and writes nothing.
#[tokio::test]
async fn an_empty_status_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for body in [
        json!({}),
        json!({"status": ""}),
        json!({"status": "   "}),
        json!({"status": "\n\t \n"}),
        json!({"status": "  ", "spoiler_text": "a warning is no text"}),
    ] {
        let response = publish(&app, Some(token.as_str()), body.clone()).await;
        common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, "an empty post");
        common::assert_error(&response, EMPTY_STATUS, &format!("the body `{body}`"));
    }
    assert_eq!(stored_count(&pool).await, 0, "and nothing was written");
}

/// The 422 matrix over HTTP: one probe per refusal the endpoint owes — an
/// unknown `visibility`, an unknown `content_type` (down to Pleroma's own
/// `text/x-markdown` alias), a `spoiler_text` one character past 200, a
/// `language` one past 10 and a `status` that is only whitespace — each 422
/// in its own wording, and none of them reaches the database.
#[tokio::test]
async fn the_422_matrix_refuses_each_bad_parameter_in_its_own_wording() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for (body, message) in [
        (
            json!({"status": "hi", "visibility": "bogus"}),
            BAD_VISIBILITY,
        ),
        (
            json!({"status": "hi", "content_type": "text/x-markdown"}),
            BAD_CONTENT_TYPE,
        ),
        (
            json!({"status": "hi", "spoiler_text": "x".repeat(201)}),
            "Validation failed: Spoiler text is too long (maximum is 200 characters)",
        ),
        (
            json!({"status": "hi", "language": "a".repeat(11)}),
            "Validation failed: Language is too long (maximum is 10 characters)",
        ),
        (json!({"status": "   \t\n  "}), EMPTY_STATUS),
    ] {
        let response = publish(&app, Some(token.as_str()), body.clone()).await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("the refusal for `{body}`"),
        );
        common::assert_error(&response, message, &format!("the wording for `{body}`"));
    }

    assert_eq!(stored_count(&pool).await, 0, "not one of them wrote a row");
    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        0,
        "and none of them counted for the author"
    );
}

/// A refusal: the same post, asserted 422 with `message`, then dropped.
async fn refused(app: &Router, token: &str, body: Value, message: &str) {
    let response = publish(app, Some(token), body).await;
    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a refused post",
    );
    common::assert_error(&response, message, "the refusal wording");
}

/// A form body creates exactly what the same keys would as JSON, with the
/// string spellings of booleans and defaults accepted (decision 10).
#[tokio::test]
async fn a_form_body_posts_like_the_json_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[
            ("status", "posted from a form"),
            ("visibility", "unlisted"),
            ("sensitive", "true"),
            ("spoiler_text", "news"),
            ("language", "nl"),
        ])
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "a form post");
    let value = response.json_value();
    let row = created_row(&pool, &value).await;

    assert_eq!(value["content"], "<p>posted from a form</p>", "the text");
    assert_eq!(value["visibility"], "unlisted", "the scope");
    assert_eq!(value["sensitive"], true, "a form `true` is the flag on");
    assert_eq!(value["spoiler_text"], "news", "the content warning");
    assert_eq!(value["language"], "nl", "and the language tag");
    assert_eq!(
        (
            row.raw_text.as_str(),
            row.visibility,
            row.sensitive,
            row.spoiler_text.as_str(),
            row.language.as_deref(),
        ),
        (
            "posted from a form",
            Visibility::Unlisted,
            true,
            "news",
            Some("nl")
        ),
        "stored exactly as the JSON body would have stored it"
    );

    let off = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "flag off"), ("sensitive", "0")])
        .send(&app)
        .await;
    common::assert_answer(&off, StatusCode::OK, "the second form post");
    assert_eq!(
        off.json_value()["sensitive"],
        false,
        "`0` is the flag off, whatever the account defaults to"
    );
}

/// The write route runs the whole `AuthUser` + `write:statuses` pipeline: no
/// credential and an unresolvable one are the 401s, a token that names no
/// user and one without the scope are the 403s, and none of them posts.
#[tokio::test]
async fn the_route_needs_a_user_token_that_writes_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (reader, reader_token) = user_with_token(&pool, "reader", "read:statuses").await;
    let (_writer, writer_token) = user_with_token(&pool, "writer", WRITE).await;
    let ownerless = common::create_app_only_token(&pool, WRITE).await;
    let app = common::test_app(pool.clone());
    let body = json!({"status": "who is speaking"});

    let anonymous = publish(&app, None, body.clone()).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no credential");
    common::assert_error(&anonymous, NO_TOKEN, "no credential");

    let unknown = publish(&app, Some("token-that-was-never-minted"), body.clone()).await;
    common::assert_answer(&unknown, StatusCode::UNAUTHORIZED, "an unknown token");
    common::assert_error(&unknown, INVALID_TOKEN, "an unknown token");

    let narrow = publish(&app, Some(reader_token.as_str()), body.clone()).await;
    common::assert_answer(&narrow, StatusCode::FORBIDDEN, "a token without the scope");
    common::assert_error(&narrow, NO_WRITE, "a token without the scope");

    let app_only = publish(&app, Some(ownerless.as_str()), body.clone()).await;
    common::assert_answer(&app_only, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(&app_only, USER_REQUIRED, "an app-only token");

    assert_eq!(stored_count(&pool).await, 0, "not one refusal wrote a row");
    assert_eq!(
        author_now(&pool, reader.id).await.statuses_count,
        0,
        "and the near-enough token gained nothing"
    );

    let granted = posted(&app, &writer_token, body).await;
    assert_eq!(
        granted["account"]["acct"], "writer",
        "the scope does post, as its own author"
    );
}

/// The route is in the `statuses_actions` bucket Pleroma throttles status
/// writes through: the request past the budget is refused before the
/// service runs, so nothing is posted and nothing is counted.
#[tokio::test]
async fn the_route_spends_the_statuses_actions_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("statuses_actions=2:600"),
    );

    for spent in 0..2 {
        let response = publish(&app, Some(token.as_str()), json!({"status": "counted"})).await;
        common::assert_answer(&response, StatusCode::OK, "a request inside the budget");
        assert_eq!(
            response.header("x-ratelimit-limit"),
            Some("2"),
            "the bucket's budget"
        );
        assert_eq!(
            response.header("x-ratelimit-remaining"),
            Some(if spent == 0 { "1" } else { "0" }),
            "the window counts down"
        );
    }

    let throttled = publish(&app, Some(token.as_str()), json!({"status": "throttled"})).await;
    common::assert_answer(&throttled, StatusCode::TOO_MANY_REQUESTS, "over the budget");
    common::assert_error(&throttled, "Throttled", "over the budget");
    assert_eq!(
        stored_count(&pool).await,
        2,
        "the third request never reached the service"
    );
    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        2,
        "and so the author counts two posts, not three"
    );
}

/// The batch read and the write share the `/statuses` path without either
/// losing its gate: the throttled `POST` is a 401 with no credential while
/// the unthrottled `GET` still answers the anonymous `[]` it always did.
#[tokio::test]
async fn the_write_route_does_not_leak_onto_the_read_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("statuses_actions=1:600"),
    );
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;

    let read = batch(&app, None, "").await;
    common::assert_answer(&read, StatusCode::OK, "the anonymous batch");
    assert_eq!(
        read.header("x-ratelimit-limit"),
        None,
        "and it carries no throttle headers"
    );

    let first = publish(&app, Some(token.as_str()), json!({"status": "one"})).await;
    common::assert_answer(&first, StatusCode::OK, "the first post of the window");
    let second = publish(&app, Some(token.as_str()), json!({"status": "two"})).await;
    common::assert_answer(&second, StatusCode::TOO_MANY_REQUESTS, "the second");
    let read_again = batch(&app, None, "").await;
    common::assert_answer(
        &read_again,
        StatusCode::OK,
        "the batch is still unthrottled",
    );
    assert_eq!(
        read_again.json_value(),
        json!([]),
        "and the visible ids it answers are unchanged by the throttle"
    );
}
