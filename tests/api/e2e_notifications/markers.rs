//! Scenario (3): markers — the notifications marker round trip and its
//! unread linkage, the chat and conversation reads, and the stranger gate.

use axum::Router;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::support::{
    ACCOUNTS, BOTH_TIMELINES, CHATS, CONVERSATIONS, MARKERS, NOTIFICATIONS_TIMELINE, PLEROMA_READ,
    acted, direct_post, flat, follow, gate_strangers, got, got_array, ids_of, lock_account, post,
    posted, sorted, status_action, telling, told, unread, user,
};
use super::{
    chats::{chat_messages, chat_unread, open_chat, send_message},
    conversations::listed_unread,
};
use crate::common;

/// Seeds alice's three-telling inbox over HTTP (a favourite, a boost and a
/// mention) and answers the subject id beside the flat page's ids, newest
/// first.
async fn seed_three(app: &Router, pool: &PgPool, alice_token: &str) -> (String, Vec<String>) {
    let subject = common::id_of(&post(app, alice_token, "mark me").await);
    let (_bob, bob_token) = user(pool, "bob").await;
    let (_carol, carol_token) = user(pool, "carol").await;
    let (_dave, dave_token) = user(pool, "dave").await;
    let _liked = status_action(app, &bob_token, &subject, "favourite").await;
    let _boosted = status_action(app, &carol_token, &subject, "reblog").await;
    let _mentioned = post(app, &dave_token, "@alice the third").await;
    (subject, ids_of(&flat(app, alice_token, "").await))
}

/// The `POST /api/v1/markers` answer for the `subject` home position and
/// the `oldest` notifications position: two markers, version `0`, `home`
/// unread-free by Pleroma's rule and `notifications` counting the two
/// unseen rows that stand past the position.
fn assert_written_markers(written: &Value, subject: &str, oldest: &str) {
    assert_eq!(
        written["home"],
        json!({
            "last_read_id": subject,
            "version": 0,
            "updated_at": written["home"]["updated_at"],
            "pleroma": { "unread_count": 0 },
        }),
        "home round-trips its position and has no unread concept: {written}"
    );
    assert_eq!(
        written["notifications"]["last_read_id"],
        json!(oldest),
        "notifications round-trips"
    );
    assert_eq!(written["notifications"]["version"], 0, "a first marker");
    assert_eq!(
        written["notifications"]["pleroma"]["unread_count"], 2,
        "the two unseen rows past the position"
    );
}

/// `POST /api/v1/markers` writes the notifications and home positions and
/// `GET` reads exactly that object back — `last_read_id`, `version` and
/// Pleroma's `pleroma.unread_count` — and the notifications position is
/// the marker the v2 `unread_count` reads through: unseen rows past it
/// count in both places, marking them seen with Pleroma's `read` takes
/// both to zero, and a rewrite bumps the version.
#[tokio::test]
async fn markers_round_trip_and_drive_the_unread_linkage() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_token) = user(&pool, "alice").await;
    let (subject, ids) = seed_three(&app, &pool, &alice_token).await;
    let oldest = ids[2].clone();

    assert_eq!(
        got(&app, &alice_token, &format!("{MARKERS}{BOTH_TIMELINES}")).await,
        json!({}),
        "an account that marked nothing has no markers"
    );
    let written = posted(
        &app,
        &alice_token,
        MARKERS,
        &json!({
            "home": { "last_read_id": subject },
            "notifications": { "last_read_id": oldest },
        }),
    )
    .await;
    assert_written_markers(&written, &subject, &oldest);
    assert_eq!(
        unread(&app, &alice_token).await,
        2,
        "and the v2 count reads through the very same marker"
    );
    assert_eq!(
        got(&app, &alice_token, &format!("{MARKERS}{BOTH_TIMELINES}")).await,
        written,
        "the GET answers exactly what the POST wrote"
    );

    let read = posted(&app, &alice_token, PLEROMA_READ, &json!({ "id": ids[0] })).await;
    assert_eq!(
        read["pleroma"]["is_seen"],
        json!(true),
        "Pleroma's read answers the newest notification, seen now"
    );
    assert_eq!(
        unread(&app, &alice_token).await,
        0,
        "the v2 count fell to nothing"
    );
    let notes = got(
        &app,
        &alice_token,
        &format!("{MARKERS}{NOTIFICATIONS_TIMELINE}"),
    )
    .await;
    assert_eq!(
        notes["notifications"]["pleroma"]["unread_count"], 0,
        "and so did the marker's own, with no unseen row left"
    );

    let moved = posted(
        &app,
        &alice_token,
        MARKERS,
        &json!({ "notifications": { "last_read_id": ids[1] } }),
    )
    .await;
    assert_eq!(
        moved["notifications"]["version"], 1,
        "a rewrite bumps the version"
    );
    assert_eq!(
        moved["notifications"]["last_read_id"],
        json!(&ids[1]),
        "and moves the position"
    );
}

/// The read positions the DM surfaces keep outside the markers table also
/// move through their own endpoints: `POST /api/v1/conversations/{id}/read`
/// flips the participation's flag, and `POST /api/v1/pleroma/chats/{id}/read`
/// walks the chat's unread references behind the posted `last_read_id`.
#[tokio::test]
async fn the_chat_and_conversation_reads_move_their_own_markers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;

    let _dm = direct_post(&app, &alice_token, "@bob read me").await;
    let owed = common::id_of(&got_array(&app, &bob_token, CONVERSATIONS).await.remove(0));
    assert!(
        listed_unread(&app, &bob_token).await,
        "the fresh DM stands unread"
    );
    let _read = acted(&app, &bob_token, &format!("{CONVERSATIONS}/{owed}/read")).await;
    assert!(
        !listed_unread(&app, &bob_token).await,
        "the read endpoint moved the flag"
    );

    let hers = common::id_of(&open_chat(&app, &alice_token, &bob.id.to_string()).await);
    let his = common::id_of(&open_chat(&app, &bob_token, &alice.id.to_string()).await);
    let _first = send_message(&app, &alice_token, &hers, "ping").await;
    assert_eq!(
        chat_unread(&app, &bob_token, &his).await,
        1,
        "one message owed"
    );
    let refs = ids_of(&chat_messages(&app, &bob_token, &his).await);
    let marked = posted(
        &app,
        &bob_token,
        &format!("{CHATS}/{his}/read"),
        &json!({ "last_read_id": refs[0] }),
    )
    .await;
    assert_eq!(
        marked["unread"],
        json!(0),
        "the read answers the emptied chat"
    );
    assert_eq!(
        chat_unread(&app, &bob_token, &his).await,
        0,
        "the marker moved and the count followed"
    );
}

// ────────────────  Scenario (4): notification settings  ────────────────

/// `PUT /api/pleroma/notification_settings` with `block_from_strangers`
/// set: a stranger's favourite and mention never reach the inbox, an
/// account the recipient follows back gets through the gate
/// (followers-need-not), and the documented exemption lets a stranger's
/// follow request land on a gated, locked account anyway.
#[tokio::test]
async fn the_stranger_gate_withholds_all_but_the_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;
    let (dave, dave_token) = user(&pool, "dave").await;
    let (eve, eve_token) = user(&pool, "eve").await;

    gate_strangers(&app, &alice_token, true).await;
    let own = got(
        &app,
        &alice_token,
        &format!("{ACCOUNTS}/verify_credentials"),
    )
    .await;
    assert_eq!(
        own["pleroma"]["notification_settings"]["block_from_strangers"],
        json!(true),
        "the own-view renders the stored flag"
    );

    let subject = common::id_of(&post(&app, &alice_token, "gate me").await);
    let _withheld_like = status_action(&app, &bob_token, &subject, "favourite").await;
    let _withheld_mention = post(&app, &bob_token, "@alice can you hear me").await;
    assert!(
        flat(&app, &alice_token, "").await.is_empty(),
        "a stranger's like and mention are both withheld"
    );

    let followed = follow(&app, &alice_token, &dave.id.to_string()).await;
    assert_eq!(
        followed["following"],
        json!(true),
        "alice follows dave back"
    );
    let dave_id = dave.id.to_string();
    let _told_like = status_action(&app, &dave_token, &subject, "favourite").await;
    let dave_post = common::id_of(&post(&app, &dave_token, "@alice now we talk").await);
    assert_eq!(
        sorted(told(&flat(&app, &alice_token, "").await)),
        sorted(vec![
            telling("favourite", &dave_id, Some(&subject)),
            telling("mention", &dave_id, Some(&dave_post)),
        ]),
        "the account alice follows reaches the inbox, and nothing of \
         bob's ever did"
    );

    lock_account(&app, &eve_token).await;
    gate_strangers(&app, &eve_token, true).await;
    let _requested = follow(&app, &bob_token, &eve.id.to_string()).await;
    assert_eq!(
        told(&flat(&app, &eve_token, "").await),
        vec![telling("follow_request", &bob.id.to_string(), None)],
        "the exempt follow request still lands on a gated, locked account"
    );
}
