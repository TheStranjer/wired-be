//! WBE-85's `in_reply_to_id`: the reply answers its parent's ids, joins its
//! conversation, auto-mentions the parent's author and steps the parent's
//! `replies_count`; a reblog row folds to the original it boosts; every
//! unanswerable parent is the shared 404; and the audience and language the
//! reply inherits stay inside the parent's bounds.

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        status::{self, NewStatus, Status, Visibility},
        user::User,
    },
};

use super::support::{
    NOT_FOUND, UNKNOWN_ID, WRITE, created_row, deleted, followed, mention_links, posted, publish,
    reblog_of, stored, stored_count, user_with_token,
};
use crate::common;

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
