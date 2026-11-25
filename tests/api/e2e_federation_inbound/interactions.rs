//! Inbound likes, reactions and announces, and their undos, end to end.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::custom_emoji;

use super::support::{
    AS_PUBLIC, BOB, REACTIONS, STATUSES, activity, carries_uri, deliver, get, list, local, merge,
    post_status, public_timeline, told_of, uri_of,
};
use crate::{common, remote};

/// Scenario: the favourite and its withdrawal. A delivered `Like` raises the
/// author's `favourite_count` and one telling; the `Undo` takes both back.
#[tokio::test]
async fn a_remote_like_and_its_undo_move_the_count_and_telling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-like-alice").await;
    let posted = post_status(&app, &alice_token, json!({"status": "favourite me"})).await;
    let id = common::id_of(&posted);
    let actor = peer.actor(BOB);

    let like = activity(&peer, "Like", "like-1", &actor, json!(uri_of(&posted)));
    deliver(&app, &state, &peer, BOB, &like).await;
    let liked = get(&app, Some(&alice_token), &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        liked["favourites_count"],
        json!(1),
        "the delivered like raised the count"
    );
    let told = told_of(&app, &alice_token, "favourite").await;
    assert_eq!(told.len(), 1, "and raised one telling");
    assert_eq!(
        told[0]["account"]["acct"],
        json!(peer.handle(BOB)),
        "by the mock's actor"
    );

    let like_id = like["id"]
        .as_str()
        .expect("the Like carries its id")
        .to_owned();
    let inner = json!({"id": like_id, "type": "Like", "actor": actor});
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Undo", "like-undo", &actor, inner),
    )
    .await;
    let unliked = get(&app, Some(&alice_token), &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        unliked["favourites_count"],
        json!(0),
        "the undo took the count back"
    );
    assert!(
        told_of(&app, &alice_token, "favourite").await.is_empty(),
        "and cleared the telling"
    );
}

/// Scenario: the emoji reaction and its withdrawal. A delivered `EmojiReact`
/// lands in the reactions read and the inline entity and tells the author;
/// the `Undo` removes both.
#[tokio::test]
async fn a_remote_reaction_and_its_undo_land_in_the_reactions_read() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ei-react-alice").await;
    let posted = post_status(&app, &alice_token, json!({"status": "react to me"})).await;
    let id = common::id_of(&posted);
    let actor = peer.actor(BOB);

    let react = {
        let mut react = activity(
            &peer,
            "EmojiReact",
            "react-1",
            &actor,
            json!(uri_of(&posted)),
        );
        merge(&mut react, json!({"content": "🎉", "to": [alice.ap_id]}));
        react
    };
    deliver(&app, &state, &peer, BOB, &react).await;
    let reactions = list(
        &app,
        Some(&alice_token),
        &format!("{REACTIONS}/{id}/reactions"),
    )
    .await;
    assert_eq!(reactions.len(), 1, "one reaction landed");
    assert_eq!(reactions[0]["name"], json!("🎉"), "the delivered emoji");
    assert_eq!(
        told_of(&app, &alice_token, "pleroma:emoji_reaction")
            .await
            .len(),
        1
    );

    let react_id = react["id"]
        .as_str()
        .expect("the EmojiReact carries its id")
        .to_owned();
    let inner = json!({"id": react_id, "type": "EmojiReact", "actor": actor});
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Undo", "react-undo", &actor, inner),
    )
    .await;
    assert!(
        list(
            &app,
            Some(&alice_token),
            &format!("{REACTIONS}/{id}/reactions")
        )
        .await
        .is_empty(),
        "the undo emptied the reactions read"
    );
    assert!(
        told_of(&app, &alice_token, "pleroma:emoji_reaction")
            .await
            .is_empty(),
        "and cleared the reaction telling"
    );
}

/// Scenario: the boost and its withdrawal. A delivered public `Announce`
/// lands a boost of the author's post on the public timeline and raises the
/// `reblog` telling; the `Undo` takes the boost back out and clears the
/// telling.
#[tokio::test]
async fn a_remote_announce_and_its_undo_boost_the_public_timeline() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-announce-alice").await;
    let posted = post_status(&app, &alice_token, json!({"status": "boost me"})).await;
    let id = common::id_of(&posted);
    let actor = peer.actor(BOB);

    let announce = {
        let mut announce = activity(&peer, "Announce", "boost-1", &actor, json!(uri_of(&posted)));
        merge(&mut announce, json!({"to": [AS_PUBLIC]}));
        announce
    };
    let announce_id = announce["id"]
        .as_str()
        .expect("the Announce carries its id")
        .to_owned();
    deliver(&app, &state, &peer, BOB, &announce).await;

    assert!(
        carries_uri(&public_timeline(&app).await, &announce_id),
        "the boost appears on the public timeline under the Announce id"
    );
    let boosted = get(&app, Some(&alice_token), &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        boosted["reblogs_count"],
        json!(1),
        "and the original counts the boost"
    );
    assert_eq!(
        told_of(&app, &alice_token, "reblog").await.len(),
        1,
        "the author is told once"
    );

    let inner = json!({"id": announce_id, "type": "Announce", "actor": actor});
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Undo", "boost-undo", &actor, inner),
    )
    .await;
    assert!(
        !carries_uri(&public_timeline(&app).await, &announce_id),
        "the undo withdrew the boost from the timeline"
    );
    let unboosted = get(&app, Some(&alice_token), &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        unboosted["reblogs_count"],
        json!(0),
        "and stepped the count back"
    );
    assert!(
        told_of(&app, &alice_token, "reblog").await.is_empty(),
        "and cleared the telling"
    );
}

/// Scenario: joining a remote custom reaction. A delivered `EmojiReact` with an
/// `Emoji` tag is named `shortcode@domain` in the reactions read, and the local
/// author reacting with that name lands in the same entry.
#[tokio::test]
async fn a_remote_custom_reaction_is_named_for_its_instance_and_can_be_joined() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-custom-alice").await;
    let posted = post_status(&app, &alice_token, json!({"status": "react to me"})).await;
    let id = common::id_of(&posted);
    let actor = peer.actor(BOB);
    let image = format!("{}/files/misskeycat.png", peer.origin());

    let react = {
        let mut react = activity(
            &peer,
            "EmojiReact",
            "custom-react-1",
            &actor,
            json!(uri_of(&posted)),
        );
        merge(
            &mut react,
            json!({
                "content": ":misskeycat:",
                "tag": [{
                    "type": "Emoji",
                    "name": ":misskeycat:",
                    "icon": {"type": "Image", "url": image},
                }],
            }),
        );
        react
    };
    deliver(&app, &state, &peer, BOB, &react).await;

    let domain =
        sqlx::query_scalar::<_, Option<String>>("SELECT emoji_domain FROM emoji_reactions")
            .fetch_one(&pool)
            .await
            .expect("the reaction was stored")
            .expect("a remote custom emoji records its domain");
    let name = format!("misskeycat@{domain}");
    let remembered = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM custom_emojis WHERE shortcode = 'misskeycat' AND domain = $1",
    )
    .bind(&domain)
    .fetch_one(&pool)
    .await
    .expect("the emoji table reads");
    assert_eq!(remembered, 0, "a reaction never files an emoji row");
    custom_emoji::upsert_remote(&pool, "misskeycat", Some(domain.as_str()), &image)
        .await
        .expect("a remote post would have taught the emoji");

    let path = format!("{REACTIONS}/{id}/reactions");
    let reactions = list(&app, Some(&alice_token), &path).await;
    assert_eq!(reactions.len(), 1, "one reaction landed");
    assert_eq!(reactions[0]["name"], json!(name), "named as Pleroma does");
    assert_eq!(reactions[0]["url"], json!(image), "with the remote image");

    let joined = common::TestRequest::put(&format!("{path}/{name}"))
        .bearer(&alice_token)
        .send(&app)
        .await;
    common::assert_answer(&joined, StatusCode::OK, "the author joins the reaction");
    let joined_reactions = list(&app, Some(&alice_token), &path).await;
    assert_eq!(
        joined_reactions.len(),
        1,
        "the two reactors share one entry"
    );
    assert_eq!(joined_reactions[0]["count"], json!(2), "counted together");
    assert_eq!(
        joined_reactions[0]["me"],
        json!(true),
        "and the author is one"
    );
}
