//! WBE-123's DM tracking through the endpoints: a `direct` post opens its
//! thread through the create hook with the author read and the recipient
//! unread, `in_reply_to_conversation_id` posts back into that thread with
//! its recipients re-mentioned under a forced `direct`, an unusable
//! conversation id is the shared 404, and deletes repoint the
//! participations to the newest DM left or withdraw them.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{conversation, status::Visibility, user::User};

use super::support::{
    NO_TOKEN, NOT_FOUND, UNKNOWN_ID, WRITE, WRITE_AND_READ, created_row, destroy, mention_links,
    posted, publish, show, stored_count, user_with_token,
};
use crate::common;

// ---------------------------------------------------------------------------
// WBE-123: conversation participation on direct statuses.
// ---------------------------------------------------------------------------

/// The `id` of `who`'s participation in `thread`, for an assertion.
async fn participation_id(pool: &PgPool, who: &User, thread: Uuid) -> Uuid {
    *conversation::participation_ids(pool, who.id, &[thread])
        .await
        .expect("the participation batch reads")
        .get(&thread)
        .expect("the fixture account participates in the thread")
}

/// `who`'s stored participation row in `thread`.
async fn participation_row(pool: &PgPool, who: &User, thread: Uuid) -> conversation::Participation {
    let id = participation_id(pool, who, thread).await;
    conversation::get_participation(pool, who.id, id)
        .await
        .expect("the participation reads")
        .expect("the fixture participation exists")
}

/// A `direct` post opens its DM thread through the create hook: the author
/// and the mentioned local account both participate, the author read and
/// the recipient unread, and every later read of the status shows the
/// viewer their own participation id in `pleroma.direct_conversation_id`
/// — the create response itself renders before the hook lands and shows
/// `null`. A public post threads nothing and renders the key `null`.
#[tokio::test]
async fn a_direct_post_opens_its_thread_for_both_sides() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user_with_token(&pool, "alice", WRITE_AND_READ).await;
    let (bob, bob_token) = user_with_token(&pool, "bob", WRITE_AND_READ).await;
    let app = common::test_app(pool.clone());

    let value = posted(
        &app,
        &alice_token,
        json!({"status": "@bob psst", "visibility": "direct"}),
    )
    .await;
    let row = created_row(&pool, &value).await;
    assert_eq!(row.visibility, Visibility::Direct, "posted as a DM");
    let thread = row
        .direct_conversation_id
        .expect("the create hook linked the thread");

    assert_eq!(
        value["pleroma"]["direct_conversation_id"],
        Value::Null,
        "the participation did not exist when the create rendered"
    );

    let shown = show(&app, Some(&alice_token), &row.id.to_string()).await;
    assert_eq!(
        shown.json_value()["pleroma"]["direct_conversation_id"],
        participation_id(&pool, &alice, thread).await.to_string(),
        "the author sees her own participation id"
    );
    let for_bob = show(&app, Some(&bob_token), &row.id.to_string()).await;
    assert_eq!(
        for_bob.json_value()["pleroma"]["direct_conversation_id"],
        participation_id(&pool, &bob, thread).await.to_string(),
        "and the recipient his — one read per viewer, from the batch"
    );

    let author = participation_row(&pool, &alice, thread).await;
    assert!(author.read, "the author's own post is read");
    assert_eq!(author.last_status_id, Some(row.id));
    let recipient = participation_row(&pool, &bob, thread).await;
    assert!(!recipient.read, "the recipient lands unread");
    assert_eq!(recipient.last_status_id, Some(row.id));
    assert_eq!(
        conversation::unread_count(&pool, bob.id).await.unwrap(),
        1,
        "the badge counts the one unread thread"
    );

    let public = posted(&app, &alice_token, json!({"status": "hello"})).await;
    assert_eq!(
        public["pleroma"]["direct_conversation_id"],
        Value::Null,
        "a public post is nobody's thread"
    );
    assert_eq!(
        created_row(&pool, &public).await.direct_conversation_id,
        None,
        "and stores no thread either"
    );
}

/// `in_reply_to_conversation_id` posts into the named DM thread: the
/// participation's recipients join the mentions, the visibility is forced
/// to `direct` over the submitted `public`, and the row lands in the same
/// conversation — the recipients' participations bump and land unread, the
/// poster's lands read.
#[tokio::test]
async fn replying_to_a_conversation_forces_direct_and_addresses_the_recipients() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user_with_token(&pool, "alice", WRITE_AND_READ).await;
    let (bob, bob_token) = user_with_token(&pool, "bob", WRITE_AND_READ).await;
    let app = common::test_app(pool.clone());

    let first = posted(
        &app,
        &alice_token,
        json!({"status": "@bob one word", "visibility": "direct"}),
    )
    .await;
    let first_row = created_row(&pool, &first).await;
    let thread = first_row
        .direct_conversation_id
        .expect("the opening DM has its thread");
    let bobs_id = participation_id(&pool, &bob, thread).await;
    let alices_id = participation_id(&pool, &alice, thread).await;

    // bob answers the conversation, naming nobody in the text and asking
    // for a public audience the DM refuses.
    let answer = posted(
        &app,
        &bob_token,
        json!({
            "status": "answering the conversation",
            "visibility": "public",
            "in_reply_to_conversation_id": bobs_id.to_string(),
        }),
    )
    .await;
    assert_eq!(
        answer["visibility"], "direct",
        "the conversation forces the audience"
    );
    assert_eq!(
        answer["in_reply_to_id"],
        Value::Null,
        "a conversation answer joins the thread, not the last post"
    );
    let row = created_row(&pool, &answer).await;
    assert_eq!(
        row.direct_conversation_id,
        Some(thread),
        "the post lands in the named thread"
    );
    assert_eq!(
        mention_links(&pool, row.id).await,
        vec![alice.id],
        "the participation's recipients are the mentions"
    );

    let alice_now = conversation::get_participation(&pool, alice.id, alices_id)
        .await
        .expect("alice's participation reads")
        .expect("alice still participates");
    assert!(!alice_now.read, "bob's answer leaves alice unread");
    assert_eq!(
        alice_now.last_status_id,
        Some(row.id),
        "and moves her pointer to it"
    );
    let bob_now = participation_row(&pool, &bob, thread).await;
    assert!(bob_now.read, "bob authored this one");
    assert_eq!(
        conversation::unread_count(&pool, alice.id).await.unwrap(),
        1,
        "one unread thread for alice"
    );
    assert_eq!(
        conversation::recipients(&pool, bob_now.id)
            .await
            .expect("the recipient list reads"),
        vec![alice.id],
        "and bob's list keeps addressing alice"
    );
}

/// Every conversation id the poster cannot use — malformed text, an id
/// that names nothing, and a participation of somebody else's — is the
/// shared 404 before anything renders, and the auth gate still answers a
/// tokenless request first.
#[tokio::test]
async fn an_unusable_conversation_id_is_the_shared_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, alice_token) = user_with_token(&pool, "alice", WRITE).await;
    let (bob, bob_token) = user_with_token(&pool, "bob", WRITE).await;
    let app = common::test_app(pool.clone());

    let first = posted(
        &app,
        &alice_token,
        json!({"status": "@bob psst", "visibility": "direct"}),
    )
    .await;
    let thread = created_row(&pool, &first)
        .await
        .direct_conversation_id
        .expect("the opening DM has its thread");
    let bobs_id = participation_id(&pool, &bob, thread).await;
    let before = stored_count(&pool).await;

    let malformed = publish(
        &app,
        Some(&alice_token),
        json!({"status": "hi", "in_reply_to_conversation_id": "not-an-id"}),
    )
    .await;
    common::assert_error(&malformed, NOT_FOUND, "a malformed conversation id");
    let unknown = publish(
        &app,
        Some(&alice_token),
        json!({"status": "hi", "in_reply_to_conversation_id": UNKNOWN_ID}),
    )
    .await;
    common::assert_error(&unknown, NOT_FOUND, "a conversation id that names nothing");
    let foreign = publish(
        &app,
        Some(&alice_token),
        json!({"status": "hi", "in_reply_to_conversation_id": bobs_id.to_string()}),
    )
    .await;
    common::assert_error(&foreign, NOT_FOUND, "a participation somebody else owns");
    assert_eq!(
        stored_count(&pool).await,
        before,
        "not one refused post wrote a row"
    );

    let anonymous = publish(
        &app,
        None,
        json!({"status": "hi", "in_reply_to_conversation_id": bobs_id.to_string()}),
    )
    .await;
    common::assert_error(&anonymous, NO_TOKEN, "the auth gate runs before the lookup");
    let unscoped = publish(
        &app,
        Some(&bob_token),
        json!({"status": "hi", "in_reply_to_conversation_id": bobs_id.to_string()}),
    )
    .await;
    common::assert_answer(
        &unscoped,
        StatusCode::OK,
        "but bob may answer his own thread",
    );
}

/// Deleting through the endpoint repoints the conversation: the newest DM
/// goes, and every participation that named it moves to the newest status
/// left; when the last one goes, the participations go with it — the read
/// flags of a thread nobody can list any more are nobody's unread badge
/// either.
#[tokio::test]
async fn deleting_direct_statuses_repoints_then_withdraws_the_participations() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user_with_token(&pool, "alice", WRITE_AND_READ).await;
    let (bob, bob_token) = user_with_token(&pool, "bob", WRITE_AND_READ).await;
    let app = common::test_app(pool.clone());

    let first = posted(
        &app,
        &alice_token,
        json!({"status": "@bob psst", "visibility": "direct"}),
    )
    .await;
    let first_row = created_row(&pool, &first).await;
    let thread = first_row
        .direct_conversation_id
        .expect("the opening DM has its thread");
    let second = posted(
        &app,
        &bob_token,
        json!({
            "status": "back",
            "in_reply_to_conversation_id": participation_id(&pool, &bob, thread).await.to_string(),
        }),
    )
    .await;
    let second_row = created_row(&pool, &second).await;
    assert_eq!(
        second_row.direct_conversation_id,
        Some(thread),
        "the answer joined the same thread"
    );

    let gone = destroy(&app, &bob_token, &second_row.id.to_string()).await;
    common::assert_answer(&gone, StatusCode::OK, "bob deletes his own answer");
    for who in [&alice, &bob] {
        let row = participation_row(&pool, who, thread).await;
        assert_eq!(
            row.last_status_id,
            Some(first_row.id),
            "the pointer moved back to the newest status left"
        );
    }

    // Capture the participation ids before the deletes withdraw them.
    let alices_id = participation_id(&pool, &alice, thread).await;
    let withdrawn = destroy(&app, &alice_token, &first_row.id.to_string()).await;
    common::assert_answer(&withdrawn, StatusCode::OK, "alice deletes her opening DM");
    assert!(
        conversation::get_participation(&pool, alice.id, alices_id)
            .await
            .expect("the read runs")
            .is_none(),
        "with no live DM left, alice's participation is gone"
    );
    assert_eq!(
        conversation::unread_count(&pool, alice.id).await.unwrap(),
        0,
        "and her badge has nothing to count"
    );
}
