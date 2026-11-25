//! Inbound custom-emoji reactions, updates and deletes end to end.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    BOB, REACTIONS, STATUSES, activity, create_note, deliver, entry_for, get, list, local, lookup,
    merge, note_object, post_status, public_timeline, told_of, uri_of,
};
use crate::{common, remote};

/// Scenario: a remote custom emoji through the reaction path. A delivered
/// `EmojiReact` naming a `:shortcode:` with its own `Emoji` tag lands in the
/// reactions read carrying the remote image url, and tells the author.
#[tokio::test]
async fn a_remote_custom_emoji_reaction_carries_its_image_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ei-emoji-alice").await;
    let posted = post_status(
        &app,
        &alice_token,
        json!({"status": "react with a custom emoji"}),
    )
    .await;
    let actor = peer.actor(BOB);
    let ralph = format!("{}/emoji/ralph.png", peer.origin());

    let react = {
        let mut react = activity(
            &peer,
            "EmojiReact",
            "emoji-react",
            &actor,
            json!(uri_of(&posted)),
        );
        merge(
            &mut react,
            json!({
                "content": ":ralph:",
                "to": [alice.ap_id],
                "tag": [{"type": "Emoji", "name": ":ralph:", "icon": {"type": "Image", "url": ralph}}],
            }),
        );
        react
    };
    deliver(&app, &state, &peer, BOB, &react).await;

    let id = common::id_of(&posted);
    let reactions = list(
        &app,
        Some(&alice_token),
        &format!("{REACTIONS}/{id}/reactions"),
    )
    .await;
    assert_eq!(reactions.len(), 1, "one reaction landed");
    let domain =
        sqlx::query_scalar::<_, Option<String>>("SELECT emoji_domain FROM emoji_reactions")
            .fetch_one(&pool)
            .await
            .expect("the reaction was stored")
            .expect("a remote custom emoji records its domain");
    assert_eq!(
        reactions[0]["name"],
        json!(format!("ralph@{domain}")),
        "as shortcode@domain, the reactor's instance"
    );
    assert_eq!(
        reactions[0]["url"],
        json!(ralph),
        "carrying the delivered emoji's own image"
    );
    let inline = get(&app, Some(&alice_token), &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        inline["pleroma"]["emoji_reactions"][0]["url"],
        json!(ralph),
        "and rendering it inline on the status"
    );
    assert_eq!(
        told_of(&app, &alice_token, "pleroma:emoji_reaction")
            .await
            .len(),
        1
    );
}

/// Scenario: an `Update` of a delivered note edits it and leaves the client's
/// `history` with the superseded version, and a later `Update` restating the
/// same version records nothing further.
#[tokio::test]
async fn an_update_of_a_remote_note_edits_it_and_keeps_the_history() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    let uri = peer.note("edit-1");
    let original = create_note(&peer, &actor, "edit-1", "<p>first draft</p>", json!({}));
    deliver(&app, &state, &peer, BOB, &original).await;
    let public = public_timeline(&app).await;
    let id = common::id_of(entry_for(&public, &uri));

    let first_edit = note_object(
        &peer,
        &actor,
        "edit-1",
        "<p>second draft</p>",
        json!({"updated": "2024-05-01T12:00:00.000Z"}),
    );
    let update1 = activity(&peer, "Update", "edit-1-u1", &actor, first_edit);
    deliver(&app, &state, &peer, BOB, &update1).await;

    let shown = get(&app, None, &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        shown["content"],
        json!("<p>second draft</p>"),
        "the edit rewrote the body"
    );
    assert_ne!(shown["edited_at"], Value::Null, "and stamped the edit");
    let history = list(&app, None, &format!("{STATUSES}/{id}/history")).await;
    assert_eq!(
        history.len(),
        2,
        "the superseded version beside the current one"
    );
    assert_eq!(
        history[0]["content"],
        json!("<p>first draft</p>"),
        "oldest first"
    );

    let restated = note_object(
        &peer,
        &actor,
        "edit-1",
        "<p>second draft again</p>",
        json!({"updated": "2024-05-01T11:00:00.000Z"}),
    );
    let update2 = activity(&peer, "Update", "edit-1-u2", &actor, restated);
    deliver(&app, &state, &peer, BOB, &update2).await;
    let after = get(&app, None, &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        after["content"],
        json!("<p>second draft</p>"),
        "a stale update rewrote nothing"
    );
    assert_eq!(
        list(&app, None, &format!("{STATUSES}/{id}/history"))
            .await
            .len(),
        2,
        "and recorded no third version"
    );
}

/// Scenario: a `Delete` of a delivered reply takes it down and steps its
/// parent's counter back, and a `Create` of the same uri arriving after the
/// delete is ignored — no resurrection, and the author's post count unmoved.
#[tokio::test]
async fn a_delete_of_a_remote_note_takes_it_down_and_ignores_a_late_create() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    let parent_uri = peer.note("del-parent");
    let child_uri = peer.note("del-child");
    let parent = create_note(&peer, &actor, "del-parent", "<p>the parent</p>", json!({}));
    let child = create_note(
        &peer,
        &actor,
        "del-child",
        "<p>the answer</p>",
        json!({"inReplyTo": parent_uri}),
    );
    deliver(&app, &state, &peer, BOB, &parent).await;
    deliver(&app, &state, &peer, BOB, &child).await;

    let public = public_timeline(&app).await;
    let parent_entry = entry_for(&public, &parent_uri);
    let parent_id = common::id_of(parent_entry);
    assert_eq!(
        parent_entry["replies_count"],
        json!(1),
        "the child counted on its parent"
    );
    let child_id = common::id_of(entry_for(&public, &child_uri));

    let delete = activity(&peer, "Delete", "del-act", &actor, json!(child_uri));
    deliver(&app, &state, &peer, BOB, &delete).await;
    assert_eq!(
        get(&app, None, &format!("{STATUSES}/{child_id}"))
            .await
            .status,
        StatusCode::NOT_FOUND,
        "the deleted reply is gone from the reads"
    );
    let after_delete = get(&app, None, &format!("{STATUSES}/{parent_id}"))
        .await
        .json_value();
    assert_eq!(
        after_delete["replies_count"],
        json!(0),
        "and its counter stepped back down"
    );
    let count_after_delete = lookup(&app, &peer.handle(BOB)).await["statuses_count"].clone();

    let late = create_note(&peer, &actor, "del-child", "<p>too late</p>", json!({}));
    deliver(&app, &state, &peer, BOB, &late).await;
    assert_eq!(
        get(&app, None, &format!("{STATUSES}/{child_id}"))
            .await
            .status,
        StatusCode::NOT_FOUND,
        "the Create of a deleted uri is ignored — no resurrection"
    );
    let count_after_late = lookup(&app, &peer.handle(BOB)).await["statuses_count"].clone();
    assert_eq!(
        count_after_late, count_after_delete,
        "and the ignored create left the author's post count unchanged"
    );
}
