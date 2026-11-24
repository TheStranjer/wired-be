//! Scenarios 4-6: pin/unpin, thread mute/unmute and emoji reactions,
//! each read back through every surface.

use axum::Router;
use serde_json::{Value, json};

use super::support::{
    ACCOUNTS, PARTY, PARTY_SEGMENT, PLEROMA, STATUSES, THUMB, THUMB_SEGMENT, acted,
    app_with_pin_limit, assert_every, created, follows, get, pick, post, reacted, representations,
    user, withdrawn,
};
use crate::common;

// ────────────────────────  Scenario (4): pin  ────────────────────────

/// Pins flag the owner's every read with `pinned` and
/// `pleroma.pinned_at`, drive the profile's `?pinned=true` page in
/// newest-pin-first order, and an unpin walks the flag and the page back.
#[tokio::test]
async fn pins_flag_the_owner_and_order_the_profile() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app_with_pin_limit(&pool, 5);
    let (alice, alice_token) = user(&pool, "alice").await;
    let account_id = alice.id.to_string();
    let first = common::id_of(&post(&app, &alice_token, "pin me first").await);
    let second = common::id_of(&post(&app, &alice_token, "and me second").await);

    let pinned = acted(&app, &alice_token, &format!("{STATUSES}/{first}/pin")).await;
    assert_eq!(pinned["pinned"], json!(true), "the pin flags the answer");
    assert!(
        pinned["pleroma"]["pinned_at"].is_string(),
        "and stamps it: {pinned}"
    );
    let second_pin = acted(&app, &alice_token, &format!("{STATUSES}/{second}/pin")).await;
    assert_eq!(
        second_pin["pinned"],
        json!(true),
        "the second pin flags too"
    );

    let page = get(
        &app,
        Some(&alice_token),
        &format!("{ACCOUNTS}/{account_id}/statuses?pinned=true"),
    )
    .await
    .json_value();
    assert_eq!(
        common::ids_of(&page),
        vec![second.clone(), first.clone()],
        "the featured page answers newest pin first"
    );

    let owners = representations(&app, Some(&alice_token), &first).await;
    assert_every(&owners, &["pinned"], &json!(true), "the owner's reads");
    let strangers = representations(&app, None, &first).await;
    assert_every(
        &strangers,
        &["pinned"],
        &json!(false),
        "a stranger sees nobody's pin",
    );
    assert_every(
        &strangers,
        &["pleroma", "pinned_at"],
        &Value::Null,
        "and none of its stamp",
    );

    let released = acted(&app, &alice_token, &format!("{STATUSES}/{first}/unpin")).await;
    assert_eq!(
        released["pinned"],
        json!(false),
        "the unpin clears the flag"
    );
    assert_eq!(
        released["pleroma"]["pinned_at"],
        Value::Null,
        "and its stamp"
    );
    let rest = get(
        &app,
        Some(&alice_token),
        &format!("{ACCOUNTS}/{}/statuses?pinned=true", alice.id),
    )
    .await
    .json_value();
    assert_eq!(
        common::ids_of(&rest),
        vec![second],
        "the page keeps the standing pin alone"
    );
}

// ─────────────────────  Scenario (5): thread mute  ─────────────────────

/// A thread mute flags the muting viewer's every read of the whole
/// conversation with `muted` and `pleroma.thread_muted`, hides nothing
/// from anybody else, and the unmute clears the pair.
#[tokio::test]
async fn a_thread_mute_flags_the_whole_thread_until_the_unmute() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_token) = user(&pool, "alice").await;
    let id = common::id_of(&post(&app, &alice_token, "a thread to mute").await);
    let answer = common::id_of(
        &created(
            &app,
            &alice_token,
            json!({"status": "and its reply", "in_reply_to_id": id}),
        )
        .await,
    );

    let muted = acted(&app, &alice_token, &format!("{STATUSES}/{id}/mute")).await;
    assert_eq!(muted["muted"], json!(true), "the write flags the answer");
    assert_eq!(
        muted["pleroma"]["thread_muted"],
        json!(true),
        "on both halves of the pair"
    );

    let own = representations(&app, Some(&alice_token), &id).await;
    assert_every(&own, &["muted"], &json!(true), "the muter's reads");
    assert_every(
        &own,
        &["pleroma", "thread_muted"],
        &json!(true),
        "the muter's reads, both halves",
    );
    let thread = representations(&app, Some(&alice_token), &answer).await;
    assert_every(
        &thread,
        &["pleroma", "thread_muted"],
        &json!(true),
        "the mute covers the whole conversation",
    );
    let strangers = representations(&app, None, &id).await;
    assert_every(
        &strangers,
        &["muted"],
        &json!(false),
        "one muter is nobody else's",
    );
    assert_every(
        &strangers,
        &["pleroma", "thread_muted"],
        &json!(false),
        "on either half of the pair",
    );

    let released = acted(&app, &alice_token, &format!("{STATUSES}/{id}/unmute")).await;
    assert_eq!(
        released["muted"],
        json!(false),
        "the unmute clears the flag"
    );
    assert_eq!(
        released["pleroma"]["thread_muted"],
        json!(false),
        "and the extension with it",
    );
    assert_every(
        &representations(&app, Some(&alice_token), &id).await,
        &["muted"],
        &json!(false),
        "every read agrees the thread is open",
    );
}

// ────────────────────  Scenario (6): emoji reactions  ────────────────────

/// The `count` entries of `pleroma.emoji_reactions` are this instance's
/// reaction counts (Pleroma 2.10's spelling, whose `reaction_count`
/// stays inside the activity object): adding an emoji counts on every
/// representation and on the reactions endpoint, and withdrawing clears
/// both again.
#[tokio::test]
async fn reactions_step_the_rollup_and_the_reactions_endpoint() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;
    let id = common::id_of(&post(&app, &alice_token, "react to me").await);
    let bob_id = bob.id.to_string();
    follows(&app, &bob_token, &alice.id.to_string()).await;

    let thumb_on = reacted(
        &app,
        &bob_token,
        &format!("{PLEROMA}/{id}/reactions/{THUMB_SEGMENT}"),
    )
    .await;
    assert_eq!(
        thumb_on["pleroma"]["emoji_reactions"],
        json!([rollup(THUMB, true, &bob_id)]),
        "the write answers the whole roll-up"
    );
    let repeat = reacted(
        &app,
        &bob_token,
        &format!("{PLEROMA}/{id}/reactions/{THUMB_SEGMENT}"),
    )
    .await;
    assert_eq!(
        repeat["pleroma"]["emoji_reactions"], thumb_on["pleroma"]["emoji_reactions"],
        "a repeat restates one reaction"
    );
    let partied = reacted(
        &app,
        &bob_token,
        &format!("{PLEROMA}/{id}/reactions/{PARTY_SEGMENT}"),
    )
    .await;
    let mine = json!([rollup(THUMB, true, &bob_id), rollup(PARTY, true, &bob_id)]);
    assert_eq!(
        pick(&partied, &["pleroma", "emoji_reactions"]),
        mine,
        "the second emoji rolls up beside the first"
    );

    let views = representations(&app, Some(&bob_token), &id).await;
    assert_every(
        &views,
        &["pleroma", "emoji_reactions"],
        &mine,
        "the reactor's reads",
    );
    let theirs = json!([rollup(THUMB, false, &bob_id), rollup(PARTY, false, &bob_id)]);
    let strangers = representations(&app, None, &id).await;
    assert_every(
        &strangers,
        &["pleroma", "emoji_reactions"],
        &theirs,
        "a stranger counts without being one",
    );
    assert_listed(&app, &bob_token, &id, &bob_id).await;
    assert_withdrawals(&app, &bob_token, &id, &bob_id).await;
}

/// One `pleroma.emoji_reactions` entry: `emoji` counted once, flagged for
/// the viewer by `me`, with `reactor` as its only listed account.
fn rollup(emoji: &str, me: bool, reactor: &str) -> Value {
    json!({
        "name": emoji,
        "count": 1,
        "me": me,
        "url": null,
        "account_ids": [reactor],
    })
}

/// The reactions endpoint read for status `id`: two entries in reaction
/// order, each counting its single reactor and carrying them as
/// [`Account`] entities.
///
/// [`Account`]: wired_be::entities::account::Account
async fn assert_listed(app: &Router, token: &str, id: &str, reactor: &str) {
    let listed = get(app, Some(token), &format!("{PLEROMA}/{id}/reactions"))
        .await
        .json_value();
    let entries = listed.as_array().expect("the reactions read is an array");
    assert_eq!(
        entries.len(),
        2,
        "one entry per emoji, first reaction first"
    );
    assert_eq!(entries[0]["name"], json!(THUMB), "the thumbs-up came first");
    assert_eq!(entries[0]["count"], json!(1), "each counting its reactor");
    assert_eq!(
        entries[0]["accounts"][0]["id"],
        json!(reactor),
        "and naming them as accounts"
    );
}

/// Withdrawing both reactions in turn: the first withdrawal leaves the
/// other emoji standing, the last empties the write's answer, the
/// reactions endpoint and every read representation.
async fn assert_withdrawals(app: &Router, token: &str, id: &str, reactor: &str) {
    let thumbed_off = withdrawn(
        app,
        token,
        &format!("{PLEROMA}/{id}/reactions/{THUMB_SEGMENT}"),
    )
    .await;
    assert_eq!(
        thumbed_off["pleroma"]["emoji_reactions"],
        json!([rollup(PARTY, true, reactor)]),
        "the withdrawal leaves the other emoji standing"
    );
    let last_off = withdrawn(
        app,
        token,
        &format!("{PLEROMA}/{id}/reactions/{PARTY_SEGMENT}"),
    )
    .await;
    assert_eq!(
        last_off["pleroma"]["emoji_reactions"],
        json!([]),
        "and the last one empties the roll-up"
    );
    let empty = get(app, Some(token), &format!("{PLEROMA}/{id}/reactions"))
        .await
        .json_value();
    assert_eq!(empty, json!([]), "the reactions read empties with it");
    assert_every(
        &representations(app, Some(token), id).await,
        &["pleroma", "emoji_reactions"],
        &json!([]),
        "every read agrees the post is unreacted",
    );
}
