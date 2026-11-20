//! The emoji-reaction tellings of [`crate::services::notify`]: the
//! author's `pleroma:emoji_reaction` and its per-emoji withdrawal. The
//! shared creation plumbing these calls live in the parent module.

use tracing::warn;

use crate::{
    error::AppResult,
    models::{
        emoji_reaction::EmojiReaction as ReactionRow,
        notification::{self, NewNotification, TYPE_EMOJI_REACTION, Telling},
        status, user,
    },
    state::AppState,
};

use super::announce;

/// The reaction routes' notification, landed from `after_reaction_changed`
/// in [`crate::api::emoji_reactions`].
///
/// A reaction tells the status's author `pleroma:emoji_reaction` with the
/// emoji stored on the row (Pleroma's `EmojiReact` arm: the potential
/// receiver is the object's actor alone), and the withdrawal takes exactly
/// that telling back — the one emoji's row, so a second reaction with a
/// different emoji on the same status stands. The repeat `PUT` an
/// idempotent write answers again is deduplicated on the full key
/// ([`notification::already_told`]); a `DELETE` that found no reaction
/// never reaches the hook, and a deleted status has lost its
/// notifications to [`notification::delete_for_status`] anyway.
pub async fn on_reaction_changed(state: &AppState, reaction: &ReactionRow, reacting: bool) {
    if let Err(err) = reaction_telling(state, reaction, reacting).await {
        warn!(
            status = %reaction.status_id,
            emoji = %reaction.emoji,
            %err,
            "could not tell a reacted post's author about the reaction"
        );
    }
}

/// [`on_reaction_changed`]'s work, create and withdraw arms.
async fn reaction_telling(
    state: &AppState,
    reaction: &ReactionRow,
    reacting: bool,
) -> AppResult<()> {
    let Some(reacted) = status::get_by_id(&state.db, reaction.status_id).await? else {
        return Ok(());
    };
    let telling = Telling {
        user_id: reacted.user_id,
        from_account_id: reaction.user_id,
        notification_type: TYPE_EMOJI_REACTION,
        status_id: Some(reaction.status_id),
        emoji: Some(reaction.emoji.as_str()),
    };
    if !reacting {
        return notification::delete_telling(&state.db, &telling)
            .await
            .map(drop);
    }
    if notification::already_told(&state.db, &telling).await? {
        return Ok(());
    }
    let Some(author) = user::get_by_id(&state.db, reacted.user_id).await? else {
        return Ok(());
    };
    if author.id == reaction.user_id {
        return Ok(());
    }
    announce(
        state,
        &author,
        &NewNotification {
            user_id: author.id,
            from_account_id: reaction.user_id,
            notification_type: TYPE_EMOJI_REACTION,
            status_id: Some(reaction.status_id),
            emoji: Some(reaction.emoji.as_str()),
            report_id: None,
            chat_message_id: None,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::emoji_reaction::{self, NewReaction},
        services::notify::test_support::{BASE, app_state, local, post, reacted, remote, rows},
    };

    #[tokio::test]
    async fn a_reaction_tells_the_author_per_emoji_and_the_undo_retires_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "rc-alice").await;
        let bob = local(pool, "rc-bob").await;

        let posted = post(pool, &alice, "react to me").await;
        reacted(&state, pool, &bob, &posted, "👍").await;
        let notes = rows(pool, alice.id).await;
        assert_eq!(notes.len(), 1, "the reaction is told to the author");
        let note = notes.first().expect("the fixture telling");
        assert_eq!(note.event_type, TYPE_EMOJI_REACTION);
        assert_eq!(note.from_account_id, bob.id);
        assert_eq!(note.status_id, Some(posted.id));
        assert_eq!(
            note.emoji.as_deref(),
            Some("👍"),
            "the emoji rides in the row"
        );

        // The idempotent `PUT` re-fires the hook on the same fact: the
        // deduplication keeps it one row.
        let again = emoji_reaction::react(
            pool,
            BASE,
            &NewReaction {
                user_id: bob.id,
                status_id: posted.id,
                emoji: "👍",
                url: None,
            },
        )
        .await
        .expect("the repeat reaction restates the row");
        on_reaction_changed(&state, &again, true).await;
        assert_eq!(
            rows(pool, alice.id).await.len(),
            1,
            "a restated reaction tells once"
        );

        // A second emoji is a second fact.
        reacted(&state, pool, &bob, &posted, "🎉").await;
        assert_eq!(
            rows(pool, alice.id).await.len(),
            2,
            "two emojis on one status are two tellings"
        );

        // And the withdrawal takes exactly its own emoji back.
        let removed = emoji_reaction::unreact(pool, bob.id, posted.id, "👍")
            .await
            .expect("the withdrawal runs")
            .expect("the reaction was there");
        on_reaction_changed(&state, &removed, false).await;
        let left = rows(pool, alice.id).await;
        assert_eq!(left.len(), 1, "the other emoji's telling stands");
        assert_eq!(
            left.first().expect("the standing telling").emoji.as_deref(),
            Some("🎉"),
            "and it is the one that was not withdrawn"
        );
    }

    #[tokio::test]
    async fn a_self_or_remote_reaction_tells_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let bob = local(pool, "rs-bob").await;
        let mallory = remote(pool, "rs-mallory", "remote.example").await;

        let own = post(pool, &bob, "my own post").await;
        reacted(&state, pool, &bob, &own, "👍").await;
        assert!(
            rows(pool, bob.id).await.is_empty(),
            "reacting to your own post tells nobody"
        );

        let theirs = post(pool, &mallory, "their post").await;
        reacted(&state, pool, &bob, &theirs, "👍").await;
        assert!(
            rows(pool, mallory.id).await.is_empty(),
            "a remote author is not ours to tell"
        );

        // A withdrawal of a reaction that never existed leaves the hook
        // nothing to retire and no error to report.
        let untouched = emoji_reaction::unreact(pool, bob.id, own.id, "🎉")
            .await
            .expect("the withdrawal runs");
        assert!(untouched.is_none(), "there was no row to remove");
    }
}
