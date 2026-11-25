//! Inbound thread backfill, ballots and chat messages end to end.

use serde_json::{Value, json};
use wired_be::models::notification::TYPE_CHAT_MENTION;

use super::support::{
    AS_PUBLIC, BOB, CHATS, DAVE, STATUSES, activity, carries_uri, deliver, entry_for, get, list,
    local, note_object, post_status, public_timeline, told_of, uri_of,
};
use crate::{common, remote};

/// Scenario: thread backfill. A delivered reply whose parent this instance has
/// never seen fetches the missing parent from the mock over real HTTP and
/// renders the full `context` with the ancestor present in the right order.
#[tokio::test]
async fn a_delivered_reply_backfills_its_missing_parent_from_the_peer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    // The mock's own `/objects/parent` is the ancestor: the reply answers it,
    // and the backfill fetch dereferences it over the wire.
    let parent_uri = peer.note("parent");
    let child_uri = peer.note("child");
    let object = note_object(
        &peer,
        &actor,
        "child",
        "<p>answering the parent</p>",
        json!({"inReplyTo": parent_uri, "context": peer.note("thread")}),
    );
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Create", "child", &actor, object),
    )
    .await;

    let public = public_timeline(&app).await;
    assert!(
        carries_uri(&public, &child_uri),
        "the reply landed even though its parent was unknown on arrival"
    );
    assert!(
        carries_uri(&public, &parent_uri),
        "and the backfill fetched and ingested the parent"
    );
    let child_id = common::id_of(entry_for(&public, &child_uri));
    let context = get(&app, None, &format!("{STATUSES}/{child_id}/context"))
        .await
        .json_value();
    let ancestors: Vec<&str> = context["ancestors"]
        .as_array()
        .expect("ancestors is an array")
        .iter()
        .map(|entry| entry["uri"].as_str().expect("a uri'd ancestor"))
        .collect();
    assert_eq!(
        ancestors,
        [parent_uri.as_str()],
        "the fetched parent is the sole ancestor"
    );
}

/// Scenario: the degraded backfill. A reply to a parent the mock 404s stands
/// on its own — the thread stays as stored, no ancestor is invented — the way
/// the code degrades today without dropping the delivered reply.
#[tokio::test]
async fn a_reply_to_a_missing_parent_stands_when_the_peer_404s() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    // A path the mock serves nothing at: the backfill's fetch is a 404, which
    // the resolver treats as a thread that legitimately starts here.
    let missing = format!("{}/gone-parent", peer.origin());
    let child_uri = peer.note("orphan");
    let object = note_object(
        &peer,
        &actor,
        "orphan",
        "<p>answering nothing</p>",
        json!({"inReplyTo": missing}),
    );
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Create", "orphan", &actor, object),
    )
    .await;

    let public = public_timeline(&app).await;
    assert!(
        carries_uri(&public, &child_uri),
        "the reply stands on the timeline despite its parent being gone"
    );
    let child_id = common::id_of(entry_for(&public, &child_uri));
    let context = get(&app, None, &format!("{STATUSES}/{child_id}/context"))
        .await
        .json_value();
    let ancestors = context["ancestors"]
        .as_array()
        .expect("the context carries an ancestors array");
    assert!(
        ancestors.is_empty(),
        "and the context carries no invented ancestor"
    );
}

/// Scenario: a remote ballot on a local poll. A delivered `Create` of the
/// answer shape `src/federation/handlers/vote.rs` accepts marks the option's
/// count through `GET /api/v1/polls/{id}`, is idempotent when restated, and
/// raises no `poll` notification for the local poster.
#[tokio::test]
async fn a_remote_ballot_marks_a_local_poll_and_tells_its_author() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-ballot-alice").await;
    let actor = peer.actor(BOB);

    let posted = post_status(
        &app,
        &alice_token,
        json!({"status": "tea or coffee?", "poll": {"options": ["tea", "coffee"], "expires_in": 3600}}),
    )
    .await;
    let poll_id = common::id_of(&posted["poll"]);
    let question_uri = uri_of(&posted);

    let vote = vote_create(&peer, &actor, "ballot-1", &question_uri, "tea");
    deliver(&app, &state, &peer, BOB, &vote).await;
    let stand = get(
        &app,
        Some(&alice_token),
        &format!("/api/v1/polls/{poll_id}"),
    )
    .await
    .json_value();
    assert_eq!(
        stand["options"][0]["votes_count"],
        json!(1),
        "the delivered ballot marked the tea option"
    );
    assert_eq!(stand["votes_count"], json!(1), "and the poll's total");
    assert!(
        told_of(&app, &alice_token, "poll").await.is_empty(),
        "the ballot tells the author nothing"
    );

    deliver(&app, &state, &peer, BOB, &vote).await;
    let restated = get(
        &app,
        Some(&alice_token),
        &format!("/api/v1/polls/{poll_id}"),
    )
    .await
    .json_value();
    assert_eq!(
        restated["votes_count"],
        json!(1),
        "the restated ballot counted nothing further"
    );
}

/// Scenario: an inbound chat message. The mock's `Create{ChatMessage}` opens
/// the pair's chat with exactly one `pleroma:chat_mention`, readable through
/// the chat endpoints; the sender's `Delete` withdraws the message and the
/// telling, and a stranger's `Delete` leaves both standing.
#[tokio::test]
async fn a_delivered_chat_message_lands_the_pair_and_one_telling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ei-chat-alice").await;
    let actor = peer.actor(BOB);

    let message_uri = peer.note("chat-hi");
    let create = chat_create(&peer, &actor, "chat-hi", &alice.ap_id, "<p>hi alice</p>");
    deliver(&app, &state, &peer, BOB, &create).await;

    let told = told_of(&app, &alice_token, TYPE_CHAT_MENTION).await;
    assert_eq!(told.len(), 1, "exactly one chat_mention for the recipient");
    assert!(
        told[0].get("status").is_none(),
        "and the telling names no status"
    );
    // The recipient's own chat is keyed by the message id the telling carries;
    // it is read back through the chat endpoints, never a row dump.
    let chat_id = told[0]["chat_message"]["chat_id"]
        .as_str()
        .expect("the chat mention names its chat")
        .to_owned();
    let chat = get(&app, Some(&alice_token), &format!("{CHATS}/{chat_id}"))
        .await
        .json_value();
    assert_eq!(chat["unread"], json!(1), "the recipient's side is unread");
    let messages = list(
        &app,
        Some(&alice_token),
        &format!("{CHATS}/{chat_id}/messages"),
    )
    .await;
    assert_eq!(
        messages[0]["content"],
        json!("<p>hi alice</p>"),
        "the delivered text renders in the chat's messages"
    );

    // A stranger's Delete leaves the message and its telling standing.
    let dave = peer.actor(DAVE);
    deliver(
        &app,
        &state,
        &peer,
        DAVE,
        &chat_delete(&peer, &dave, "chat-steal", &message_uri),
    )
    .await;
    assert_eq!(
        told_of(&app, &alice_token, TYPE_CHAT_MENTION).await.len(),
        1,
        "the stranger moved nothing"
    );

    // The sender's own Delete withdraws both.
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &chat_delete(&peer, &actor, "chat-gone", &message_uri),
    )
    .await;
    assert!(
        told_of(&app, &alice_token, TYPE_CHAT_MENTION)
            .await
            .is_empty(),
        "the telling went with it"
    );
    let after = list(
        &app,
        Some(&alice_token),
        &format!("{CHATS}/{chat_id}/messages"),
    )
    .await;
    assert!(
        after.is_empty(),
        "and the withdrawn message leaves the chat's pages"
    );
}

// --- scenario-local builders ------------------------------------------------------------

/// A `Create` of the answer document `src/federation/handlers/vote.rs` folds:
/// a bodyless `Note` naming option `option` and answering the question at
/// `question_uri`, on the peer's origin named `name`.
fn vote_create(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    question_uri: &str,
    option: &str,
) -> Value {
    let object = json!({
        "id": format!("{}/objects/vote-{}", peer.origin(), name),
        "type": "Note",
        "attributedTo": actor,
        "to": [AS_PUBLIC],
        "inReplyTo": question_uri,
        "name": option,
    });
    peer.activity("Create", name, actor, &object)
}

/// A `Create{ChatMessage}` from `actor` to the local account `recipient_ap`,
/// the object named `name` on the peer, the text `content`.
fn chat_create(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    recipient_ap: &str,
    content: &str,
) -> Value {
    let object = json!({
        "id": peer.note(name),
        "type": "ChatMessage",
        "attributedTo": actor,
        "to": [recipient_ap],
        "content": content,
        "published": "2024-06-01T10:00:00.000Z",
    });
    peer.activity("Create", name, actor, &object)
}

/// A `Delete` by `actor` of the chat message named `message_uri`, its own
/// activity named `name`.
fn chat_delete(peer: &remote::MockPeer, actor: &str, name: &str, message_uri: &str) -> Value {
    let object = json!({"id": message_uri, "type": "ChatMessage"});
    peer.activity("Delete", name, actor, &object)
}
