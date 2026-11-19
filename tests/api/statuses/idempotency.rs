//! WBE-87's `Idempotency-Key` header on the create: the same key twice
//! answers the first status and writes nothing further, different keys post
//! separately, a key belongs to its sender alone, an over-long key text is
//! ignored, and a deleted status frees its claim for the next post.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::{
    config::TEST_BASE_URL,
    util::entities::{hashtag_html, mention_html},
};

use super::support::{
    WRITE, author_now, created_row, destroy, id_of, key_rows, mention_links, publish_keyed,
    stored_count, tag_links, user_with_token,
};
use crate::common;

// ─────────────  POST /api/v1/statuses: Idempotency-Key  ─────────────

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
