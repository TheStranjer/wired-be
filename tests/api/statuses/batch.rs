//! `GET /api/v1/statuses?id[]=…`: the batch read answers the visible
//! entities in requested order over every id spelling, ignores what it must,
//! carries the optional-auth gate the show route runs, and renders reblogs
//! exactly as the show route does.

use axum::http::StatusCode;
use serde_json::Value;
use uuid::Uuid;
use wired_be::models::status::Visibility;

use super::support::{
    INVALID_TOKEN, NO_SCOPE, NOT_FOUND, SCOPES, UNKNOWN_ID, batch, deleted, followed, id_query,
    ids_of, reblog_of, show, user_with_token,
};
use crate::common;

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
        common::ids_of(&strangers.json_value()),
        ids_of(&[&second, &first]),
        "the two public posts, in the order they were requested"
    );

    let authors = batch(&app, Some(alice_token.as_str()), &query).await;
    common::assert_answer(&authors, StatusCode::OK, "the author's batch");
    assert_eq!(
        common::ids_of(&authors.json_value()),
        ids_of(&[&second, &hidden, &first]),
        "the private post joins the page for the account that may see it"
    );

    let anonymous = batch(&app, None, &query).await;
    common::assert_answer(&anonymous, StatusCode::OK, "the anonymous batch");
    assert_eq!(
        common::ids_of(&anonymous.json_value()),
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
        assert_eq!(common::ids_of(&page.json_value()), want, "{who}");
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
        common::ids_of(&crowded_page.json_value()),
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
        common::ids_of(&twice.json_value()),
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
        common::ids_of(&app_only_page.json_value()),
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
        common::ids_of(&strangers.json_value()),
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
        common::ids_of(&followers.json_value()),
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
