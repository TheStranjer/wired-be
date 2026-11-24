//! Scenario (5): conversations — a direct post threading its participants,
//! replies, read-all and deletion.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::support::{
    CONVERSATIONS, NOT_FOUND, PLEROMA_CONVERSATIONS, STATUSES, acted, direct_post, got_array,
    ids_of, posted, sorted, user, withdrawn,
};
use crate::common;

/// Whether the caller's single listed conversation stands unread.
pub(super) async fn listed_unread(app: &Router, token: &str) -> bool {
    let mut page = got_array(app, token, CONVERSATIONS).await;
    assert_eq!(page.len(), 1, "one conversation is listed");
    page.remove(0)["unread"]
        .as_bool()
        .expect("unread is a boolean")
}

/// The account ids a conversation entity names, sorted for comparison.
fn participant_ids(entity: &Value) -> Vec<String> {
    sorted(
        entity["accounts"]
            .as_array()
            .expect("accounts is a list")
            .iter()
            .map(common::id_of)
            .collect::<Vec<String>>(),
    )
}

/// One participant's conversation entity: the DM as `last_status` and
/// exactly `owed` as the other participants (never the viewer).
fn assert_participation(name: &str, entity: &Value, dm_id: &str, owed: Vec<String>) {
    assert_eq!(
        entity["last_status"]["id"],
        json!(dm_id),
        "{name} sees the DM as the thread's newest post"
    );
    assert_eq!(
        participant_ids(entity),
        sorted(owed),
        "{name}'s conversation names the other participants, never the viewer"
    );
}

/// A `direct` post @-mentioning two local users opens one conversation
/// all three list: each side carries its own participation id, the
/// thread's newest post as `last_status`, the other two participants as
/// `accounts` (never the viewer), the author's own side read and every
/// addressee's unread — and `recipients[]` filters the listing on exactly
/// those addressee lists.
#[tokio::test]
async fn a_direct_post_threads_the_three_participants() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;
    let (carol, carol_token) = user(&pool, "carol").await;
    let dm = direct_post(&app, &alice_token, "@bob @carol psst").await;
    let dm_id = common::id_of(&dm);

    let alices = got_array(&app, &alice_token, CONVERSATIONS).await;
    let bobs = got_array(&app, &bob_token, CONVERSATIONS).await;
    let carols = got_array(&app, &carol_token, CONVERSATIONS).await;
    assert_eq!(
        (alices.len(), bobs.len(), carols.len()),
        (1, 1, 1),
        "the thread stands once for each participant"
    );

    let (alice_id, bob_id, carol_id) = (
        alice.id.to_string(),
        bob.id.to_string(),
        carol.id.to_string(),
    );
    let mut parts = vec![
        common::id_of(&alices[0]),
        common::id_of(&bobs[0]),
        common::id_of(&carols[0]),
    ];
    parts.sort();
    parts.dedup();
    assert_eq!(parts.len(), 3, "every side carries its own id");
    assert_eq!(
        alices[0]["unread"],
        json!(false),
        "the author's own post reads"
    );
    assert_eq!(
        bobs[0]["unread"],
        json!(true),
        "the first addressee owes it"
    );
    assert_eq!(carols[0]["unread"], json!(true), "and so does the second");
    assert_participation(
        "alice",
        &alices[0],
        &dm_id,
        vec![bob_id.clone(), carol_id.clone()],
    );
    assert_participation(
        "bob",
        &bobs[0],
        &dm_id,
        vec![alice_id.clone(), carol_id.clone()],
    );
    assert_participation("carol", &carols[0], &dm_id, vec![alice_id.clone(), bob_id]);

    let both = format!("?recipients={alice_id}&recipients={carol_id}");
    assert_eq!(
        ids_of(&got_array(&app, &bob_token, &format!("{CONVERSATIONS}{both}")).await),
        vec![common::id_of(&bobs[0])],
        "bob's thread addresses alice and carol together"
    );
    assert!(
        got_array(
            &app,
            &bob_token,
            &format!("{CONVERSATIONS}?recipients={}", Uuid::now_v7())
        )
        .await
        .is_empty(),
        "an account nobody shares the thread with empties the listing"
    );
}

/// The scenario 5 seed: a `direct` post by alice @-mentioning bob and
/// carol, beside the three tokens and each side's participation id in the
/// thread it opened.
#[derive(Debug)]
struct Thread {
    /// Alice's token.
    alice: String,
    /// Bob's token.
    bob: String,
    /// Carol's token.
    carol: String,
    /// Alice's own participation id.
    alice_part: String,
    /// Bob's own participation id.
    bob_part: String,
    /// Carol's own participation id.
    carol_part: String,
}

/// Posts the opening DM through HTTP and reads each participant's side of
/// the thread back from their own listing.
async fn open_thread(app: &Router, pool: &PgPool) -> Thread {
    let (_alice, alice_token) = user(pool, "alice").await;
    let (_bob, bob_token) = user(pool, "bob").await;
    let (_carol, carol_token) = user(pool, "carol").await;
    let _dm = direct_post(app, &alice_token, "@bob @carol thread me").await;
    Thread {
        alice_part: common::id_of(&got_array(app, &alice_token, CONVERSATIONS).await.remove(0)),
        bob_part: common::id_of(&got_array(app, &bob_token, CONVERSATIONS).await.remove(0)),
        carol_part: common::id_of(&got_array(app, &carol_token, CONVERSATIONS).await.remove(0)),
        alice: alice_token,
        bob: bob_token,
        carol: carol_token,
    }
}

/// A reply through `in_reply_to_conversation_id` joins the same thread for
/// everybody (forced `direct`, its recipients auto-addressed), and
/// Pleroma's `…/read` flips every unread participation at once, answering
/// exactly the rows that changed — the empty array when nothing is.
#[tokio::test]
async fn a_conversation_reply_joins_the_thread_and_read_all_flips_the_sides() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let thread = open_thread(&app, &pool).await;

    let reply = posted(
        &app,
        &thread.carol,
        STATUSES,
        &json!({ "status": "aye", "in_reply_to_conversation_id": thread.carol_part }),
    )
    .await;
    let reply_id = common::id_of(&reply);
    assert_eq!(
        reply["visibility"],
        json!("direct"),
        "the reply is forced direct"
    );

    let alices = got_array(&app, &thread.alice, CONVERSATIONS).await;
    assert_eq!(
        common::id_of(&alices[0]),
        thread.alice_part,
        "the reply joined the same thread, not a new one"
    );
    assert_eq!(
        alices[0]["last_status"]["id"],
        json!(reply_id),
        "and the reply is its newest post"
    );
    assert_eq!(alices[0]["unread"], json!(true), "alice owes it");

    let flipped = acted(
        &app,
        &thread.alice,
        &format!("{PLEROMA_CONVERSATIONS}/read"),
    )
    .await;
    let rows = flipped
        .as_array()
        .expect("the read-all answers an array")
        .clone();
    assert_eq!(
        rows.len(),
        1,
        "exactly the one unread participation flipped"
    );
    assert_eq!(
        common::id_of(&rows[0]),
        thread.alice_part,
        "the caller's own row"
    );
    assert_eq!(rows[0]["unread"], json!(false), "now read");
    assert_eq!(
        acted(
            &app,
            &thread.alice,
            &format!("{PLEROMA_CONVERSATIONS}/read")
        )
        .await,
        json!([]),
        "and an empty second pass flips nothing"
    );
}

/// `DELETE /api/v1/conversations/{id}` withdraws only its owner's side: a
/// stranger's id is the shared 404, the owner's listing empties, the other
/// participants keep listing the thread — newest post included — and the
/// thread's statuses stay readable to them.
#[tokio::test]
async fn a_conversation_delete_withdraws_only_the_owners_side() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let thread = open_thread(&app, &pool).await;
    let reply = posted(
        &app,
        &thread.carol,
        STATUSES,
        &json!({ "status": "aye", "in_reply_to_conversation_id": thread.carol_part }),
    )
    .await;
    let reply_id = common::id_of(&reply);

    let foreign = common::TestRequest::delete(&format!("{CONVERSATIONS}/{}", thread.carol_part))
        .bearer(&thread.bob)
        .send(&app)
        .await;
    common::assert_answer(&foreign, StatusCode::NOT_FOUND, "a stranger's withdrawal");
    common::assert_error(&foreign, NOT_FOUND, "the foreign participation id");

    assert_eq!(
        withdrawn(
            &app,
            &thread.carol,
            &format!("{CONVERSATIONS}/{}", thread.carol_part)
        )
        .await,
        json!({}),
        "the withdrawal answers the empty object"
    );
    assert!(
        got_array(&app, &thread.carol, CONVERSATIONS)
            .await
            .is_empty(),
        "carol's own side is gone"
    );
    let bobs = got_array(&app, &thread.bob, CONVERSATIONS).await;
    assert_eq!(
        ids_of(&bobs),
        vec![thread.bob_part],
        "bob keeps listing exactly his one participation"
    );
    assert_eq!(
        bobs[0]["last_status"]["id"],
        json!(reply_id),
        "with carol's reply as the newest post"
    );
    common::assert_answer(
        &common::TestRequest::get(&format!("{STATUSES}/{reply_id}"))
            .bearer(&thread.alice)
            .send(&app)
            .await,
        StatusCode::OK,
        "the statuses survive the withdrawal as readable posts",
    );
}
