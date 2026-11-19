//! `DELETE /api/v1/statuses/{id}`: the author's delete and its counters,
//! reblogs taken down with the original, staff deletes, the ownership
//! refusal, the shared gate, its `write:statuses` demand, and the one
//! `statuses_actions` bucket it spends together with the favourite writes.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        media_attachment,
        status::{self, NewStatus, Visibility},
        user::{Flag, NewUser},
    },
};

use super::support::{
    INVALID_TOKEN, NO_TOKEN, NO_WRITE, NOT_FOUND, SCOPES, UNKNOWN_ID, USER_REQUIRED, WRITE, attach,
    deleted, destroy, favourite, followed, reblog_of, show, staff_with_token, statuses_count,
    stored, user_with_token, writer,
};
use crate::common;

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
