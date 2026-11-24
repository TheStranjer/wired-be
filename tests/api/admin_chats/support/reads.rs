//! The admin chat reads: a user's chats, one chat and a chat's messages.

use super::*;

// --- GET /users/{nickname}/chats ---------------------------------------------------------

/// The user chat listing answers the account's OWN chats — the admin holds
/// neither — newest activity first, full `Chat` entities whose `account`
/// names the other participant, under the `Link` header keyed on the page's
/// chat ids; `max_id` walks back through the rest.
#[tokio::test]
async fn the_admin_lists_a_users_chats_newest_first_under_link_headers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", "admin:read:chats").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let (_, alice_bob, bobs_copy) = send(&pool, &alice, &bob, "yo bob").await;
    let (_, alice_carol, _) = send(&pool, &alice, &carol, "yo carol").await;
    let (_, alice_dave, _) = send(&pool, &alice, &dave, "yo dave").await;
    backdate_chat(&pool, alice_bob.chat_id, 2).await;
    backdate_chat(&pool, alice_carol.chat_id, 1).await;

    let path = user_chats_path("alice");
    let listed = get(&app, &token, &path).await;
    common::assert_answer(&listed, StatusCode::OK, "listing alice's chats");
    let value = listed.json_value();
    assert_eq!(
        common::ids_of(&value),
        vec![
            alice_dave.chat_id.to_string(),
            alice_carol.chat_id.to_string(),
            alice_bob.chat_id.to_string(),
        ],
        "newest activity first, the backdated rows last"
    );
    for entry in value.as_array().expect("the list is an array") {
        assert_eq!(json_keys(entry), CHAT_KEYS, "every entry is a Chat entity");
    }
    assert_eq!(
        value[0]["account"]["id"],
        dave.id.to_string(),
        "the entity names the OTHER participant, from alice's seat"
    );
    assert_eq!(
        value[0]["last_message"]["id"],
        alice_dave.id.to_string(),
        "and carries her newest message"
    );

    let first = get(&app, &token, &format!("{path}?limit=2")).await;
    assert_eq!(
        first.header("link"),
        Some(
            expected_link(
                &path,
                &alice_dave.chat_id.to_string(),
                &alice_carol.chat_id.to_string()
            )
            .as_str()
        ),
        "the cursors key on the page's own chat ids"
    );
    let walked = get(
        &app,
        &token,
        &format!("{path}?limit=2&max_id={}", alice_carol.chat_id),
    )
    .await;
    assert_eq!(
        common::ids_of(&walked.json_value()),
        vec![alice_bob.chat_id.to_string()],
        "max_id visits what the first page left out"
    );

    let bobs_side = get(&app, &token, &user_chats_path("bob")).await;
    assert_eq!(
        common::ids_of(&bobs_side.json_value()),
        vec![bobs_copy.chat_id.to_string()],
        "bob's listing is HIS own row of the same conversation"
    );
    assert_eq!(
        bobs_side.json_value()[0]["account"]["id"],
        alice.id.to_string(),
        "seen from his seat, where alice is the other side"
    );
    assert_eq!(
        bobs_side.json_value()[0]["unread"],
        1,
        "and alice's message still counts unread there"
    );
}

/// A remote `nick@domain` handle is a stored nickname (decision 7), so the
/// lookup takes it verbatim; an account that owns no chats answers the empty
/// array under no `Link` header; the id spelling resolves like the show
/// route's; an unknown nickname is the admin API's 404
/// `{"error":"Not found"}`; and a malformed cursor is the pagination
/// module's 400, before anything reads.
#[tokio::test]
async fn the_user_lookup_takes_handles_and_refuses_strangers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", "admin:read:chats").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let _mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;

    let quiet = get(&app, &token, &user_chats_path("mallory@remote.example")).await;
    common::assert_answer(&quiet, StatusCode::OK, "a remote handle resolves");
    assert_eq!(quiet.json_value(), json!([]), "and owns no chat rows here");
    assert_eq!(quiet.header("link"), None, "an empty page links nowhere");

    let stranger = get(&app, &token, &user_chats_path("nobody")).await;
    common::assert_answer(&stranger, StatusCode::NOT_FOUND, "an unknown nickname");
    assert_eq!(
        error_message(&stranger).as_deref(),
        Some(NOT_FOUND),
        "the ticket's wording"
    );

    let by_id = get(&app, &token, &user_chats_path(&alice.id.to_string())).await;
    common::assert_answer(&by_id, StatusCode::OK, "the id spelling resolves too");

    let junk = get(
        &app,
        &token,
        &format!("{}?max_id=nope", user_chats_path("alice")),
    )
    .await;
    common::assert_answer(&junk, StatusCode::BAD_REQUEST, "a broken cursor");
    assert_eq!(
        error_message(&junk).as_deref(),
        Some("Invalid id"),
        "in the pagination module's wording"
    );
}

// --- GET /chats/{id} -----------------------------------------------------------------------

/// The chat show takes ANY chat by its id and renders the entity its OWNER
/// sees — the `account` is the other participant, `last_message` the
/// owner's newest live reference — which is the whole difference from the
/// client route's owned read: the admin here is neither participant. A
/// malformed id and an unknown one are one 404.
#[tokio::test]
async fn the_admin_shows_any_chat_whatever_its_owner() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", "admin:read:chats").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let (_, alice_ref, bob_ref) = send(&pool, &alice, &bob, "look at me").await;

    let alices_row = get(&app, &token, &chat_path(&alice_ref.chat_id.to_string())).await;
    common::assert_answer(&alices_row, StatusCode::OK, "alice's side of the pair");
    let shown = alices_row.json_value();
    assert_eq!(json_keys(&shown), CHAT_KEYS, "the ordinary Chat entity");
    assert_eq!(
        shown["id"],
        alice_ref.chat_id.to_string(),
        "keyed by her row"
    );
    assert_eq!(
        shown["account"]["id"],
        bob.id.to_string(),
        "rendered from the owner's seat: the account is bob"
    );
    assert_eq!(
        shown["last_message"]["id"],
        alice_ref.id.to_string(),
        "with the owner's newest message attached"
    );

    let bobs_row = get(&app, &token, &chat_path(&bob_ref.chat_id.to_string())).await;
    assert_eq!(
        bobs_row.json_value()["account"]["id"],
        alice.id.to_string(),
        "bob's own row is a second chat, rendered from HIS seat"
    );

    for raw in ["not-a-uuid", &Uuid::now_v7().to_string()] {
        let missing = get(&app, &token, &chat_path(raw)).await;
        common::assert_answer(
            &missing,
            StatusCode::NOT_FOUND,
            &format!("no chat answers to {raw:?}"),
        );
        assert_eq!(
            error_message(&missing).as_deref(),
            Some(RECORD_NOT_FOUND),
            "the record-miss wording, malformed or unknown alike"
        );
    }
}

// --- GET /chats/{id}/messages ------------------------------------------------------------------

/// The message listing answers the chat owner's references newest first
/// under the reference-id `Link` header, walks by cursor, and leaves a
/// withdrawn message out of every read — the model's live filter, the same
/// one the participants' clients see through. A malformed cursor is the 400
/// `Invalid id`, an unknown chat the 404.
#[tokio::test]
async fn the_admin_lists_a_chats_messages_newest_first_without_the_withdrawn() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let mut refs: Vec<ChatMessageReference> = Vec::new();
    for text in ["one", "two", "three"] {
        let (_, _, bob_side) = send(&pool, &alice, &bob, text).await;
        refs.push(bob_side);
    }
    refs.sort_unstable_by_key(|reference| reference.id);
    let chat_id = refs.first().expect("three references").chat_id.to_string();
    let path = messages_path(&chat_id);

    let listed = get(&app, &token, &path).await;
    common::assert_answer(&listed, StatusCode::OK, "listing the chat's messages");
    let newest_first: Vec<String> = refs.iter().map(|r| r.id.to_string()).rev().collect();
    assert_eq!(
        common::ids_of(&listed.json_value()),
        newest_first,
        "newest reference id first"
    );
    assert_message_entities(&listed.json_value(), &refs, &alice.id.to_string(), &chat_id);
    assert_eq!(
        listed.header("link"),
        Some(expected_link(&path, &refs[2].id.to_string(), &refs[0].id.to_string()).as_str()),
        "the cursors key on the page's first and last reference ids"
    );

    let tail = get(
        &app,
        &token,
        &format!("{path}?limit=2&max_id={}", refs[1].id),
    )
    .await;
    assert_eq!(
        common::ids_of(&tail.json_value()),
        vec![refs[0].id.to_string()],
        "max_id pages back through the chat"
    );
    let junk = get(&app, &token, &format!("{path}?since_id=nope")).await;
    common::assert_answer(&junk, StatusCode::BAD_REQUEST, "a broken cursor");
    assert_eq!(error_message(&junk).as_deref(), Some("Invalid id"));

    // Withdraw the middle message through the admin delete and every read
    // of this chat — this one included — loses it.
    let withdrawn = delete(
        &app,
        &token,
        &message_path(&chat_id, &refs[1].id.to_string()),
    )
    .await;
    common::assert_answer(&withdrawn, StatusCode::OK, "the withdrawal itself");
    let after = get(&app, &token, &path).await;
    assert_eq!(
        common::ids_of(&after.json_value()),
        vec![refs[2].id.to_string(), refs[0].id.to_string()],
        "the withdrawn message is gone from the admin's read too"
    );

    let nowhere = get(&app, &token, &messages_path(&Uuid::now_v7().to_string())).await;
    common::assert_answer(
        &nowhere,
        StatusCode::NOT_FOUND,
        "an unknown chat has no messages",
    );
    assert_eq!(
        error_message(&nowhere).as_deref(),
        Some(RECORD_NOT_FOUND),
        "one 404, malformed or unknown alike"
    );
    let malformed = get(&app, &token, &messages_path("not-a-uuid")).await;
    common::assert_answer(&malformed, StatusCode::NOT_FOUND, "a malformed chat id");
}

/// Checks a message-list page holds the full `ChatMessage` entities for
/// `refs`: every key, each entry rendered from the stored reference the
/// model wrote — its content, the writer's `account_id`, the named chat.
fn assert_message_entities(list: &Value, refs: &[ChatMessageReference], sender: &str, chat: &str) {
    for entry in list.as_array().expect("the list is an array") {
        assert_eq!(
            json_keys(entry),
            MESSAGE_KEYS,
            "every entry is a ChatMessage entity"
        );
        let source = refs
            .iter()
            .find(|r| r.id.to_string() == entry["id"].as_str().expect("an id"))
            .expect("only the fixture's own references are listed");
        assert_eq!(
            entry["content"], source.content,
            "rendered from the reference the model wrote"
        );
        assert_eq!(entry["account_id"], sender, "the writer's account id");
        assert_eq!(entry["chat_id"], chat, "all in the named chat");
    }
}
