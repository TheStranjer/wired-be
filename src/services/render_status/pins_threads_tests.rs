//! Unit tests for the viewer's pin, thread-mute and direct-message
//! participation parts of a page render, each answered by one batched
//! read.

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    ViewerMode, render_statuses,
    test_support::{BASE, app_state, post, user},
};
use crate::{
    entities::status::Status,
    models::{
        conversation, conversation_mute, pinned_status,
        status::{self, NewStatus, Visibility, insert},
        user::User,
    },
    util::time::mastodon_time,
};

/// Pins `status_id` as `who`, asserting the write created its row — the
/// state the pin flag test starts from.
async fn pin_it(pool: &PgPool, who: &User, status_id: Uuid) {
    let created = pinned_status::pin(pool, who.id, status_id)
        .await
        .expect("the fixture pin stores");
    assert!(created.is_some(), "the fixture pair had not pinned yet");
}

/// The pinning ticket's loader inside the fixed query set: one batched
/// read answers both the `pinned` flag and `pleroma.pinned_at` of a
/// whole page — and, like the bookmarks flag, keyed to each row's *own*
/// id, so a reblog of a pinned original renders unflagged while the
/// nested original carries the pin. An anonymous page is nobody's pin.
#[tokio::test]
async fn the_pinned_flag_and_stamp_follow_one_batched_read() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let own = post(&pool, &alice, "alice's own").await;
    let second = post(&pool, &alice, "second").await;
    let reblog = insert(
        &pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(own.id),
            ..NewStatus::local(&bob, "")
        },
    )
    .await
    .expect("a reblog of a stored post inserts");
    pin_it(&pool, &alice, own.id).await;

    let stamped = pinned_status::pinned_ids(&pool, alice.id, &[own.id])
        .await
        .expect("the pin reads")
        .get(&own.id)
        .copied()
        .expect("alice pinned her own post");

    let flags = |entity: &Status| {
        let value = serde_json::to_value(entity).expect("a rendered status serializes");
        (
            value["pinned"].clone(),
            value["pleroma"]["pinned_at"].clone(),
        )
    };
    let page = [own.clone(), second.clone(), reblog.clone()];
    let for_alice = render_statuses(&state, ViewerMode::user(&alice), &page)
        .await
        .expect("the page renders for alice");
    assert_eq!(
        flags(&for_alice[0]),
        (json!(true), json!(mastodon_time(&stamped))),
        "alice's pin shows in the flag and in the stamp together"
    );
    assert_eq!(
        flags(&for_alice[1]),
        (json!(false), json!(null)),
        "the unpinned post carries neither"
    );
    let reblog_json = serde_json::to_value(&for_alice[2]).expect("the reblog serializes");
    assert_eq!(
        reblog_json["pinned"],
        json!(false),
        "bob's boost of her post is not alice's pin — a pin is keyed to \
         the row its account owns"
    );
    assert_eq!(
        reblog_json["reblog"]["pinned"],
        json!(true),
        "and the nested original carries the pin, from the same batch"
    );
    assert_eq!(
        reblog_json["reblog"]["pleroma"]["pinned_at"],
        json!(mastodon_time(&stamped)),
        "with its stamp"
    );

    let for_bob = render_statuses(&state, ViewerMode::user(&bob), std::slice::from_ref(&own))
        .await
        .expect("the post renders for bob");
    assert_eq!(
        flags(&for_bob[0]),
        (json!(false), json!(null)),
        "alice's pin is nobody else's flag"
    );
    let anonymous = render_statuses(&state, ViewerMode::ANONYMOUS, &page)
        .await
        .expect("the page renders anonymously");
    assert!(
        anonymous
            .iter()
            .all(|entity| flags(entity) == (json!(false), json!(null))),
        "an anonymous page is nobody's pin"
    );
}

/// The thread-mute ticket's loader inside the fixed query set: one
/// batched read answers both the `muted` flag and `pleroma.thread_muted`
/// of a whole page — keyed to each row's *own* `conversation_id`, so a
/// reblog row and its nested original carry the same pair — and an
/// anonymous page is nobody's mute.
#[tokio::test]
async fn the_thread_muted_flags_follow_one_batched_read() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let muted = post(&pool, &bob, "in a muted thread").await;
    let open = post(&pool, &bob, "in an open thread").await;
    let (boost, made) = status::create_reblog(&pool, BASE, &alice, &muted, Visibility::Public)
        .await
        .expect("a reblog of a stored post inserts");
    assert!(made, "the fixture boost is a fresh row");
    conversation_mute::mute(&pool, alice.id, muted.conversation_id)
        .await
        .expect("the fixture thread mute stores");

    let flags = |entity: &Status| {
        let value = serde_json::to_value(entity).expect("a rendered status serializes");
        (
            value["muted"].clone(),
            value["pleroma"]["thread_muted"].clone(),
        )
    };
    let page = [muted.clone(), open.clone(), boost.clone()];
    let for_alice = render_statuses(&state, ViewerMode::user(&alice), &page)
        .await
        .expect("the page renders for alice");
    assert_eq!(
        flags(&for_alice[0]),
        (json!(true), json!(true)),
        "alice's thread mute shows in both halves of her read"
    );
    assert_eq!(
        flags(&for_alice[1]),
        (json!(false), json!(false)),
        "the open thread carries neither"
    );
    let boost_json = serde_json::to_value(&for_alice[2]).expect("the reblog serializes");
    assert_eq!(
        boost_json["muted"],
        json!(true),
        "the reblog shares the muted thread's conversation, so it is \
         flagged too — one key, the row's own conversation id"
    );
    assert_eq!(
        boost_json["reblog"]["pleroma"]["thread_muted"],
        json!(true),
        "and the nested original carries the same pair"
    );

    let for_bob = render_statuses(&state, ViewerMode::user(&bob), std::slice::from_ref(&muted))
        .await
        .expect("the post renders for bob");
    assert_eq!(
        flags(&for_bob[0]),
        (json!(false), json!(false)),
        "alice's mute is nobody else's flag"
    );
    let anonymous = render_statuses(&state, ViewerMode::ANONYMOUS, &page)
        .await
        .expect("the page renders anonymously");
    assert!(
        anonymous
            .iter()
            .all(|entity| flags(entity) == (json!(false), json!(false))),
        "an anonymous page is nobody's mute"
    );
}

/// The conversation-participations ticket's loader inside the fixed
/// query set: one batched read answers `pleroma.direct_conversation_id`
/// of a whole page with each viewer's own participation id — the same
/// DM thread renders a different id per participant, and a non-DM row,
/// a non-participant and the anonymous request all render `null`.
#[tokio::test]
async fn the_direct_conversation_id_follows_one_batched_read() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let carol = user(&pool, "carol").await;
    let dm = insert(
        &pool,
        BASE,
        &NewStatus {
            visibility: Visibility::Direct,
            mentioned_user_ids: vec![bob.id],
            ..NewStatus::local(&alice, "@bob psst")
        },
    )
    .await
    .expect("the direct fixture inserts");
    conversation::participate_for_status(&pool, &dm)
        .await
        .expect("the fixture thread bump runs");
    // Re-read for the render: the page renderer keys the participation
    // to the row's stored `direct_conversation_id`, which the bump
    // writes after the insert's own answer.
    let dm = status::get_by_id(&pool, dm.id)
        .await
        .expect("the row reads")
        .expect("the fixture is live");
    let thread = dm
        .direct_conversation_id
        .expect("the bump linked the thread");
    let quiet = post(&pool, &bob, "public noise").await;

    let field = |entity: &Status| {
        serde_json::to_value(entity).expect("a rendered status serializes")["pleroma"]
            ["direct_conversation_id"]
            .clone()
    };
    let alice_expected = the_participation(&pool, &alice, thread).await;
    let bob_expected = the_participation(&pool, &bob, thread).await;
    let page = [dm.clone(), quiet.clone()];

    let for_alice = render_statuses(&state, ViewerMode::user(&alice), &page)
        .await
        .expect("the page renders for alice");
    assert_eq!(
        field(&for_alice[0]),
        json!(alice_expected),
        "the author sees her own participation id"
    );
    assert_eq!(
        field(&for_alice[1]),
        Value::Null,
        "and the public row on the same page stays unkeyed"
    );

    let for_bob = render_statuses(&state, ViewerMode::user(&bob), std::slice::from_ref(&dm))
        .await
        .expect("the post renders for bob");
    assert_eq!(
        field(&for_bob[0]),
        json!(bob_expected),
        "bob's view of the same DM names his participation, not alice's"
    );

    let for_carol = render_statuses(&state, ViewerMode::user(&carol), std::slice::from_ref(&dm))
        .await
        .expect("the post renders for carol");
    assert_eq!(
        field(&for_carol[0]),
        Value::Null,
        "a stranger to the thread renders null"
    );
    let anonymous = render_statuses(&state, ViewerMode::ANONYMOUS, &page)
        .await
        .expect("the page renders anonymously");
    assert!(
        anonymous.iter().all(|entity| field(entity) == Value::Null),
        "an anonymous page has no participation to name"
    );
}

/// `who`'s stored participation id in `thread`, as the wire spells it.
async fn the_participation(pool: &PgPool, who: &User, thread: Uuid) -> String {
    conversation::participation_ids(pool, who.id, &[thread])
        .await
        .expect("the participation reads")
        .get(&thread)
        .copied()
        .expect("the fixture account participates")
        .to_string()
}
