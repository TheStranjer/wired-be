//! The status endpoints over HTTP: the write `POST /api/v1/statuses` (WBE-76),
//! the reads `GET /api/v1/statuses/{id}` and the batch
//! `GET /api/v1/statuses?id[]=…` (WBE-77), the thread read
//! `GET /api/v1/statuses/{id}/context` (WBE-78) and the write
//! `DELETE /api/v1/statuses/{id}` (WBE-79).
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
//! `GET /api/v1/statuses/{id}/context` (WBE-78) is Pleroma's `context`: the
//! thread around the named status as `{"ancestors": [...], "descendants":
//! [...]}`. Ancestors are the reply chain above it, root first; descendants
//! are every reply below it, across every branch, in flat chronological
//! order. Both arms carry only what the visibility model admits — an
//! invisible or deleted link is skipped but walked *past*, so the thread
//! beyond it still arrives — and descendants additionally drop statuses by
//! accounts the viewer blocked or muted (the viewer's own excepted). Each
//! descendant's `pleroma.parent_visible` says whether its direct parent
//! came back in the answer. The named status itself never appears in either
//! arm, and a status this caller may not see is the shared 404.
//!
//! Both reads run Pleroma's optional-auth pipeline: no token proceeds, a
//! presented user token must carry `read:statuses` (403 for one that does
//! not, even on a public post), an app-only token proceeds unauthenticated,
//! and a token that does not resolve is the extractor's 401.
//!
//! `POST /api/v1/statuses` (WBE-76) is Pleroma's `create` and runs the strict
//! pipeline — `AuthUser` plus `write:statuses` plus the
//! `statuses_actions` throttle — and the ticket's own refusals: 422 for a
//! post with nothing to say, one over `MAX_STATUS_CHARS` (counted with every
//! URL at 23 characters), an unknown `visibility` or `content_type`, an
//! over-long `spoiler_text` or `language`. What passes is stored as a `local`
//! row with its mentions in `status_mentions` and its tags in
//! `status_hashtags`, the posting client recorded as its application, the
//! author's `statuses_count` stepped, and the whole thing answered as the
//! `Status` entity the author themselves sees — from a JSON or a form body
//! alike. WBE-87 adds the route's two request-level arms: an
//! `Idempotency-Key` header whose retry answers the status the key created
//! without writing a second one (until the key's 24 hours run out and the
//! `idempotency.cleanup` sweep frees the pair), and a `preview` parameter
//! that runs every check and the full render and answers the would-be
//! entity with no write, no hook and no key at all. WBE-90 adds Pleroma's
//! `expires_in`: a top-level integer of seconds, at least
//! `MIN_EXPIRATION_SECS` (600 by default — a shorter one is 422
//! `{"error":"expires_in must be at least 600 seconds"}`, no integer is 422
//! of the extractor's own), stored as the row's `expires_at`, rendered as
//! `pleroma.expires_at`, and scheduled for deletion as one
//! `statuses.expire` job due at that instant — with the periodic
//! `statuses.expire_sweep` pass behind it (see
//! `wired_be::jobs::statuses`).
//!
//! WBE-85 adds `in_reply_to_id`: the reply's parent is read with the show
//! route's visibility rules — a malformed, unknown, deleted or
//! poster-invisible parent is the shared 404, and a reblog row folds to the
//! original it boosts — and the post lands in the parent's conversation
//! with the parent's author auto-mentioned in `status_mentions` (the text
//! itself is never modified), an audience capped by the parent's (`direct`
//! forces `direct`, `private` pulls a requested `public`/`unlisted` down,
//! and an unsubmitted scope never goes wider than the parent), and the
//! parent's language as the fallback tag; the parent's `replies_count`
//! steps with the insert.
//!
//! WBE-86 adds `media_ids`: the post's attachments, as a JSON array or
//! `media_ids[]` form keys, are the poster's own uploads no status has
//! claimed. More than `MAX_MEDIA_ATTACHMENTS` (4 by default) is 422
//! `{"error":"Too many attachments"}`, and an id that is malformed,
//! unknown, somebody else's or already posted is 422
//! `{"error":"Media not found"}` — refused inside the insert's own
//! transaction, so a short claim writes no status and frees nothing it
//! took. Accepted ids are claimed at the order the body sent them, that
//! order is what the entity's `media_attachments` answers, a body
//! carrying at least one attachment may leave `status` empty, and
//! attachments ride every visibility, `direct` included.
//!
//! `DELETE /api/v1/statuses/{id}` (WBE-79) is Pleroma's `delete`: a user
//! token with `write:statuses`, on the same `statuses_actions` bucket the
//! favourite writes ride, behind the same status gate — a malformed, unknown,
//! deleted or invisible id is the shared 404. Past the gate only the author
//! may delete their own post, while an admin or a moderator may delete
//! anybody's; a visible post of somebody else's is a 403 that keeps Pleroma's
//! `"Record not found"` wording. The answer is the `Status` entity exactly as
//! the caller had seen it, before the row became a tombstone, plus the raw
//! source text under the top-level `text` key (Mastodon's delete-and-redraw).
//! Deleting a post takes every live reblog of it down in the same
//! transaction, steps the counters each of them fed, and leaves the media
//! attachment rows in the table for a later cleanup job.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        block,
        follow::{self, FollowState},
        hashtag,
        media_attachment::{self, NewMediaAttachment},
        mention,
        mute::{self, MuteOptions},
        oauth_app::{self, NewOauthApp},
        status::{self, NewStatus, Status, Visibility},
        user::{self, Flag, NewUser, User},
    },
    util::{
        entities::{hashtag_html, mention_html},
        format::status_length,
        time::parse_time,
    },
};

use crate::common;

/// The body every miss on these routes carries (decision 8).
const NOT_FOUND: &str = "Record not found";
/// The 403 body the scope gate renders for a user token without the scope.
const NO_SCOPE: &str = "Insufficient permissions: read:statuses.";
/// The 401 body `MaybeAuthUser` renders for a token that does not resolve.
const INVALID_TOKEN: &str = "The access token is invalid";
/// The scope both read routes ask a presented user token for.
const SCOPES: &str = "read:statuses";
/// The scope the write routes — `POST /api/v1/statuses` and the delete — ask
/// a user token for.
const WRITE: &str = "write:statuses";
/// [`WRITE`] beside `read:statuses`: what a delete test's token carries when
/// the test also reads the status back through the show route.
const WRITE_AND_READ: &str = "write:statuses read:statuses";
/// The 403 body the scope gate renders for a user token without [`WRITE`].
const NO_WRITE: &str = "Insufficient permissions: write:statuses.";
/// The 401 body `AuthUser` renders when the request carries no token at all.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body `AuthUser` renders for a token bound to no user.
const USER_REQUIRED: &str = "This API requires an authenticated user";
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

/// `DELETE /api/v1/statuses/{id}` as `token`. The id stays text so the miss
/// probes can feed any spelling as the path segment.
async fn destroy(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::delete(&format!("/api/v1/statuses/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// `POST /api/v1/statuses/{id}/favourite` as `token`, for the test that shows
/// the two writes spend one bucket.
async fn favourite(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/favourite"))
        .bearer(token)
        .send(app)
        .await
}

/// A local user named `nickname` with a `write:statuses` bearer token (and
/// `read:statuses`, so a test can read the status back through the show
/// route): the caller the delete route answers.
async fn writer(pool: &PgPool, nickname: &str) -> (User, String) {
    user_with_token(pool, nickname, WRITE_AND_READ).await
}

/// A local user named `nickname` whose `write:statuses` token belongs to an
/// account `customize` has made staff (Pleroma's `messages_delete` privilege).
async fn staff_with_token<F>(pool: &PgPool, nickname: &str, customize: F) -> (User, String)
where
    F: FnOnce(NewUser) -> NewUser,
{
    let created = common::create_local_user_with(pool, nickname, customize).await;
    let token = common::create_app_and_token(pool, &created, WRITE_AND_READ).await;
    (created, token)
}

/// The stored row behind `id`, tombstone included: what a delete leaves.
async fn stored(pool: &PgPool, id: Uuid) -> Status {
    status::get_by_id_including_deleted(pool, id)
        .await
        .expect("the row reads")
        .expect("the fixture status is stored")
}

/// The live-post count stored for `author`, for the counter assertions.
async fn statuses_count(pool: &PgPool, author: &User) -> i32 {
    user::get_by_id(pool, author.id)
        .await
        .expect("the account reads")
        .expect("the fixture account exists")
        .statuses_count
}

/// One media attachment of `author` posted on `status_id`, as its row id: the
/// row a status delete leaves behind.
async fn attach(pool: &PgPool, author: &User, status_id: Uuid) -> Uuid {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            status_id: Some(status_id),
            url: format!("{TEST_BASE_URL}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("a fixture attachment inserts")
    .id
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

/// The author's delete answers 200 with the entity exactly as the show route
/// had just rendered it, plus the raw source text at the top level; the row is
/// left as a text-free tombstone with its `uri` reserved, its author stops
/// counting it, its media attachment stays in the table for the cleanup job,
/// and the id answers the shared 404 from then on.
#[tokio::test]
async fn the_author_deletes_their_own_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let post = common::create_status(&pool, &alice, "take me back", Visibility::Public).await;
    let attachment = attach(&pool, &alice, post.id).await;
    let id = post.id.to_string();
    let before = show(&app, Some(token.as_str()), &id).await.json_value();

    let response = destroy(&app, &token, &id).await;
    common::assert_answer(&response, StatusCode::OK, "the author's delete");
    let body = response.json_value();
    assert_eq!(body["text"], "take me back", "the raw source text");
    let mut expected = before;
    expected["text"] = json!("take me back");
    assert_eq!(
        body, expected,
        "the entity is the whole status as it was before the deletion"
    );

    let tombstone = stored(&pool, post.id).await;
    assert!(tombstone.deleted_at.is_some(), "the row is a tombstone");
    assert_eq!(tombstone.content, "", "its content is cleared");
    assert_eq!(tombstone.raw_text, "", "its source text with it");
    assert_eq!(tombstone.uri, post.uri, "and its uri stays reserved");
    assert_eq!(
        statuses_count(&pool, &alice).await,
        0,
        "the author no longer counts the post"
    );
    assert!(
        media_attachment::get_by_id(&pool, attachment)
            .await
            .expect("the media row reads")
            .is_some(),
        "the attachment row survives, referenced by no live status"
    );
    common::assert_all_refused(
        &[show(&app, Some(token.as_str()), &id).await],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the id after its delete",
    );
}

/// Deleting a reply steps the counters that fed on it and leaves the rest of
/// the thread standing: the parent's `replies_count` returns to zero, the
/// answerer's `statuses_count` loses the answer, the parent stays live and
/// readable, and the deleted entity still names what it answered.
#[tokio::test]
async fn deleting_a_reply_steps_the_counters_that_counted_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _alice_token) = writer(&pool, "alice").await;
    let (bob, bob_token) = writer(&pool, "bob").await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let answer = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            in_reply_to_id: Some(root.id),
            ..NewStatus::local(&bob, "an answer")
        },
    )
    .await
    .expect("a reply to a stored status inserts");

    let response = destroy(&app, &bob_token, &answer.id.to_string()).await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        "the author's delete of their own reply",
    );
    assert_eq!(
        response.json_value()["in_reply_to_id"],
        root.id.to_string(),
        "the answer's entity still names the post it was answering"
    );

    let parent = stored(&pool, root.id).await;
    assert_eq!(parent.replies_count, 0, "the parent stops counting it");
    assert!(parent.deleted_at.is_none(), "and stays live");
    assert_eq!(
        statuses_count(&pool, &bob).await,
        0,
        "the answerer counts nothing"
    );
    assert_eq!(
        statuses_count(&pool, &alice).await,
        1,
        "the root's author is untouched"
    );
    common::assert_answer(
        &show(&app, None, &root.id.to_string()).await,
        StatusCode::OK,
        "the parent after its answer was deleted",
    );
}

/// A delete takes the boosts down with the post, in the same transaction:
/// every live reblog of it becomes a tombstone that no reader answers, the
/// deleted status lands at zero boosts, each booster stops counting their
/// reblog, and a reblog of a *different* status is left alone.
#[tokio::test]
async fn deleting_a_status_takes_its_reblogs_with_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = writer(&pool, "alice").await;
    let (bob, bob_token) = writer(&pool, "bob").await;
    let (carol, carol_token) = writer(&pool, "carol").await;
    let app = common::test_app(pool.clone());
    let post = common::create_status(&pool, &alice, "boost me then", Visibility::Public).await;
    let other = common::create_status(&pool, &carol, "someone else's", Visibility::Public).await;
    let mine = reblog_of(&pool, &bob, &post).await;
    let theirs = reblog_of(&pool, &carol, &post).await;
    let untouched = reblog_of(&pool, &bob, &other).await;

    common::assert_answer(
        &destroy(&app, &alice_token, &post.id.to_string()).await,
        StatusCode::OK,
        "the author's delete of a boosted post",
    );

    assert_eq!(
        stored(&pool, post.id).await.reblogs_count,
        0,
        "the deleted status counts no boosts any more"
    );
    common::assert_all_refused(
        &[
            show(&app, Some(bob_token.as_str()), &mine.id.to_string()).await,
            show(&app, Some(carol_token.as_str()), &theirs.id.to_string()).await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "a reblog the delete took down",
    );
    for reblog in [&mine, &theirs] {
        let tombstone = stored(&pool, reblog.id).await;
        assert!(
            tombstone.deleted_at.is_some(),
            "its row is a tombstone, not a hard delete"
        );
        assert_eq!(
            tombstone.reblog_of_id,
            Some(post.id),
            "and it still names what it boosted"
        );
    }
    common::assert_answer(
        &show(&app, Some(bob_token.as_str()), &untouched.id.to_string()).await,
        StatusCode::OK,
        "bob's reblog of another status, untouched",
    );
    assert_eq!(
        statuses_count(&pool, &bob).await,
        1,
        "bob keeps one of his two boosts"
    );
    assert_eq!(
        statuses_count(&pool, &carol).await,
        1,
        "carol keeps her post and loses her boost"
    );
    assert_eq!(
        statuses_count(&pool, &alice).await,
        0,
        "alice's post is gone"
    );
}

/// Staff delete anything the gate lets them see: an admin takes a stranger's
/// `public` post down and a moderator takes a stranger's `local` one (rule 4
/// shows it to any local account), both answered with the author's entity and
/// both leaving a tombstone and a stepped-down count.
#[tokio::test]
async fn staff_delete_anyone_s_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _alice_token) = writer(&pool, "alice").await;
    let (_root, root_token) = staff_with_token(&pool, "root", |new| NewUser {
        is_admin: Flag::new(true),
        ..new
    })
    .await;
    let (_mod, mod_token) = staff_with_token(&pool, "mod", |new| NewUser {
        is_moderator: Flag::new(true),
        ..new
    })
    .await;
    let app = common::test_app(pool.clone());
    let public =
        common::create_status(&pool, &alice, "admins can take this", Visibility::Public).await;
    let kept_local =
        common::create_status(&pool, &alice, "and moderators can", Visibility::Local).await;

    let by_admin = destroy(&app, &root_token, &public.id.to_string()).await;
    common::assert_answer(
        &by_admin,
        StatusCode::OK,
        "an admin deleting a stranger's post",
    );
    assert_eq!(
        by_admin.json_value()["account"]["acct"],
        "alice",
        "the entity speaks as its author"
    );
    assert_eq!(
        by_admin.json_value()["text"],
        "admins can take this",
        "and carries its source text to a staff caller"
    );
    assert!(
        stored(&pool, public.id).await.deleted_at.is_some(),
        "the post is a tombstone"
    );

    let by_mod = destroy(&app, &mod_token, &kept_local.id.to_string()).await;
    common::assert_answer(
        &by_mod,
        StatusCode::OK,
        "a moderator deleting a stranger's local-only post",
    );
    assert_eq!(
        by_mod.json_value()["visibility"],
        "local",
        "the entity keeps the stored visibility"
    );
    assert!(
        stored(&pool, kept_local.id).await.deleted_at.is_some(),
        "and that row is tombstoned too"
    );
    assert_eq!(
        statuses_count(&pool, &alice).await,
        0,
        "alice counts neither of the two any more"
    );
}

/// A visible post that is not the caller's is Pleroma's ownership refusal:
/// `"Record not found"` under a 403, for a stranger and for an accepted
/// follower of the author alike, and the post comes through untouched.
#[tokio::test]
async fn another_user_cannot_delete_someone_elses_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = writer(&pool, "alice").await;
    let (bob, bob_token) = writer(&pool, "bob").await;
    let (_carol, carol_token) = writer(&pool, "carol").await;
    let app = common::test_app(pool.clone());
    followed(&pool, &bob, &alice).await;
    let post =
        common::create_status(&pool, &alice, "not yours to delete", Visibility::Public).await;
    let id = post.id.to_string();

    for (token, who) in [
        (carol_token.as_str(), "a stranger"),
        (bob_token.as_str(), "an accepted follower"),
    ] {
        let refused = destroy(&app, token, &id).await;
        common::assert_answer(&refused, StatusCode::FORBIDDEN, who);
        common::assert_error(&refused, NOT_FOUND, who);
    }

    assert!(
        stored(&pool, post.id).await.deleted_at.is_none(),
        "the refused deletes took nothing down"
    );
    assert_eq!(
        statuses_count(&pool, &alice).await,
        1,
        "and no counter moved"
    );
    common::assert_answer(
        &show(&app, Some(alice_token.as_str()), &id).await,
        StatusCode::OK,
        "the post, still readable by its author",
    );
}

/// The gate answers first and answers alike: a malformed id, an id that names
/// nothing, a post the caller may not see and an already deleted one are the
/// one 404 — and the author's second attempt at their own tombstone steps no
/// counter a second time.
#[tokio::test]
async fn the_delete_route_answers_the_shared_gate_with_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = writer(&pool, "alice").await;
    let (_carol, carol_token) = writer(&pool, "carol").await;
    let app = common::test_app(pool.clone());
    let hidden =
        common::create_status(&pool, &alice, "for my followers", Visibility::Private).await;
    let doomed = common::create_status(&pool, &alice, "already gone", Visibility::Public).await;
    deleted(&pool, &doomed).await;

    for (probe, who) in [
        ("not-an-id".to_owned(), "text that is not a UUID"),
        (format!("urn:uuid:{UNKNOWN_ID}"), "an unread id spelling"),
        (UNKNOWN_ID.to_owned(), "an id that names nothing"),
        (hidden.id.to_string(), "a post the caller may not see"),
        (doomed.id.to_string(), "an already deleted post"),
    ] {
        common::assert_all_refused(
            &[destroy(&app, &carol_token, &probe).await],
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            who,
        );
    }
    common::assert_all_refused(
        &[destroy(&app, &alice_token, &doomed.id.to_string()).await],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the author, deleting their own tombstone again",
    );
    assert_eq!(
        statuses_count(&pool, &alice).await,
        1,
        "so the second attempt moves nothing"
    );
}

/// The route is a user write: no token is the 401, a token that does not
/// resolve is the extractor's 401, a token bound to no user and a user token
/// without `write:statuses` are the two 403s — and none of them touched the
/// status.
#[tokio::test]
async fn the_delete_route_demands_a_write_statuses_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _alice_token) = writer(&pool, "alice").await;
    let (_reader, reader_token) = user_with_token(&pool, "bob", SCOPES).await;
    let app_only = common::create_app_only_token(&pool, WRITE).await;
    let app = common::test_app(pool.clone());
    let post = common::create_status(&pool, &alice, "still here", Visibility::Public).await;
    let id = post.id.to_string();

    let no_token = common::TestRequest::delete(&format!("/api/v1/statuses/{id}"))
        .send(&app)
        .await;
    common::assert_answer(&no_token, StatusCode::UNAUTHORIZED, "no token at all");
    common::assert_error(&no_token, NO_TOKEN, "no token at all");

    for (refused, status, message, who) in [
        (
            destroy(&app, "token-that-was-never-minted", &id).await,
            StatusCode::UNAUTHORIZED,
            INVALID_TOKEN,
            "a token that does not resolve",
        ),
        (
            destroy(&app, &app_only, &id).await,
            StatusCode::FORBIDDEN,
            USER_REQUIRED,
            "an app-only token",
        ),
        (
            destroy(&app, &reader_token, &id).await,
            StatusCode::FORBIDDEN,
            NO_WRITE,
            "a user token without write:statuses",
        ),
    ] {
        common::assert_answer(&refused, status, who);
        common::assert_error(&refused, message, who);
    }

    assert!(
        stored(&pool, post.id).await.deleted_at.is_none(),
        "the post survived every refusal"
    );
}

/// The delete shares Pleroma's `statuses_actions` bucket with the favourite
/// writes: one window covers both paths, and the request that runs the budget
/// out is the bucket's 429 before it ever reaches the handler.
#[tokio::test]
async fn the_delete_route_shares_the_statuses_actions_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("statuses_actions=2:600"),
    );
    let (alice, token) = user_with_token(&pool, "alice", "write").await;
    let liked = common::create_status(&pool, &alice, "liked first", Visibility::Public).await;
    let doomed = common::create_status(&pool, &alice, "deleted second", Visibility::Public).await;
    let throttled =
        common::create_status(&pool, &alice, "over the budget", Visibility::Public).await;

    let first = favourite(&app, &token, &liked.id.to_string()).await;
    common::assert_answer(&first, StatusCode::OK, "a favourite inside the budget");
    assert_eq!(first.header("x-ratelimit-limit"), Some("2"), "the bucket");
    assert_eq!(
        first.header("x-ratelimit-remaining"),
        Some("1"),
        "one request spent"
    );

    let second = destroy(&app, &token, &doomed.id.to_string()).await;
    common::assert_answer(&second, StatusCode::OK, "a delete inside the budget");
    assert_eq!(
        second.header("x-ratelimit-remaining"),
        Some("0"),
        "and the window is out for this route too"
    );

    let over = destroy(&app, &token, &throttled.id.to_string()).await;
    common::assert_answer(&over, StatusCode::TOO_MANY_REQUESTS, "over the budget");
    common::assert_error(&over, "Throttled", "the delete shares the bucket");
    assert!(
        stored(&pool, throttled.id).await.deleted_at.is_none(),
        "the refused delete never reached the handler"
    );
}

/// `GET /api/v1/statuses/{id}/context` as `token` (`None`: anonymous). The id
/// stays text so the miss probes can feed any spelling as the path segment.
async fn context(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}/context"));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// Inserts `author`'s `text` as a reply to `parent` at `visibility`, and
/// returns its row: the thread fixture the context tests walk.
async fn reply_under(
    pool: &PgPool,
    author: &User,
    parent: &Status,
    text: &str,
    visibility: Visibility,
) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility,
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent.user_id),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a reply fixture inserts")
}

/// The `id` values of one arm of a context answer, in response order.
fn context_ids(body: &Value, arm: &str) -> Vec<String> {
    body[arm]
        .as_array()
        .unwrap_or_else(|| panic!("the `{arm}` arm is an array"))
        .iter()
        .map(|status| status["id"].as_str().expect("a status id").to_owned())
        .collect()
}

/// The `pleroma.parent_visible` of the entry `id` of one arm.
fn parent_visible(body: &Value, arm: &str, id: &str) -> bool {
    body[arm]
        .as_array()
        .expect("the arm is an array")
        .iter()
        .find(|status| status["id"] == id)
        .unwrap_or_else(|| panic!("`{id}` is listed under `{arm}`"))["pleroma"]["parent_visible"]
        .as_bool()
        .expect("parent_visible is a bool")
}

/// The answer's shape, checked once on the linear thread: an object of
/// exactly the two arrays.
fn assert_two_arms(body: &Value) {
    let arms: Vec<&String> = body
        .as_object()
        .expect("the answer is an object")
        .keys()
        .collect();
    assert_eq!(arms, vec!["ancestors", "descendants"], "the two arms");
}

/// A linear thread answers its chain in both directions from any entry:
/// ancestors root-first (oldest first), descendants in flat chronological
/// order, each one's `parent_visible` true because its direct parent came
/// back — and a root with no replies at all answers two empty arrays.
#[tokio::test]
async fn a_linear_thread_answers_both_ways_around_its_named_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let second = reply_under(&pool, &bob, &root, "second", Visibility::Public).await;
    let third = reply_under(&pool, &carol, &second, "third", Visibility::Public).await;
    let leaf = reply_under(&pool, &dave, &third, "leaf", Visibility::Public).await;

    // Mid-thread: the whole chain above, the rest below.
    let response = context(&app, None, &third.id.to_string()).await;
    common::assert_answer(&response, StatusCode::OK, "mid-thread, anonymous");
    let body = response.json_value();
    assert_two_arms(&body);
    assert_eq!(
        context_ids(&body, "ancestors"),
        ids_of(&[&root, &second]),
        "root first, the named status itself absent"
    );
    assert_eq!(context_ids(&body, "descendants"), ids_of(&[&leaf]));
    assert!(
        parent_visible(&body, "descendants", &leaf.id.to_string()),
        "the leaf answers the third, which came back"
    );
    assert_eq!(
        body["ancestors"][0]["account"]["acct"], "alice",
        "the arrays carry rendered entities, not bare ids"
    );

    // From the root: nothing above, both later replies below in id order.
    let from_root = context(&app, None, &root.id.to_string()).await.json_value();
    assert!(
        context_ids(&from_root, "ancestors").is_empty(),
        "the root has no thread above it"
    );
    assert_eq!(
        context_ids(&from_root, "descendants"),
        ids_of(&[&second, &third, &leaf]),
        "flat chronological order, not tree order"
    );
    assert!(
        parent_visible(&from_root, "descendants", &second.id.to_string()),
        "the root is shown, so its direct reply has its parent"
    );

    // From the leaf: everything above, nothing below.
    let from_leaf = context(&app, None, &leaf.id.to_string()).await.json_value();
    assert_eq!(
        context_ids(&from_leaf, "ancestors"),
        ids_of(&[&root, &second, &third]),
        "and never the named status itself"
    );
    assert!(
        context_ids(&from_leaf, "descendants").is_empty(),
        "the leaf has no thread below it"
    );

    // A root nobody answered: both arrays empty, and still a 200.
    let lonely = common::create_status(&pool, &alice, "lonely", Visibility::Public).await;
    let from_lonely = context(&app, None, &lonely.id.to_string())
        .await
        .json_value();
    assert!(
        context_ids(&from_lonely, "ancestors").is_empty(),
        "nothing above"
    );
    assert!(
        context_ids(&from_lonely, "descendants").is_empty(),
        "nothing below"
    );
}

/// A branching thread answers every branch below the named status in one
/// flat, id-ascending (chronological) list — the second hop of the left
/// branch follows the right branch's root reply, not its own parent — and
/// each entry's `parent_visible` names whoever it really answered.
#[tokio::test]
async fn a_branching_thread_answers_a_flat_chronological_descendants_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let left = reply_under(&pool, &bob, &root, "left", Visibility::Public).await;
    let right = reply_under(&pool, &carol, &root, "right", Visibility::Public).await;
    let deep = reply_under(&pool, &dave, &left, "deep", Visibility::Public).await;

    let from_root = context(&app, None, &root.id.to_string()).await.json_value();
    assert_eq!(
        context_ids(&from_root, "descendants"),
        ids_of(&[&left, &right, &deep]),
        "creation order across branches: `deep` last, though it answers `left`"
    );
    for reply in [&left, &right, &deep] {
        assert!(
            parent_visible(&from_root, "descendants", &reply.id.to_string()),
            "`{}` answers a row that came back",
            reply.id
        );
    }

    // A branch entry sees only its own ancestry and its own descendants.
    let from_left = context(&app, None, &left.id.to_string()).await.json_value();
    assert_eq!(context_ids(&from_left, "ancestors"), ids_of(&[&root]));
    assert_eq!(context_ids(&from_left, "descendants"), ids_of(&[&deep]));
    let from_right = context(&app, None, &right.id.to_string())
        .await
        .json_value();
    assert_eq!(context_ids(&from_right, "ancestors"), ids_of(&[&root]));
    assert!(
        context_ids(&from_right, "descendants").is_empty(),
        "the other branch is not below this reply"
    );
}

/// A `private` reply is the author's and an accepted follower's, not a
/// stranger's — but skipping it never breaks the walk: the stranger's
/// answer still carries the public reply *under* it with
/// `parent_visible: false`, and its ancestors list still reaches the root
/// past the hidden link.
#[tokio::test]
async fn an_invisible_reply_is_skipped_but_the_walk_continues_past_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let (_mallory, mallory_token) = user_with_token(&pool, "mallory", SCOPES).await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let secret = reply_under(&pool, &alice, &root, "secret", Visibility::Private).await;
    followed(&pool, &bob, &alice).await;
    let answer = reply_under(&pool, &bob, &secret, "answer", Visibility::Public).await;

    // A stranger (and the anonymous caller alike): the private link drops
    // out of both arms, the thread past it stays, and its parent shows the
    // hole.
    for (who, token) in [
        ("a stranger", Some(mallory_token.as_str())),
        ("anonymous", None),
    ] {
        let hidden = context(&app, token, &root.id.to_string())
            .await
            .json_value();
        assert_eq!(
            context_ids(&hidden, "descendants"),
            ids_of(&[&answer]),
            "{who}: the private reply is out, its public answer stays"
        );
        assert!(
            !parent_visible(&hidden, "descendants", &answer.id.to_string()),
            "{who}: the hidden parent did not come back"
        );
        let past = context(&app, token, &answer.id.to_string())
            .await
            .json_value();
        assert_eq!(
            context_ids(&past, "ancestors"),
            ids_of(&[&root]),
            "{who}: the walk passes through the hidden link to the root"
        );
    }

    // The accepted follower sees the whole stretch, holes and all.
    let follower_root = context(&app, Some(bob_token.as_str()), &root.id.to_string())
        .await
        .json_value();
    assert_eq!(
        context_ids(&follower_root, "descendants"),
        ids_of(&[&secret, &answer]),
        "the follower's descendants carry the private reply too"
    );
    assert!(
        parent_visible(&follower_root, "descendants", &answer.id.to_string()),
        "and its parent is there for them"
    );
    let follower_answer = context(&app, Some(bob_token.as_str()), &answer.id.to_string())
        .await
        .json_value();
    assert_eq!(
        context_ids(&follower_answer, "ancestors"),
        ids_of(&[&root, &secret])
    );

    // The private reply the stranger may not see is the shared 404 even as
    // the named status of a context read.
    common::assert_all_refused(
        &[
            context(&app, Some(mallory_token.as_str()), &secret.id.to_string()).await,
            context(&app, None, &secret.id.to_string()).await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the hidden reply named directly",
    );
}

/// Statuses by accounts the viewer blocked or muted drop out of
/// `descendants` — while the thread below the silenced reply still arrives,
/// `parent_visible: false` marking its author's absence, and the silenced
/// author's own public post staying in `ancestors`, which the visibility
/// model alone narrows. An anonymous caller, who silences nobody, sees
/// everything public.
#[tokio::test]
async fn a_silenced_author_vanishes_from_the_descendants() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (carol, carol_token) = user_with_token(&pool, "carol", SCOPES).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let erin = common::create_local_user(&pool, "erin").await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &carol, "root", Visibility::Public).await;
    let blocked = reply_under(&pool, &bob, &root, "blocked", Visibility::Public).await;
    let muted = reply_under(&pool, &dave, &root, "muted", Visibility::Public).await;
    let own = reply_under(&pool, &carol, &root, "own", Visibility::Public).await;
    let under = reply_under(&pool, &erin, &blocked, "under", Visibility::Public).await;
    let block_row = block::block(&pool, TEST_BASE_URL, &carol, &bob)
        .await
        .expect("carol's block of bob stores");
    assert_eq!(
        block_row.blocked_id, bob.id,
        "the fixture block is carol's on bob"
    );
    let mute_row = mute::mute(&pool, &carol, &dave, MuteOptions::default())
        .await
        .expect("carol's mute of dave stores");
    assert_eq!(
        mute_row.muted_id, dave.id,
        "and the mute is carol's on dave"
    );

    let for_carol = context(&app, Some(carol_token.as_str()), &root.id.to_string())
        .await
        .json_value();
    assert_eq!(
        context_ids(&for_carol, "descendants"),
        ids_of(&[&own, &under]),
        "the blocked and muted authors' rows are out; the viewer's own row \
         stays, and so does the live reply under the silenced one"
    );
    assert!(
        parent_visible(&for_carol, "descendants", &own.id.to_string()),
        "carol answers the root, which is shown"
    );
    assert!(
        !parent_visible(&for_carol, "descendants", &under.id.to_string()),
        "the row under bob's answer marks its parent missing"
    );

    // Silence is the viewer's own filter, not a visibility rule: bob's
    // public reply still stands in carol's ancestors chain, and an
    // anonymous caller's descendants list is whole.
    let for_under = context(&app, Some(carol_token.as_str()), &under.id.to_string())
        .await
        .json_value();
    assert_eq!(
        context_ids(&for_under, "ancestors"),
        ids_of(&[&root, &blocked]),
        "ancestors are narrowed by visibility alone"
    );
    let anon = context(&app, None, &root.id.to_string()).await.json_value();
    assert_eq!(
        context_ids(&anon, "descendants"),
        ids_of(&[&blocked, &muted, &own, &under]),
        "nobody is silenced for the anonymous caller"
    );
    assert!(
        parent_visible(&anon, "descendants", &under.id.to_string()),
        "and its parent came back for them"
    );
}

/// A deleted reply is out of both arms — the visibility model lets a
/// tombstone stand for nobody — while the live reply under it walks past:
/// it arrives in the descendants with `parent_visible: false`, and the
/// root still heads its ancestors. Naming the tombstone itself is the 404.
#[tokio::test]
async fn a_deleted_reply_drops_out_of_both_arms() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let middle = reply_under(&pool, &alice, &root, "middle", Visibility::Public).await;
    let leaf = reply_under(&pool, &alice, &middle, "leaf", Visibility::Public).await;
    deleted(&pool, &middle).await;

    let from_root = context(&app, None, &root.id.to_string()).await.json_value();
    assert_eq!(
        context_ids(&from_root, "descendants"),
        ids_of(&[&leaf]),
        "the tombstone is out, its live child still arrives"
    );
    assert!(
        !parent_visible(&from_root, "descendants", &leaf.id.to_string()),
        "and the child marks its deleted parent missing"
    );

    let from_leaf = context(&app, None, &leaf.id.to_string()).await.json_value();
    assert_eq!(
        context_ids(&from_leaf, "ancestors"),
        ids_of(&[&root]),
        "the walk passes through the deleted link to the root"
    );

    common::assert_all_refused(
        &[context(&app, None, &middle.id.to_string()).await],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the tombstone named directly",
    );
}

/// The context route runs the show route's optional-auth gate: no token and
/// an app-only token proceed as anonymous, a presented user token without
/// `read:statuses` is the scope 403, a token that does not resolve the 401,
/// and a malformed, unknown, deleted or invisible status is one
/// indistinguishable 404.
#[tokio::test]
async fn the_context_route_runs_the_optional_read_statuses_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let (_noscope, noscope_token) = user_with_token(&pool, "mallory", WRITE).await;
    let app_only = common::create_app_only_token(&pool, SCOPES).await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let secret = common::create_status(&pool, &alice, "secret", Visibility::Private).await;
    let gone = common::create_status(&pool, &alice, "gone", Visibility::Public).await;
    deleted(&pool, &gone).await;
    let id = root.id.to_string();

    // Unauthenticated reads succeed on public threads, private rows aside.
    common::assert_answer(
        &context(&app, None, &id).await,
        StatusCode::OK,
        "no token at all",
    );
    common::assert_answer(
        &context(&app, Some(&app_only), &id).await,
        StatusCode::OK,
        "an app-only token, which proceeds unauthenticated",
    );
    for (who, token) in [
        ("a stranger's user token", Some(bob_token.as_str())),
        ("the anonymous caller", None),
    ] {
        common::assert_all_refused(
            &[context(&app, token, &secret.id.to_string()).await],
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("{who}, naming a private status"),
        );
    }
    common::assert_all_refused(
        &[context(&app, Some(&noscope_token), &id).await],
        StatusCode::FORBIDDEN,
        NO_SCOPE,
        "a user token without read:statuses",
    );
    let refused = context(&app, Some("token-that-was-never-minted"), &id).await;
    common::assert_answer(&refused, StatusCode::UNAUTHORIZED, "a dead token");
    common::assert_error(&refused, INVALID_TOKEN, "a dead token");

    for (probe, who) in [
        ("not-an-id".to_owned(), "text that is not a UUID"),
        (format!("urn:uuid:{UNKNOWN_ID}"), "an unread id spelling"),
        (UNKNOWN_ID.to_owned(), "an id that names nothing"),
        (gone.id.to_string(), "a deleted status"),
    ] {
        common::assert_all_refused(
            &[context(&app, None, &probe).await],
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            who,
        );
    }
}

// ────────────────────────────  POST /api/v1/statuses  ────────────────────────

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

/// One post naming an account and a tag links both in the same content, in
/// mention order and whatever case the text spelled the account in: a
/// case-different `@BOB` still resolves to bob's stored row and profile URL,
/// and the tag answers folded.
#[tokio::test]
async fn a_mention_and_a_hashtag_link_one_post_with_the_acct_in_any_case() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let value = posted(&app, &token, json!({"status": "hi @BOB #tag"})).await;
    let row = created_row(&pool, &value).await;

    assert_eq!(
        value["content"],
        format!(
            "<p>hi {} {}</p>",
            mention_html(&bob.ap_id, "BOB"),
            hashtag_html(TEST_BASE_URL, "tag"),
        ),
        "the case-different mention links to bob's profile URL (the `ap_id` \
         fallback the fixture's row leaves it on) displayed as typed, and the \
         tag links beside it"
    );
    assert_eq!(
        mention_links(&pool, row.id).await,
        vec![bob.id],
        "the account the folded name finds is linked in `status_mentions`"
    );
    assert_eq!(
        tag_links(&pool, row.id).await,
        vec!["tag"],
        "and the tag in `status_hashtags`"
    );
    assert_eq!(
        value["mentions"],
        json!([{ "id": bob.id.to_string(), "username": "bob",
                 "url": bob.ap_id, "acct": "bob" }]),
        "bob answers in the entity's `mentions`, by the entity's own spelling"
    );
    assert_eq!(
        value["tags"],
        json!([{ "name": "tag", "url": format!("{TEST_BASE_URL}/tags/tag") }]),
        "and the tag in its `tags`"
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

// ─────────────  POST /api/v1/statuses: Idempotency-Key  ─────────────

/// The row-count helpers above stop at `statuses`; the key claims live in
/// their own table, and every keyed test reads this to see one claim per
/// pair, no claim for a preview, and one for a key that moved.
async fn key_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM idempotency_keys")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// `POST /api/v1/statuses` with the JSON `body` as `token`, carrying an
/// `Idempotency-Key` header.
async fn publish_keyed(app: &Router, token: &str, key: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .json(&body)
        .bearer(token)
        .header("idempotency-key", key)
        .send(app)
        .await
}

/// The same post under one key twice: the second request creates nothing
/// and answers the first status's entity — same id, same render, one row,
/// one claim, one counted post. The retry body is not even looked at.
#[tokio::test]
async fn the_same_idempotency_key_creates_one_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let first = publish_keyed(
        &app,
        &token,
        "K-1",
        json!({"status": "keyed once @bob #tag"}),
    )
    .await;
    common::assert_answer(&first, StatusCode::OK, "the keyed post");
    let second = publish_keyed(&app, &token, "K-1", json!({"status": "a different retry"})).await;
    common::assert_answer(&second, StatusCode::OK, "the replay of the same key");

    assert_eq!(
        second.json_value()["id"],
        first.json_value()["id"],
        "the same key answers the same id"
    );
    assert_eq!(
        second.json_value()["content"],
        format!(
            "<p>keyed once {} {}</p>",
            mention_html(&bob.ap_id, "bob"),
            hashtag_html(TEST_BASE_URL, "tag"),
        ),
        "and the first body's entity — the retry body changed nothing"
    );
    assert_eq!(
        stored_count(&pool).await,
        1,
        "the retry created no second status"
    );
    assert_eq!(author_now(&pool, alice.id).await.statuses_count, 1);
    assert_eq!(key_rows(&pool).await, 1, "one claim for the pair");
    let row = created_row(&pool, &first.json_value()).await;
    assert_eq!(mention_links(&pool, row.id).await, vec![bob.id]);
    assert_eq!(tag_links(&pool, row.id).await, vec!["tag"]);
}

/// Two keys are two posts: the same body under a different key creates
/// afresh, each key keeping its own status.
#[tokio::test]
async fn different_idempotency_keys_create_two_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let body = json!({"status": "posted twice, under two keys"});
    let first = publish_keyed(&app, &token, "K-1", body.clone()).await;
    let second = publish_keyed(&app, &token, "K-2", body).await;
    common::assert_answer(&first, StatusCode::OK, "the first key's post");
    common::assert_answer(&second, StatusCode::OK, "the second key's post");

    assert_ne!(
        id_of(&first.json_value()),
        id_of(&second.json_value()),
        "a different key is a different post"
    );
    assert_eq!(stored_count(&pool).await, 2);
    assert_eq!(key_rows(&pool).await, 2);
    assert_eq!(author_now(&pool, alice.id).await.statuses_count, 2);
}

/// One key text belongs to the account that sent it: two users posting
/// under `"shared"` get two statuses, and each one's replay answers its
/// own author's post.
#[tokio::test]
async fn an_idempotency_key_is_its_owner_s_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, alice_token) = user_with_token(&pool, "alice", WRITE).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", WRITE).await;
    let app = common::test_app(pool.clone());

    let mine = publish_keyed(&app, &alice_token, "shared", json!({"status": "alice's"})).await;
    let theirs = publish_keyed(&app, &bob_token, "shared", json!({"status": "bob's"})).await;
    common::assert_answer(&mine, StatusCode::OK, "alice's keyed post");
    common::assert_answer(&theirs, StatusCode::OK, "bob's keyed post");

    assert_ne!(
        id_of(&theirs.json_value()),
        id_of(&mine.json_value()),
        "the same key text is not the same key across accounts"
    );
    assert_eq!(mine.json_value()["account"]["acct"], "alice");
    assert_eq!(theirs.json_value()["account"]["acct"], "bob");

    let alice_again =
        publish_keyed(&app, &alice_token, "shared", json!({"status": "ignored"})).await;
    assert_eq!(
        alice_again.json_value()["id"],
        mine.json_value()["id"],
        "alice's retry replays alice's post"
    );
    let bob_again = publish_keyed(&app, &bob_token, "shared", json!({"status": "ignored"})).await;
    assert_eq!(
        bob_again.json_value()["id"],
        theirs.json_value()["id"],
        "and bob's replays bob's"
    );
    assert_eq!(stored_count(&pool).await, 2);
    assert_eq!(key_rows(&pool).await, 2, "one claim per (account, key)");
}

/// The cap is 255 characters, and a longer value is ignored rather than
/// refused: 256 of the same text name no key, so each post creates and no
/// claim is stored; the 255-character edge is honored like any shorter key.
#[tokio::test]
async fn a_key_longer_than_255_characters_is_ignored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let over = "k".repeat(256);
    let first = publish_keyed(&app, &token, &over, json!({"status": "one"})).await;
    let second = publish_keyed(&app, &token, &over, json!({"status": "two"})).await;
    common::assert_answer(&first, StatusCode::OK, "the first over-long post");
    common::assert_answer(&second, StatusCode::OK, "the second over-long post");
    assert_ne!(
        id_of(&first.json_value()),
        id_of(&second.json_value()),
        "an ignored header cannot replay anything"
    );
    assert_eq!(stored_count(&pool).await, 2, "both posts stand");
    assert_eq!(key_rows(&pool).await, 0, "and no claim was stored");

    let edge = "k".repeat(255);
    let third = publish_keyed(&app, &token, &edge, json!({"status": "three"})).await;
    let again = publish_keyed(&app, &token, &edge, json!({"status": "ignored"})).await;
    assert_eq!(
        again.json_value()["id"],
        third.json_value()["id"],
        "the cap itself is a key like any other"
    );
    assert_eq!(key_rows(&pool).await, 1);
    assert_eq!(stored_count(&pool).await, 3);
}

/// A replayed key whose status has been deleted is no replay-able answer:
/// the next post under that key creates afresh, the claim moves onto it,
/// and from then on the key replays the new status.
#[tokio::test]
async fn a_deleted_status_frees_its_idempotency_key() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let first = publish_keyed(
        &app,
        &token,
        "K-1",
        json!({"status": "post then delete me"}),
    )
    .await;
    common::assert_answer(&first, StatusCode::OK, "the keyed post");
    let doomed = id_of(&first.json_value());
    let gone = destroy(&app, &token, &doomed.to_string()).await;
    common::assert_answer(&gone, StatusCode::OK, "the delete");

    let fresh = publish_keyed(&app, &token, "K-1", json!({"status": "the next one"})).await;
    common::assert_answer(&fresh, StatusCode::OK, "the post under the freed key");
    assert_ne!(
        id_of(&fresh.json_value()),
        doomed,
        "a tombstone is not what the key replays"
    );
    let restated = publish_keyed(&app, &token, "K-1", json!({"status": "ignored"})).await;
    assert_eq!(
        restated.json_value()["id"],
        fresh.json_value()["id"],
        "and the moved claim now replays the fresh status"
    );
    assert_eq!(
        key_rows(&pool).await,
        1,
        "still exactly one mapping for the pair"
    );
}

// ─────────────  POST /api/v1/statuses: preview  ─────────────

/// The three side tables a create fills: every preview test requires them
/// all empty, because the point of a preview is that it renders without
/// writing.
async fn side_rows(pool: &PgPool) -> (i64, i64, i64) {
    let mentions = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM status_mentions")
        .fetch_one(pool)
        .await
        .expect("the mention count reads");
    let tags = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM hashtags")
        .fetch_one(pool)
        .await
        .expect("the tag count reads");
    let links = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM status_hashtags")
        .fetch_one(pool)
        .await
        .expect("the tag link count reads");
    (mentions, tags, links)
}

/// A preview answers the [`Status`] entity a create of the same body would
/// have: fresh id with the urls it derives, the render with the mention and
/// the tag resolved into it, zero counters, `created_at` now, the author's
/// view — and no row anywhere behind it.
#[tokio::test]
async fn a_preview_answers_the_entity_and_writes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let response = publish(
        &app,
        Some(token.as_str()),
        json!({"status": "hello @bob and @nobody, see #Rust", "preview": true}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "a preview");
    let value = response.json_value();
    let id = id_of(&value);

    assert_eq!(
        value["uri"],
        format!("{TEST_BASE_URL}/objects/{id}"),
        "the entity carries the id a create would mint, with its urls"
    );
    assert_eq!(value["url"], json!(format!("{TEST_BASE_URL}/notice/{id}")));
    assert_eq!(
        value["content"],
        format!(
            "<p>hello {} and @nobody, see {}</p>",
            mention_html(&bob.ap_id, "bob"),
            hashtag_html(TEST_BASE_URL, "rust"),
        ),
        "the same render a create stores, the unknown name still plain text"
    );
    assert_eq!(
        value["mentions"],
        json!([{ "id": bob.id.to_string(), "username": "bob",
                 "url": bob.ap_id, "acct": "bob" }]),
        "the mention is resolved into the entity"
    );
    assert_eq!(
        value["tags"],
        json!([{ "name": "rust", "url": format!("{TEST_BASE_URL}/tags/rust") }]),
        "and so is the tag, folded"
    );
    assert_eq!(value["reblogs_count"], 0, "every counter zero");
    assert_eq!(value["favourites_count"], 0, "all four");
    assert_eq!(value["replies_count"], 0);
    assert_eq!(value["pleroma"]["quotes_count"], 0);
    assert_eq!(value["visibility"], "public", "the author's defaults apply");
    assert_eq!(value["account"]["acct"], "alice", "as the author sees it");
    assert_eq!(
        value["pleroma"]["content"],
        json!({ "text/plain": "hello @bob and @nobody, see #Rust" }),
        "the source text beside the render"
    );
    let created_at = parse_time(value["created_at"].as_str().expect("a timestamp"))
        .expect("the Mastodon-format stamp parses");
    assert!(
        (Utc::now() - created_at).num_seconds().abs() < 120,
        "created_at is now: {created_at}"
    );

    assert_eq!(stored_count(&pool).await, 0, "no status row");
    assert_eq!(side_rows(&pool).await, (0, 0, 0), "no mention or tag rows");
    assert_eq!(key_rows(&pool).await, 0, "and no claim");
    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        0,
        "and the author's count stands"
    );
    assert!(
        status::get_by_id(&pool, id).await.unwrap().is_none(),
        "the previewed id names nothing in the database"
    );
}

/// The flag is a [`flex`] boolean read through the query string as well as
/// either body encoding (decision 10), and every false spelling — the
/// absence of the key included — stays a real post.
///
/// [`flex`]: wired_be::extract::flex
#[tokio::test]
async fn the_preview_flag_reads_every_parameter_spelling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let from_form = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "previewed from a form"), ("preview", "true")])
        .send(&app)
        .await;
    common::assert_answer(&from_form, StatusCode::OK, "a form preview");
    assert_eq!(
        stored_count(&pool).await,
        0,
        "the form preview wrote nothing"
    );

    let from_query = common::TestRequest::post(&format!("{STATUSES}?preview=1"))
        .bearer(&token)
        .json(&json!({"status": "previewed via the query"}))
        .send(&app)
        .await;
    common::assert_answer(&from_query, StatusCode::OK, "a query preview");
    assert_eq!(stored_count(&pool).await, 0, "and the query wrote nothing");

    let spelled_off = publish(
        &app,
        Some(token.as_str()),
        json!({"status": "a real post", "preview": "off"}),
    )
    .await;
    common::assert_answer(&spelled_off, StatusCode::OK, "the `off` spelling posts");
    assert_eq!(stored_count(&pool).await, 1, "and it stored its row");

    let bare = publish(&app, Some(token.as_str()), json!({"status": "no flag"})).await;
    common::assert_answer(&bare, StatusCode::OK, "the absent flag posts");
    assert_eq!(stored_count(&pool).await, 2, "as it always did");
}

/// Validation runs on previews exactly as on posts: the same 422s in the
/// same wordings, nothing written, and no key stored even when the request
/// carried one.
#[tokio::test]
async fn a_preview_owes_the_creation_refusals() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for (body, message) in [
        (json!({"status": "   ", "preview": true}), EMPTY_STATUS),
        (
            json!({"status": "hi", "visibility": "sideways", "preview": true}),
            BAD_VISIBILITY,
        ),
        (
            json!({"status": "hi", "content_type": "text/rtf", "preview": true}),
            BAD_CONTENT_TYPE,
        ),
        (
            json!({"status": "hi", "spoiler_text": "x".repeat(201), "preview": true}),
            "Validation failed: Spoiler text is too long (maximum is 200 characters)",
        ),
    ] {
        let response = publish(&app, Some(token.as_str()), body.clone()).await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("the refused preview `{body}`"),
        );
        common::assert_error(&response, message, &format!("the wording for `{body}`"));
    }

    let keyed_refusal =
        publish_keyed(&app, &token, "K-1", json!({"status": "", "preview": true})).await;
    common::assert_answer(
        &keyed_refusal,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a refused preview that carried a key",
    );

    assert_eq!(stored_count(&pool).await, 0, "not one refusal wrote a row");
    assert_eq!(key_rows(&pool).await, 0, "or stored a key");
}

/// A preview is not a create, so there is nothing for a key to remember:
/// previews under one key mint fresh ids and claim nothing, and the real
/// post that follows the previews claims the key as if it had never been
/// spoken.
#[tokio::test]
async fn preview_ignores_the_idempotency_key() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let first = publish_keyed(
        &app,
        &token,
        "K-1",
        json!({"status": "first look", "preview": true}),
    )
    .await;
    let second = publish_keyed(
        &app,
        &token,
        "K-1",
        json!({"status": "second look", "preview": true}),
    )
    .await;
    common::assert_answer(&first, StatusCode::OK, "the first preview");
    common::assert_answer(&second, StatusCode::OK, "the second preview");
    assert_ne!(
        id_of(&first.json_value()),
        id_of(&second.json_value()),
        "each preview mints its own id"
    );
    assert_eq!(stored_count(&pool).await, 0);
    assert_eq!(key_rows(&pool).await, 0, "and the key stayed unclaimed");

    let real = publish_keyed(&app, &token, "K-1", json!({"status": "the real post"})).await;
    common::assert_answer(&real, StatusCode::OK, "the post after the previews");
    assert_eq!(key_rows(&pool).await, 1, "now the key is claimed");
    let replay = publish_keyed(&app, &token, "K-1", json!({"status": "ignored"})).await;
    assert_eq!(
        replay.json_value()["id"],
        real.json_value()["id"],
        "and replays the post it finally created"
    );
    assert_eq!(stored_count(&pool).await, 1);
}

// ──────────────────  POST /api/v1/statuses: `expires_in`  ───────────────────

/// The job kind a post with an `expires_in` schedules for itself.
const EXPIRE_KIND: &str = "statuses.expire";

/// The `jobs` rows of `kind`, in insertion order.
async fn scheduled_jobs(pool: &PgPool, kind: &str) -> Vec<wired_be::models::job::Job> {
    sqlx::query_as::<_, wired_be::models::job::Job>(
        "SELECT * FROM jobs WHERE kind = $1 ORDER BY id",
    )
    .bind(kind)
    .fetch_all(pool)
    .await
    .expect("the job rows read")
}

/// A post carrying Pleroma's `expires_in` stores the instant that span
/// counts out, renders it as `pleroma.expires_at`, and schedules exactly one
/// `statuses.expire` job — on the scheduled queue, due at exactly the stored
/// instant, keyed `expire:<status_id>`. A form body's numeric string is the
/// same parameter; a nested `pleroma.expires_in` is no parameter at all.
#[tokio::test]
async fn a_post_with_expires_in_stores_the_expiry_and_schedules_its_delete() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let value = posted(
        &app,
        &token,
        json!({"status": "here for an hour", "expires_in": 3600}),
    )
    .await;
    let row = created_row(&pool, &value).await;

    let expires = row.expires_at.expect("the row carries its expiry");
    let span = expires - Utc::now();
    assert!(
        (span.num_seconds() - 3600).abs() <= 1,
        "the stored expiry is the hour the body asked for, not {span}"
    );
    assert_eq!(
        value["pleroma"]["expires_at"],
        wired_be::util::time::mastodon_time(&expires),
        "the entity renders the stored instant as `pleroma.expires_at`"
    );

    let jobs = scheduled_jobs(&pool, EXPIRE_KIND).await;
    assert_eq!(jobs.len(), 1, "one scheduled delete for the expiring post");
    let job = &jobs[0];
    assert_eq!(job.queue, "scheduled", "on the scheduled queue");
    assert_eq!(
        job.args,
        json!({"status_id": row.id.to_string()}),
        "the args name exactly the status id"
    );
    assert_eq!(job.run_at, expires, "due at exactly the stored expiry");
    assert_eq!(
        job.unique_key.as_deref(),
        Some(format!("expire:{}", row.id).as_str()),
        "keyed for deduplication as the ticket spells it"
    );

    // The form spelling is the same integer, and the nested key is nothing:
    // `expires_in` is a top-level parameter only.
    let from_form = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "form expiry"), ("expires_in", "7200")])
        .send(&app)
        .await;
    common::assert_answer(&from_form, StatusCode::OK, "a form body's expires_in");
    assert!(
        created_row(&pool, &from_form.json_value())
            .await
            .expires_at
            .is_some(),
        "and it expires"
    );
    let nested = posted(
        &app,
        &token,
        json!({"status": "no nested expiry", "pleroma": {"expires_in": 3600}}),
    )
    .await;
    assert_eq!(
        created_row(&pool, &nested).await.expires_at,
        None,
        "`pleroma.expires_in` is not accepted — only the top-level key expires"
    );
    assert_eq!(
        scheduled_jobs(&pool, EXPIRE_KIND).await.len(),
        2,
        "and it scheduled no delete"
    );
}

/// An `expires_in` under the instance's floor — including zero, negatives
/// and the floor's own minute-ward neighbour — is 422 with the message
/// naming that floor, and writes nothing.
#[tokio::test]
async fn an_expires_in_under_the_floor_is_refused_with_the_floor_in_words() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for short in [json!(-1), json!(0), json!(1), json!(599), json!("599")] {
        let refused = publish(
            &app,
            Some(&token),
            json!({"status": "too soon", "expires_in": short}),
        )
        .await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            "an expires_in under the floor",
        );
        common::assert_error(
            &refused,
            "expires_in must be at least 600 seconds",
            "the floor refusal",
        );
    }
    let refused = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "too soon"), ("expires_in", "300")])
        .send(&app)
        .await;
    common::assert_error(
        &refused,
        "expires_in must be at least 600 seconds",
        "the floor refusal, from a form body too",
    );

    assert_eq!(stored_count(&pool).await, 0, "and none of it was written");
    assert!(
        scheduled_jobs(&pool, EXPIRE_KIND).await.is_empty(),
        "or scheduled"
    );
}

/// An `expires_in` that is no integer at all is a 422 of its own; `null`
/// is not an expiry but the ordinary never-expiring post.
#[tokio::test]
async fn an_expires_in_that_is_no_integer_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for not_a_number in [
        json!("soon"),
        json!(60.5),
        json!(true),
        json!([3600]),
        json!("10m"),
    ] {
        let refused = publish(
            &app,
            Some(&token),
            json!({"status": "not a number", "expires_in": not_a_number}),
        )
        .await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a non-integer expires_in",
        );
    }
    assert_eq!(stored_count(&pool).await, 0, "none of them posted");

    let unexpiry = posted(
        &app,
        &token,
        json!({"status": "null is no expiry", "expires_in": null}),
    )
    .await;
    assert_eq!(
        created_row(&pool, &unexpiry).await.expires_at,
        None,
        "null is as absent as no key"
    );
    assert_eq!(
        unexpiry["pleroma"]["expires_at"],
        Value::Null,
        "and renders null"
    );
}

/// The floor is the configured `MIN_EXPIRATION_SECS`: an instance that
/// lowered it to 120 refuses 119 in its words and posts 120.
#[tokio::test]
async fn the_floor_and_its_message_come_from_the_configuration() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let config = wired_be::config::Config {
        min_expiration_secs: 120,
        ..common::test_config()
    };
    let app = common::test_app_with_config(pool.clone(), config);

    let refused = publish(
        &app,
        Some(&token),
        json!({"status": "two minutes", "expires_in": 119}),
    )
    .await;
    common::assert_error(
        &refused,
        "expires_in must be at least 120 seconds",
        "a custom floor names itself",
    );

    let value = posted(
        &app,
        &token,
        json!({"status": "two minutes", "expires_in": 120}),
    )
    .await;
    assert!(
        created_row(&pool, &value).await.expires_at.is_some(),
        "the custom floor itself posts"
    );
}

// ────────────────  POST /api/v1/statuses: `in_reply_to_id`  ────────────────

/// `bob`'s plain stored post of `text`, at `visibility` — the fixture parent.
async fn parent_post(pool: &PgPool, author: &User, text: &str, visibility: Visibility) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility,
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a fixture parent inserts")
}

/// A reply over HTTP answers its parent's ids, joins its conversation,
/// auto-mentions the parent's author (with the text itself untouched), and
/// steps the parent's `replies_count` — the whole reply contract through
/// the endpoint.
#[tokio::test]
async fn a_reply_answers_with_its_parent_and_counts_itself() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (bob, bob_token) = user_with_token(&pool, "bob", WRITE).await;
    let app = common::test_app(pool.clone());

    let parent = parent_post(&pool, &bob, "the parent", Visibility::Public).await;
    let value = posted(
        &app,
        &token,
        json!({
            "status": "a reply",
            "in_reply_to_id": parent.id.to_string(),
        }),
    )
    .await;
    let row = created_row(&pool, &value).await;

    assert_eq!(value["in_reply_to_id"], parent.id.to_string(), "the entity");
    assert_eq!(
        value["in_reply_to_account_id"],
        bob.id.to_string(),
        "and the parent's author"
    );
    assert_eq!(
        value["pleroma"]["in_reply_to_account_acct"], "bob",
        "whose acct answers in the Pleroma block"
    );
    assert_eq!(row.in_reply_to_id, Some(parent.id), "the row names it");
    assert_eq!(row.in_reply_to_account_id, Some(bob.id));
    assert_eq!(
        row.conversation_id, parent.conversation_id,
        "the reply joins the parent's conversation"
    );
    assert_eq!(row.raw_text, "a reply", "the text is not modified");
    assert_eq!(
        value["content"], "<p>a reply</p>",
        "and no mention span was inserted"
    );
    assert_eq!(
        mention_links(&pool, row.id).await,
        vec![bob.id],
        "the parent's author is auto-mentioned"
    );
    assert_eq!(
        parent_post_count(&pool, parent.id).await,
        1,
        "the parent counts the reply"
    );

    // A second-level reply by another user still joins the root's
    // conversation, answers its own parent, and auto-mentions that parent's
    // author — here alice, whose reply is the middle row.
    let deeper = posted(
        &app,
        &bob_token,
        json!({
            "status": "deeper",
            "in_reply_to_id": row.id.to_string(),
        }),
    )
    .await;
    let deeper_row = created_row(&pool, &deeper).await;
    assert_eq!(
        deeper_row.conversation_id, parent.conversation_id,
        "three levels deep is still one conversation"
    );
    assert_eq!(deeper_row.in_reply_to_id, Some(row.id));
    assert_eq!(deeper_row.in_reply_to_account_id, Some(alice.id));
    assert_eq!(
        mention_links(&pool, deeper_row.id).await,
        vec![alice.id],
        "the middle reply's author is auto-mentioned to the deeper one"
    );
}

/// The stored `replies_count` of one status, read off the row.
async fn parent_post_count(pool: &PgPool, id: Uuid) -> i32 {
    status::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the parent is stored live")
        .replies_count
}

/// A reply to a reblog row answers the boosted original: the ids, the
/// thread, the auto-mention and the counted reply all land on the original.
#[tokio::test]
async fn a_reply_to_a_reblog_answers_the_original() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let app = common::test_app(pool.clone());

    let original = parent_post(&pool, &bob, "the original", Visibility::Public).await;
    let boost = reblog_of(&pool, &carol, &original).await;

    let value = posted(
        &app,
        &token,
        json!({"status": "reacting", "in_reply_to_id": boost.id.to_string()}),
    )
    .await;
    let row = created_row(&pool, &value).await;

    assert_eq!(
        value["in_reply_to_id"],
        original.id.to_string(),
        "the entity answers the original, not the boost row"
    );
    assert_eq!(row.in_reply_to_id, Some(original.id));
    assert_eq!(row.in_reply_to_account_id, Some(bob.id));
    assert_eq!(row.conversation_id, original.conversation_id);
    assert_eq!(mention_links(&pool, row.id).await, vec![bob.id]);
    assert_eq!(parent_post_count(&pool, original.id).await, 1);
    assert_eq!(
        parent_post_count(&pool, boost.id).await,
        0,
        "the boost row counts no replies"
    );
}

/// Every parent the reply cannot answer — a malformed id, an unknown id, a
/// deleted post, and a `private` post the poster may not see — is the one
/// shared 404, and nothing is written.
#[tokio::test]
async fn an_unanswerable_reply_parent_is_the_shared_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let gone = parent_post(&pool, &bob, "doomed", Visibility::Public).await;
    let hidden = parent_post(&pool, &bob, "hush", Visibility::Private).await;
    deleted(&pool, &gone).await;
    let before = stored_count(&pool).await;

    let bad_id = publish(
        &app,
        Some(&token),
        json!({"status": "hi", "in_reply_to_id": "not-an-id"}),
    )
    .await;
    common::assert_error(&bad_id, NOT_FOUND, "a malformed parent id");
    let unknown = publish(
        &app,
        Some(&token),
        json!({"status": "hi", "in_reply_to_id": UNKNOWN_ID}),
    )
    .await;
    common::assert_error(&unknown, NOT_FOUND, "a parent that names nothing");
    let tombstone = publish(
        &app,
        Some(&token),
        json!({"status": "hi", "in_reply_to_id": gone.id.to_string()}),
    )
    .await;
    common::assert_error(&tombstone, NOT_FOUND, "a deleted parent");
    let invisible = publish(
        &app,
        Some(&token),
        json!({"status": "hi", "in_reply_to_id": hidden.id.to_string()}),
    )
    .await;
    common::assert_error(&invisible, NOT_FOUND, "a parent the poster may not see");

    assert_eq!(
        stored_count(&pool).await,
        before,
        "not one refused reply wrote a row"
    );
    assert_eq!(
        stored(&pool, gone.id).await.replies_count,
        0,
        "and no parent gained a count"
    );
}

/// Replying narrows the audience the way the visibility inheritance rules
/// say: a requested `public` comes down under a `private` parent a follower
/// may see, and the language falls back to the parent's tag.
#[tokio::test]
async fn a_reply_inherits_its_parent_s_visibility_bound_and_language() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());
    followed(&pool, &alice, &bob).await;

    let tagged = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Private,
            language: Some("eo".to_owned()),
            ..NewStatus::local(&bob, "hush and saluton")
        },
    )
    .await
    .expect("a fixture parent inserts");

    let value = posted(
        &app,
        &token,
        json!({
            "status": "a public ask",
            "visibility": "public",
            "in_reply_to_id": tagged.id.to_string(),
        }),
    )
    .await;
    let row = created_row(&pool, &value).await;
    assert_eq!(
        value["visibility"], "private",
        "a requested public under a private parent comes down"
    );
    assert_eq!(row.visibility, Visibility::Private);
    assert_eq!(
        value["language"], "eo",
        "an absent language falls back to the parent's tag"
    );
    assert_eq!(row.language.as_deref(), Some("eo"));
}

// ───────────────────────  POST /api/v1/statuses: media  ──────────────────────

/// 422 body for more submitted `media_ids` than
/// `Config::max_media_attachments` (Pleroma's `validate_attachments_count`).
const TOO_MANY_ATTACHMENTS: &str = "Too many attachments";
/// 422 body for a `media_ids` entry that names no media the poster may
/// claim: an unknown id, another account's upload, a row a status already
/// holds and id text that is no id are all this one refusal.
const MEDIA_NOT_FOUND: &str = "Media not found";

/// An unposted upload by `author` — the row a `media_ids` entry names.
async fn upload(pool: &PgPool, author: &User) -> Uuid {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            url: format!("{TEST_BASE_URL}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("a fixture upload inserts")
    .id
}

/// The stored media row, for the claims and releases a test inspects.
async fn media_row(pool: &PgPool, id: Uuid) -> media_attachment::MediaAttachment {
    media_attachment::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the fixture media is stored")
}

/// The `media_attachments` ids of a creation answer, in answer order.
fn attachment_ids(value: &Value) -> Vec<String> {
    value["media_attachments"]
        .as_array()
        .expect("media_attachments is an array")
        .iter()
        .map(|attachment| {
            attachment["id"]
                .as_str()
                .expect("an attachment id")
                .to_owned()
        })
        .collect()
}

/// A post carrying one media attachment is created, answered as the
/// `Status` entity with the attachment rendered in `media_attachments`
/// (`id`, `type`, `url`), and claims the row for the status at position
/// zero — the poster's own upload, attached.
#[tokio::test]
async fn a_post_with_one_media_attachment_answers_and_claims_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    let picture = upload(&pool, &alice).await;

    let value = posted(
        &app,
        &token,
        json!({
            "status": "look at this",
            "media_ids": [picture.to_string()],
        }),
    )
    .await;
    let row = created_row(&pool, &value).await;

    assert_eq!(attachment_ids(&value), vec![picture.to_string()]);
    let media = value["media_attachments"].as_array().expect("an array")[0].clone();
    assert_eq!(media["id"], picture.to_string(), "the entity names the row");
    assert_eq!(media["type"], "image", "derived from the stored MIME type");
    assert_eq!(
        media["url"],
        format!("{TEST_BASE_URL}/media/fixture.png"),
        "served at the upload's URL"
    );
    assert_eq!(media["pleroma"]["mime_type"], "image/png");

    let stored = media_row(&pool, picture).await;
    assert_eq!(
        stored.status_id,
        Some(row.id),
        "the upload is now the status's"
    );
    assert_eq!(stored.position, 0, "first in a one-attachment list");
}

/// Multiple attachments render — and are stored by `position` — in the
/// order the body asked for, not the order the uploads were minted in.
#[tokio::test]
async fn media_keeps_the_requested_order_across_the_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    let first = upload(&pool, &alice).await;
    let second = upload(&pool, &alice).await;
    let third = upload(&pool, &alice).await;

    let value = posted(
        &app,
        &token,
        json!({
            "status": "a whole gallery",
            "media_ids": [third.to_string(), first.to_string(), second.to_string()],
        }),
    )
    .await;
    let row = created_row(&pool, &value).await;

    assert_eq!(
        attachment_ids(&value),
        vec![third.to_string(), first.to_string(), second.to_string(),],
        "the entity answers the order the client asked for"
    );
    for (position, id) in [(0, third), (1, first), (2, second)] {
        let stored = media_row(&pool, id).await;
        assert_eq!(stored.status_id, Some(row.id), "all three were claimed");
        assert_eq!(stored.position, position, "each at its list position");
    }
}

/// `media_ids[]` form keys (decision 10) arrive as the JSON array does:
/// the same list, in the same order, claimed the same way.
#[tokio::test]
async fn media_ids_form_keys_arrive_like_the_json_array() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    let first = upload(&pool, &alice).await;
    let second = upload(&pool, &alice).await;

    let response = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[
            ("status", "from a form"),
            ("media_ids[]", first.to_string().as_str()),
            ("media_ids[]", second.to_string().as_str()),
        ])
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "a form post with media");
    let value = response.json_value();

    assert_eq!(
        attachment_ids(&value),
        vec![first.to_string(), second.to_string()],
        "the bracketed keys keep their order"
    );
    assert_eq!(media_row(&pool, first).await.position, 0);
    assert_eq!(media_row(&pool, second).await.position, 1);
}

/// More ids than `MAX_MEDIA_ATTACHMENTS` (4 by default) is 422 `Too many
/// attachments` — counted on the submission as sent, before anything about
/// the ids is looked up, and nothing is written.
#[tokio::test]
async fn too_many_attachments_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    let mut ids: Vec<String> = Vec::new();
    for _ in 0..5 {
        ids.push(upload(&pool, &alice).await.to_string());
    }

    let response = publish(
        &app,
        Some(&token),
        json!({"status": "greedy", "media_ids": ids}),
    )
    .await;
    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a body over the attachment limit",
    );
    common::assert_error(&response, TOO_MANY_ATTACHMENTS, "the refusal wording");
    assert_eq!(stored_count(&pool).await, 0, "and nothing was written");
    for id in &ids {
        let stored = media_row(&pool, Uuid::parse_str(id).expect("an id")).await;
        assert!(stored.status_id.is_none(), "and no media was claimed");
    }
}

/// Every media id the poster may not claim — another account's upload, a
/// row a status already holds, an id no row answers and id text that is no
/// id at all — is one 422 `Media not found`, and none of a refused body's
/// attachable ids get taken along with it.
#[tokio::test]
async fn media_the_poster_cannot_claim_is_media_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());
    let foreign = upload(&pool, &bob).await;
    let stands =
        common::create_status(&pool, &alice, "an old post stands", Visibility::Public).await;
    let used = attach(&pool, &alice, stands.id).await;
    let unknown = Uuid::now_v7();

    for (what, id) in [
        ("somebody else's upload", foreign.to_string()),
        ("media a status already claims", used.to_string()),
        ("an id no row holds", unknown.to_string()),
        ("id text that is no id", "not-an-id".to_owned()),
    ] {
        let mine = upload(&pool, &alice).await;
        let response = publish(
            &app,
            Some(&token),
            json!({
                "status": "may I borrow it",
                "media_ids": [mine.to_string(), id],
            }),
        )
        .await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("the refusal for {what}"),
        );
        common::assert_error(
            &response,
            MEDIA_NOT_FOUND,
            &format!("the wording for {what}"),
        );
        assert_eq!(stored_count(&pool).await, 1, "{what}: no second post");
        assert!(
            media_row(&pool, mine).await.status_id.is_none(),
            "{what}: the claimable id rode the refusal out with the rest"
        );
    }

    assert!(
        media_row(&pool, foreign).await.status_id.is_none(),
        "the probes never attached the foreign row"
    );
    assert!(
        media_row(&pool, used).await.status_id.is_some(),
        "and the already-claimed row stayed where it was"
    );
}

/// A body carrying at least one attachment may leave its text empty — the
/// media-only post Pleroma admits — while an empty attachment list is no
/// attachment and the old refusal stands.
#[tokio::test]
async fn a_media_only_post_needs_no_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    let picture = upload(&pool, &alice).await;

    let value = posted(
        &app,
        &token,
        json!({"status": "   ", "media_ids": [picture.to_string()]}),
    )
    .await;
    let row = created_row(&pool, &value).await;
    assert_eq!(row.raw_text, "", "the post says nothing and stands");
    assert_eq!(value["content"], "", "the empty text renders empty");
    assert_eq!(
        attachment_ids(&value),
        vec![picture.to_string()],
        "and what it has to say is the picture"
    );

    let no_media = publish(&app, Some(&token), json!({"status": "", "media_ids": []})).await;
    common::assert_answer(
        &no_media,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an empty attachment list is no attachment",
    );
    common::assert_error(
        &no_media,
        EMPTY_STATUS,
        "the plain empty-status refusal stands",
    );
}

/// Attachments ride every visibility, Pleroma's `direct` included: media
/// is no reason a private audience cannot post.
#[tokio::test]
async fn every_visibility_can_post_with_media() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for scope in ["public", "unlisted", "private", "direct", "local"] {
        let picture = upload(&pool, &alice).await;
        let value = posted(
            &app,
            &token,
            json!({
                "status": format!("said under {scope}"),
                "visibility": scope,
                "media_ids": [picture.to_string()],
            }),
        )
        .await;
        assert_eq!(value["visibility"], scope, "`{scope}` stands as asked");
        assert_eq!(
            attachment_ids(&value),
            vec![picture.to_string()],
            "`{scope}` carries its attachment"
        );
        assert!(
            media_row(&pool, picture).await.status_id.is_some(),
            "`{scope}` claimed the row"
        );
    }
}

/// The `sensitive` flag a media post sends is the status's own flag and
/// nothing the attachments rewrite: the entity answers it as submitted,
/// whatever the media.
#[tokio::test]
async fn a_media_post_keeps_the_submitted_sensitive_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    let picture = upload(&pool, &alice).await;

    let value = posted(
        &app,
        &token,
        json!({
            "status": "flagged as asked",
            "sensitive": true,
            "media_ids": [picture.to_string()],
        }),
    )
    .await;
    assert_eq!(value["sensitive"], true, "the submitted flag");
    assert!(
        created_row(&pool, &value).await.sensitive,
        "stored as submitted"
    );

    let plain = upload(&pool, &alice).await;
    let off = posted(
        &app,
        &token,
        json!({"status": "unflagged", "sensitive": false, "media_ids": [plain.to_string()]}),
    )
    .await;
    assert_eq!(off["sensitive"], false, "and a submitted false too");
}
