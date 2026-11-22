//! The admin message delete: hiding, scoping and the federated withdrawal.

use super::*;

// --- DELETE /chats/{id}/messages/{message_id} ----------------------------------------------------

/// The moderation withdrawal is FOR EVERYONE: deleting through alice's
/// chat and reference takes the message out of bob's listing too — what
/// the client route's self-service delete cannot do when the caller is not
/// the writer (there, each side merely drops its own copy). The answer is
/// the entity as the chat's owner saw it BEFORE the tombstone, and a second
/// strike — through either side's reference — finds nothing left to delete.
#[tokio::test]
async fn the_admin_delete_hides_the_message_from_both_participants() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let alice_token = common::create_app_and_token(&pool, &alice, "read:chats").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let bob_token = common::create_app_and_token(&pool, &bob, "read:chats").await;
    let (message, alice_ref, bob_ref) = send(&pool, &alice, &bob, "the plan").await;

    let deleted = delete(
        &app,
        &token,
        &message_path(&alice_ref.chat_id.to_string(), &alice_ref.id.to_string()),
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "withdrawing through alice's chat");
    let entity = deleted.json_value();
    assert_eq!(json_keys(&entity), MESSAGE_KEYS, "the ordinary entity");
    assert_eq!(
        entity["id"],
        alice_ref.id.to_string(),
        "the reference the route named"
    );
    assert_eq!(
        entity["content"], message.content,
        "rendered BEFORE the delete: the text is what the admin just took away"
    );
    assert_eq!(entity["account_id"], alice.id.to_string(), "alice wrote it");

    let alices_list = get(&app, &alice_token, &own_messages_path(&alice_ref.chat_id)).await;
    common::assert_answer(
        &alices_list,
        StatusCode::OK,
        "alice still lists her own chat",
    );
    assert_eq!(
        alices_list.json_value(),
        json!([]),
        "and the message is gone from it"
    );
    let bobs_list = get(&app, &bob_token, &own_messages_path(&bob_ref.chat_id)).await;
    assert_eq!(
        bobs_list.json_value(),
        json!([]),
        "bob's own listing lost it too, though the delete named alice's chat alone"
    );

    assert_tombstoned(&pool, message.id).await;
    assert_chat_delete_logged(&pool, alice_ref.id).await;

    let repeat = delete(
        &app,
        &token,
        &message_path(&alice_ref.chat_id.to_string(), &alice_ref.id.to_string()),
    )
    .await;
    common::assert_answer(
        &repeat,
        StatusCode::NOT_FOUND,
        "a second strike at the same reference",
    );
    let via_bob = delete(
        &app,
        &token,
        &message_path(&bob_ref.chat_id.to_string(), &bob_ref.id.to_string()),
    )
    .await;
    common::assert_answer(
        &via_bob,
        StatusCode::NOT_FOUND,
        "or the same message through bob's chat",
    );
    assert_eq!(
        error_message(&via_bob).as_deref(),
        Some(RECORD_NOT_FOUND),
        "the withdrawn message leaves nothing to delete either way"
    );

    assert!(
        publish_jobs(&pool).await.is_empty(),
        "a local↔local pair announces nothing to the network"
    );
}

/// The stored message row is a tombstone: text cleared, `deleted_at` set,
/// the row itself kept (its references survive and read through the
/// listing's live filter).
async fn assert_tombstoned(pool: &PgPool, message_id: Uuid) {
    let row: (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT content, deleted_at FROM chat_messages WHERE id = $1")
            .bind(message_id)
            .fetch_one(pool)
            .await
            .expect("the row reads");
    assert_eq!(row.0, "", "the text is cleared");
    assert!(row.1.is_some(), "and the row carries the tombstone stamp");
}

/// One `chat_message_delete` entry stands in the moderation log for the
/// withdrawal of reference `reference_id`: Pleroma's sentence, the id in
/// `subject_id`, the acting admin as `actor`, and no user subjects.
async fn assert_chat_delete_logged(pool: &PgPool, reference_id: Uuid) {
    let entry: (String, Value, Vec<Uuid>) =
        sqlx::query_as("SELECT message, data, subject_ids FROM moderation_log WHERE action = $1")
            .bind("chat_message_delete")
            .fetch_one(pool)
            .await
            .expect("the log entry stands");
    assert_eq!(
        entry.0,
        format!("@root deleted chat message ##{reference_id}"),
        "Pleroma's chat_message_delete sentence, keyed by the reference id"
    );
    assert_eq!(
        entry.1["subject_id"],
        reference_id.to_string(),
        "the subject rides in the data map"
    );
    assert_eq!(
        entry.1["actor"]["nickname"], "root",
        "and the actor is the admin who struck"
    );
    assert!(entry.2.is_empty(), "a chat message is no user subject");
}

/// The delete's participant check is the CHAT, not the caller: a malformed
/// or unknown `message_id`, and a reference that lives in another chat, are
/// one 404 each — and none of them withdraws anything.
#[tokio::test]
async fn the_delete_only_names_references_of_the_named_chat() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let (message, alice_ref, _) = send(&pool, &alice, &bob, "to bob").await;
    let (_, _, carol_ref) = send(&pool, &alice, &carol, "to carol").await;

    let refusals = [
        (
            alice_ref.chat_id.to_string(),
            "not-a-uuid".to_owned(),
            "a malformed reference id",
        ),
        (
            alice_ref.chat_id.to_string(),
            Uuid::now_v7().to_string(),
            "an unknown reference id",
        ),
        (
            alice_ref.chat_id.to_string(),
            carol_ref.id.to_string(),
            "a reference of another chat",
        ),
        (
            carol_ref.id.to_string(),
            alice_ref.id.to_string(),
            "and a live reference named under the wrong chat",
        ),
        (
            Uuid::now_v7().to_string(),
            alice_ref.id.to_string(),
            "an unknown chat holding a live reference",
        ),
    ];
    for (chat_id, message_id, who) in refusals {
        let refused = delete(&app, &token, &message_path(&chat_id, &message_id)).await;
        common::assert_answer(&refused, StatusCode::NOT_FOUND, who);
        assert_eq!(
            error_message(&refused).as_deref(),
            Some(RECORD_NOT_FOUND),
            "{who} is the one 404"
        );
    }
    let still_live =
        sqlx::query_scalar::<_, bool>("SELECT deleted_at IS NULL FROM chat_messages WHERE id = $1")
            .bind(message.id)
            .fetch_one(&pool)
            .await
            .expect("the row reads");
    assert!(still_live, "no refusal touched the message");
    assert!(
        publish_jobs(&pool).await.is_empty()
            && sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moderation_log")
                .fetch_one(&pool)
                .await
                .expect("the log reads")
                == 0,
        "and none of them logged or announced anything"
    );
}

/// The withdrawal federates ONLY the messages this instance minted, and
/// only to the partner who could hold a copy: a local-origin message of a
/// local↔remote chat queues the one `Delete` — the sender as actor and
/// signer, the remote partner alone in `to`, the stored object id embedded
/// under its `ChatMessage` type — while a remote-origin withdrawal messages
/// its own way and sends nothing anywhere.
#[tokio::test]
async fn the_delete_announces_local_withdrawals_to_the_remote_partner() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;

    let borrowed = NewChatMessage {
        ap_id: Some("https://remote.example/objects/chat-1".to_owned()),
        ..NewChatMessage::text("bob said")
    };
    let (_, alices_view, _) = send_as(&pool, &bob, &alice, &borrowed).await;
    let withdrawn = delete(
        &app,
        &token,
        &message_path(
            &alices_view.chat_id.to_string(),
            &alices_view.id.to_string(),
        ),
    )
    .await;
    common::assert_answer(&withdrawn, StatusCode::OK, "the tombstone itself lands");
    assert!(
        publish_jobs(&pool).await.is_empty(),
        "and a remote-origin withdrawal announces nothing"
    );

    let (ap_id, alice_ref) = send_local_origin(&pool, &alice, &bob, "alice said").await;
    let deleted = delete(
        &app,
        &token,
        &message_path(&alice_ref.chat_id.to_string(), &alice_ref.id.to_string()),
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "withdrawing the local message");

    let jobs = publish_jobs(&pool).await;
    assert_eq!(
        jobs.len(),
        1,
        "exactly the local origin queues one announce"
    );
    assert_eq!(
        jobs[0]["actor_id"],
        alice.id.to_string(),
        "the message's own sender signs the withdrawal"
    );
    let activity = &jobs[0]["activity"];
    assert_eq!(activity["type"], "Delete", "the withdrawal activity");
    assert_eq!(activity["actor"], alice.ap_id);
    assert_eq!(
        activity["to"],
        json!([bob.ap_id]),
        "addressed to the chat's remote partner alone"
    );
    assert_eq!(activity["object"]["id"], ap_id, "naming the stored object");
    assert_eq!(
        activity["object"]["type"], "ChatMessage",
        "under the type the receiver's dispatcher routes on"
    );
    let id = activity["id"].as_str().expect("the activity has an id");
    assert!(
        id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
        "and this instance's own activity id shape: {id}"
    );
}
