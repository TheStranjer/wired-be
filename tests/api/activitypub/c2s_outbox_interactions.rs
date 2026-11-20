//! The client-to-server outbox interactions and their withdrawals: the
//! `Like` of a status and its `Undo` (by id and embedded), the `Announce`
//! whose `Location` names the reblog row itself and its `Undo`, the
//! `Block` of an actor and its `Undo` (by id and embedded), and the
//! refusals — an interaction with an unseen post is the shared 404, a
//! private one the client API's own 422.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::status::Visibility;

use super::support::{
    BASE, c2s_account, location_status_id, post_outbox, relationship, show_status,
};
use crate::common;

/// `Like` of a status through the outbox: the favourite writes, the
/// entity reads it back, a repeat answers the same activity id, and the
/// `Undo` by that id withdraws it.
#[tokio::test]
async fn the_c2s_outbox_likes_and_undoes_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = c2s_account(&pool, "c2s-out-like-a", "read write").await;
    let (bob, _) = c2s_account(&pool, "c2s-out-like-b", "read").await;
    let post = common::create_status(&pool, &bob, "likeable", Visibility::Public).await;

    let liked = post_outbox(
        &app,
        "c2s-out-like-a",
        Some(&token),
        json!({"type": "Like", "object": post.uri}),
    )
    .await;
    assert_eq!(
        liked.status,
        StatusCode::CREATED,
        "the like lands: {}",
        liked.text()
    );
    let like_id = liked.header("location").expect("a Location").to_owned();
    let shown = show_status(&app, &token, post.id).await;
    let entity = shown.json_value();
    assert_eq!(entity["favourites_count"], 1, "counted");
    assert_eq!(entity["favourited"], true, "and flagged for its viewer");

    let repeat = post_outbox(
        &app,
        "c2s-out-like-a",
        Some(&token),
        json!({"type": "Like", "object": post.uri}),
    )
    .await;
    assert_eq!(repeat.status, StatusCode::CREATED, "a repeat is restated");
    assert_eq!(
        repeat.header("location"),
        Some(like_id.as_str()),
        "naming the one Like the pair holds"
    );

    let undone = post_outbox(
        &app,
        "c2s-out-like-a",
        Some(&token),
        json!({"type": "Undo", "object": like_id}),
    )
    .await;
    assert_eq!(undone.status, StatusCode::CREATED, "the undo is accepted");
    let after_undo = show_status(&app, &token, post.id).await.json_value();
    assert_eq!(after_undo["favourites_count"], 0, "the like came off");
}

/// The like's other arms: an embedded `Undo` withdraws by the status the
/// inner activity names, and what the caller may not see is the 404.
#[tokio::test]
async fn the_c2s_outbox_undoes_an_embedded_like_and_refuses_the_invisible() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = c2s_account(&pool, "c2s-out-like-c", "read write").await;
    let (bob, bob_token) = c2s_account(&pool, "c2s-out-like-d", "read").await;
    let post = common::create_status(&pool, &bob, "likeable too", Visibility::Public).await;
    let hidden = common::create_status(&pool, &bob, "hidden away", Visibility::Private).await;

    let liked = post_outbox(
        &app,
        "c2s-out-like-c",
        Some(&token),
        json!({"type": "Like", "object": post.uri}),
    )
    .await;
    assert_eq!(liked.status, StatusCode::CREATED, "the like lands");
    let embedded_undo = post_outbox(
        &app,
        "c2s-out-like-c",
        Some(&token),
        json!({"type": "Undo", "object": {"type": "Like", "object": post.uri}}),
    )
    .await;
    assert_eq!(
        embedded_undo.status,
        StatusCode::CREATED,
        "the embedded undo lands"
    );
    let as_author = show_status(&app, &bob_token, post.id).await.json_value();
    assert_eq!(as_author["favourites_count"], 0, "and withdrew the like");

    let invisible = post_outbox(
        &app,
        "c2s-out-like-c",
        Some(&token),
        json!({"type": "Like", "object": hidden.uri}),
    )
    .await;
    common::assert_answer(
        &invisible,
        StatusCode::NOT_FOUND,
        "a like of an unseen post",
    );
}

/// `Announce` of a status through the outbox: the reblog row *is* the
/// `Announce` (decision 6), so the `Location` names it and the boost
/// renders over the original, and the `Undo` of that id takes the boost
/// back.
#[tokio::test]
async fn the_c2s_outbox_announces_and_undoes_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = c2s_account(&pool, "c2s-out-boost-a", "read write").await;
    let (bob, _) = c2s_account(&pool, "c2s-out-boost-b", "read").await;
    let post = common::create_status(&pool, &bob, "boost me", Visibility::Public).await;

    let announced = post_outbox(
        &app,
        "c2s-out-boost-a",
        Some(&token),
        json!({"type": "Announce", "object": post.uri}),
    )
    .await;
    assert_eq!(
        announced.status,
        StatusCode::CREATED,
        "the boost lands: {}",
        announced.text()
    );
    let announce_id = announced.header("location").expect("a Location").to_owned();
    assert_eq!(
        announce_id,
        format!("{BASE}/activities/{}", location_status_id(&announced)),
        "the reblog's own activity address"
    );
    let reblog = show_status(&app, &token, location_status_id(&announced)).await;
    let entity = reblog.json_value();
    assert_eq!(
        entity["uri"], announce_id,
        "the row's uri is the Announce id"
    );
    assert_eq!(
        entity["reblog"]["uri"], post.uri,
        "and it carries the original it boosts"
    );
    let original = show_status(&app, &token, post.id).await.json_value();
    assert_eq!(original["reblogs_count"], 1, "the original counts it");
    assert_eq!(original["reblogged"], true, "and flags it for the booster");

    let undone = post_outbox(
        &app,
        "c2s-out-boost-a",
        Some(&token),
        json!({"type": "Undo", "object": announce_id}),
    )
    .await;
    assert_eq!(undone.status, StatusCode::CREATED, "the undo is accepted");
    let after_undo = show_status(&app, &token, post.id).await.json_value();
    assert_eq!(after_undo["reblogs_count"], 0, "the boost came off");
    assert_eq!(
        after_undo["reblogged"], false,
        "and no longer flags it for the booster"
    );
}

/// `Block` of an actor through the outbox and its `Undo` by the activity
/// id the block answered: the relationship flips through the client API's
/// own reads, and an unseen actor is the 404.
#[tokio::test]
async fn the_c2s_outbox_blocks_and_undoes_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = c2s_account(&pool, "c2s-out-block-a", "read write").await;
    let (bob, _) = c2s_account(&pool, "c2s-out-block-b", "read").await;

    let blocked = post_outbox(
        &app,
        "c2s-out-block-a",
        Some(&token),
        json!({"type": "Block", "object": bob.ap_id}),
    )
    .await;
    assert_eq!(blocked.status, StatusCode::CREATED, "the block lands");
    let block_id = blocked.header("location").expect("a Location").to_owned();
    let rel = relationship(&app, &token, &bob).await;
    assert_eq!(
        rel["blocking"],
        json!(true),
        "the relationship says blocked"
    );

    let undone = post_outbox(
        &app,
        "c2s-out-block-a",
        Some(&token),
        json!({"type": "Undo", "object": block_id}),
    )
    .await;
    assert_eq!(undone.status, StatusCode::CREATED, "the undo is accepted");
    let lifted = relationship(&app, &token, &bob).await;
    assert_eq!(lifted["blocking"], json!(false), "and the block is lifted");

    let unknown = post_outbox(
        &app,
        "c2s-out-block-a",
        Some(&token),
        json!({"type": "Block", "object": "https://nowhere.example/users/eve"}),
    )
    .await;
    common::assert_answer(
        &unknown,
        StatusCode::NOT_FOUND,
        "a block of an unseen actor",
    );
}

/// The embedded block withdrawal: the inner `Block`'s object names the
/// target actor, and the undo lifts the pair's block.
#[tokio::test]
async fn the_c2s_outbox_undoes_an_embedded_block() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = c2s_account(&pool, "c2s-out-block-c", "read write").await;
    let (bob, _) = c2s_account(&pool, "c2s-out-block-d", "read").await;

    let reblocked = post_outbox(
        &app,
        "c2s-out-block-c",
        Some(&token),
        json!({"type": "Block", "object": bob.ap_id}),
    )
    .await;
    assert_eq!(reblocked.status, StatusCode::CREATED, "blocked");
    let undone = post_outbox(
        &app,
        "c2s-out-block-c",
        Some(&token),
        json!({"type": "Undo", "object": {"type": "Block", "object": bob.ap_id}}),
    )
    .await;
    assert_eq!(
        undone.status,
        StatusCode::CREATED,
        "the embedded undo lands"
    );
    let lifted_again = relationship(&app, &token, &bob).await;
    assert_eq!(
        lifted_again["blocking"],
        json!(false),
        "and the embedded undo lifted the block"
    );
}

/// The `Announce` refusals: what the caller may not see is the shared
/// 404, and the caller's own `private` post — visible to them and still
/// not boostable — is the client API's own 422, in its own words.
#[tokio::test]
async fn the_c2s_outbox_refuses_an_unboostable_target() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = c2s_account(&pool, "c2s-out-boost-c", "read write").await;
    let (bob, _) = c2s_account(&pool, "c2s-out-boost-d", "read").await;
    let hidden = common::create_status(&pool, &bob, "keep it quiet", Visibility::Private).await;

    let refused = post_outbox(
        &app,
        "c2s-out-boost-c",
        Some(&token),
        json!({"type": "Announce", "object": hidden.uri}),
    )
    .await;
    common::assert_answer(
        &refused,
        StatusCode::NOT_FOUND,
        "an Announce of an unseen post",
    );

    let mine = common::create_status(&pool, &alice, "my private word", Visibility::Private).await;
    let unboostable = post_outbox(
        &app,
        "c2s-out-boost-c",
        Some(&token),
        json!({"type": "Announce", "object": mine.uri}),
    )
    .await;
    common::assert_answer(
        &unboostable,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an Announce of a private post",
    );
    common::assert_error(
        &unboostable,
        "Could not reblog private status",
        "an Announce of a private post",
    );
}
