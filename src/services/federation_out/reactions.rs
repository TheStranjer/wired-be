//! The `EmojiReact` arm of the interaction hooks: a custom or unicode
//! reaction's activity and the `Undo` that takes it back, under the like's
//! addressing minus the public address (Pleroma's `react_with_emoji`).
//!
//! The shared rows, addressing and publisher hand stand in the module root;
//! the like twin is [`super::likes`].

use super::{followers_address, queue_activity, status_action, withdrawal};
use crate::{
    federation::activities::{EmojiReactContext, emoji_react_json},
    models::emoji_reaction::EmojiReaction,
    state::AppState,
};

/// Announce the reaction or its withdrawal.
///
/// The document is the `EmojiReact` behind the row — `content` the stored
/// emoji, a custom one also carrying its image as an `Emoji` `tag` —
/// under the same addressing [`like_json`] is sent with, minus the public
/// address (Pleroma's `react_with_emoji` arm), and the `Undo` repeating it
/// when the reaction is taken back.
///
/// The reaction's activity id is the row's stored `ap_id` (decision 6);
/// the same id-less, gone-row and non-local-actor refusals as
/// [`after_favourite_changed`] answer with silence. A repeat `PUT` fires
/// the hook again on the unchanged row, and the per-activity, per-inbox
/// deduplication of the delivery queue is what keeps the restated fact a
/// single sending.
///
/// [`after_favourite_changed`]: super::after_favourite_changed
/// [`like_json`]: crate::federation::activities::like_json
pub async fn after_reaction_changed(state: &AppState, reaction: &EmojiReaction, reacting: bool) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(react_id) = reaction.ap_id.as_deref() else {
        return; // an id-less row has no document to stand behind
    };
    let Some(action) = status_action(state, reaction.user_id, reaction.status_id).await else {
        return;
    };
    let followers = followers_address(&action.actor);
    let to = [action.author.ap_id.as_str()];
    let cc: Vec<&str> = if action.public {
        vec![followers.as_str()]
    } else {
        Vec::new()
    };
    let document = emoji_react_json(&EmojiReactContext {
        config: &state.config,
        id: react_id,
        actor_ap_id: &action.actor.ap_id,
        object_uri: &action.status.uri,
        to: &to,
        cc: &cc,
        emoji: &reaction.emoji,
        emoji_url: reaction.url.as_deref(),
        published: &reaction.inserted_at,
    });
    let document = if reacting {
        document
    } else {
        withdrawal(state, &action.actor.ap_id, &to, &cc, &document)
    };
    queue_activity(state, &action.actor, &document, "reaction").await;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        models::{
            emoji_reaction::{self, NewReaction},
            status::Visibility,
        },
        services::federation_out::test_support::{
            accepted_follow, app_state, delivered_inboxes, local, remote, remote_status,
            run_publishes, the_single_activity,
        },
    };

    /// A reaction on a public REMOTE status publishes the `EmojiReact`
    /// with the stored emoji in `content`, the custom emoji's image as an
    /// `Emoji` `tag`, the author in `to` and the followers collection (no
    /// public address) in `cc`.
    #[tokio::test]
    async fn a_custom_reaction_on_a_remote_status_publishes_the_tagged_emoji_react() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let reacted = remote_status(&pool, &bob, "react-target", Visibility::Public).await;
        let emoji_url = "https://test.example/emoji/railroad_track.png";
        let new = NewReaction {
            user_id: alice.id,
            status_id: reacted.id,
            emoji: ":railroad_track:",
            url: Some(emoji_url),
        };
        let row = emoji_reaction::react(&pool, TEST_BASE_URL, &new)
            .await
            .expect("the reaction writes");
        let state = app_state(&pool);

        after_reaction_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "EmojiReact");
        assert_eq!(
            activity["id"],
            row.ap_id.expect("a local reaction carries its id"),
            "the document id is the reaction row's stored ap_id"
        );
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"], reacted.uri);
        assert_eq!(
            activity["content"], ":railroad_track:",
            "the shortcode as content"
        );
        assert_eq!(
            activity["tag"],
            json!([{
                "type": "Emoji",
                "id": emoji_url,
                "name": ":railroad_track:",
                "icon": { "type": "Image", "url": emoji_url },
            }]),
            "the custom emoji rides as the note shape's Emoji tag"
        );
        assert_eq!(activity["to"], json!([bob.ap_id]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)]),
            "followers without the public address"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![
                "https://remote.example/inbox",
                "https://remote.example/users/bob/inbox",
            ],
            "the author's inbox plus the followers' instance"
        );
    }

    /// A plain unicode reaction carries `content` and no `tag` at all.
    #[tokio::test]
    async fn a_unicode_reaction_carries_no_tag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let reacted = remote_status(&pool, &bob, "react-unicode", Visibility::Public).await;
        let new = NewReaction {
            user_id: alice.id,
            status_id: reacted.id,
            emoji: "🎉",
            url: None,
        };
        let row = emoji_reaction::react(&pool, TEST_BASE_URL, &new)
            .await
            .expect("the reaction writes");
        let state = app_state(&pool);

        after_reaction_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "EmojiReact");
        assert_eq!(activity["content"], "🎉");
        assert!(
            activity.get("tag").is_none(),
            "a unicode reaction names no custom emoji to tag"
        );
    }

    /// Taking a reaction back publishes the `Undo` that embeds the stored
    /// `EmojiReact` and repeats its addressing.
    #[tokio::test]
    async fn unreacting_publishes_the_undo_of_the_reaction() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let reacted = remote_status(&pool, &bob, "react-undo", Visibility::Public).await;
        let new = NewReaction {
            user_id: alice.id,
            status_id: reacted.id,
            emoji: "🍺",
            url: None,
        };
        let created = emoji_reaction::react(&pool, TEST_BASE_URL, &new)
            .await
            .expect("the reaction writes");
        let stored_id = created.ap_id.expect("a local reaction carries its id");
        let removed = emoji_reaction::unreact(&pool, alice.id, reacted.id, "🍺")
            .await
            .expect("the withdrawal runs")
            .expect("the reaction went");
        let state = app_state(&pool);

        after_reaction_changed(&state, &removed, false).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Undo");
        assert_eq!(activity["object"]["type"], "EmojiReact");
        assert_eq!(
            activity["object"]["id"], stored_id,
            "the embedded reaction keeps the row's stored id"
        );
        assert_eq!(activity["object"]["content"], "🍺");
        let id = activity["id"].as_str().expect("the undo has an id");
        assert!(id.starts_with(&format!("{TEST_BASE_URL}/activities/")));
        assert_ne!(id, stored_id, "the wrapper stands under a fresh id");
        assert_eq!(activity["to"], json!([bob.ap_id]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)]),
            "the withdrawal repeats the reaction's cc line"
        );
    }
}
