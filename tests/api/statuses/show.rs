//! `GET /api/v1/statuses/{id}`: the show route answers the `Status` entity
//! to everyone the visibility model admits and the shared 404 to everyone
//! else — public, private, direct and deleted posts, bad and unknown ids,
//! URI ids that are no lookup without a viewer, and a reblog answering its
//! own entity with the original nested.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowState},
        status::Visibility,
        user::{Flag, NewUser},
    },
};

use super::support::{
    NOT_FOUND, SCOPES, UNKNOWN_ID, batch, deleted, direct_to, followed, id_query, ids_of,
    reblog_of, show, user_with_token,
};
use crate::common;

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
        common::ids_of(&after.json_value()),
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

/// A URI in the `{id}` segment is only a lookup once the request asks for
/// resolution and brings a viewer to own the fetch: the flagless read, the
/// anonymous `resolve=true` read and the flagged read of a non-URI id all
/// stay the module's one 404 — no federation runs for a probe that cannot
/// own its result. (The success round trip belongs to
/// `e2e_federation_discovery.rs`.)
#[tokio::test]
async fn a_uri_id_needs_resolve_and_a_viewer_to_be_a_lookup() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let app = common::test_app(pool);

    let uri = format!("{TEST_BASE_URL}/objects/e2f-none");
    let encoded = uri.replace(':', "%3A").replace('/', "%2F");

    let unflagged = show(&app, Some(alice_token.as_str()), &encoded).await;
    common::assert_answer(
        &unflagged,
        StatusCode::NOT_FOUND,
        "the URI id without resolve",
    );

    let anonymous = common::TestRequest::get(&format!("/api/v1/statuses/{encoded}?resolve=true"))
        .send(&app)
        .await;
    common::assert_answer(
        &anonymous,
        StatusCode::NOT_FOUND,
        "the anonymous resolving read",
    );

    let not_a_uri = show(&app, Some(alice_token.as_str()), "urn:uuid:not-a-uri").await;
    common::assert_all_refused(
        &[
            not_a_uri,
            common::TestRequest::get("/api/v1/statuses/not-an-id?resolve=true")
                .bearer(&alice_token)
                .send(&app)
                .await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "a flagged read of an id that is no URI",
    );
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

/// A deactivated author's post, and a boost of it, answer the shared 404 to
/// a stranger and the anonymous caller alike (the author's own token is
/// refused at the gate); reactivating the account brings the post back.
#[tokio::test]
async fn a_deactivated_authors_status_is_the_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let app = common::test_app(pool.clone());
    let post = common::create_status(&pool, &alice, "going dark", Visibility::Public).await;
    let boost = reblog_of(&pool, &bob, &post).await;
    let id = post.id.to_string();
    let boost_id = boost.id.to_string();

    common::assert_answer(
        &show(&app, None, &id).await,
        StatusCode::OK,
        "an active author's post",
    );

    set_active(&pool, alice.id, false).await;
    common::assert_answer(
        &show(&app, Some(alice_token.as_str()), &id).await,
        StatusCode::UNAUTHORIZED,
        "the deactivated author's own token no longer authenticates",
    );
    for (who, token) in [
        ("a stranger", Some(bob_token.as_str())),
        ("an anonymous caller", None),
    ] {
        common::assert_all_refused(
            &[
                show(&app, token, &id).await,
                show(&app, token, &boost_id).await,
            ],
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            who,
        );
    }

    set_active(&pool, alice.id, true).await;
    common::assert_answer(
        &show(&app, None, &id).await,
        StatusCode::OK,
        "the reactivated author's post",
    );
}

/// Sets `users.is_active` of the account `id`.
async fn set_active(pool: &sqlx::PgPool, id: uuid::Uuid, active: bool) {
    let _updated: sqlx::postgres::PgQueryResult =
        sqlx::query("UPDATE users SET is_active = $2 WHERE id = $1")
            .bind(id)
            .bind(active)
            .execute(pool)
            .await
            .expect("the flag updates");
}
