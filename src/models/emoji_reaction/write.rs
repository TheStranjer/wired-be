//! The reaction writes that record a reaction: the local one, the
//! federation mirror's under an explicit `ap_id`, and the lookup by that id.

use sqlx::PgPool;
use uuid::Uuid;

use super::{EmojiReaction, NewReaction};
use crate::{
    error::AppResult,
    util::id::{new_activity_ap_id, new_id},
};

/// [`react`]: the write, idempotent on `(user_id, status_id, emoji)`. A pair
/// that already reacted keeps its row — id, `ap_id`, `inserted_at` and so its
/// place in the roll-up's order — and only picks up the `url` the caller
/// brings, which is how a repeat reaction to a custom emoji whose image was
/// replaced shows the new image without becoming a new reaction. `COALESCE`
/// keeps the stored URL when the caller brought none.
const REACT_SQL: &str = "INSERT INTO emoji_reactions (
     id, user_id, status_id, emoji, url, ap_id
) VALUES (
     $1, $2, $3, $4, $5, $6
) ON CONFLICT (user_id, status_id, emoji) DO UPDATE SET
     url = COALESCE(EXCLUDED.url, emoji_reactions.url)
RETURNING *";

/// Records `reaction`, idempotently, and returns the stored row.
///
/// A pair that has already reacted keeps its row exactly as it is — same id,
/// same `ap_id`, same `inserted_at`, so the roll-up's order does not move — and
/// only updates the custom emoji's image URL when the caller brought one. The
/// row identity is the table's unique key, so an account holds at most one
/// reaction per status per emoji, and any number of different emojis.
///
/// `base_url` is the instance origin, i.e. [`Config::base_url`], which the
/// model layer is handed rather than reading configuration itself; the minted
/// `ap_id` is a fresh `{base_url}/activities/{uuid}` for this row's
/// `EmojiReact` activity (decision 6), which the federation ticket later
/// delivers.
///
/// [`Config::base_url`]: crate::config::Config::base_url
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError). The two foreign keys are
/// the failures reachable here: an unknown `user_id` or `status_id`.
pub async fn react(
    pool: &PgPool,
    base_url: &str,
    reaction: &NewReaction<'_>,
) -> AppResult<EmojiReaction> {
    react_with_ap_id(pool, reaction, &new_activity_ap_id(base_url)).await
}

/// [`react`]'s write under an explicit `ap_id`: the federation mirror's
/// door, where the row stands for the delivered `EmojiReact` activity.
///
/// Decision 6: a remote activity's id is whatever the remote sent, so its
/// own `Undo` names it back and [`get_by_ap_id`] finds the row by it.
///
/// [`get_by_ap_id`]: get_by_ap_id
///
/// # Errors
///
/// Those of [`react`].
pub async fn react_with_ap_id(
    pool: &PgPool,
    reaction: &NewReaction<'_>,
    ap_id: &str,
) -> AppResult<EmojiReaction> {
    let row = sqlx::query_as::<_, EmojiReaction>(REACT_SQL)
        .bind(new_id())
        .bind(reaction.user_id)
        .bind(reaction.status_id)
        .bind(reaction.emoji)
        .bind(reaction.url)
        .bind(ap_id)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The `SELECT` behind [`get_by_ap_id`]: the reaction of one account whose
/// `EmojiReact` activity carries one id. Scoping the lookup to the reactor
/// is the point, exactly as in [`crate::models::favourite::get_by_ap_id`]:
/// somebody else quoting the id finds nothing, so a `Undo` can only take
/// back its own maker's reaction.
const REACTION_BY_AP_ID_SQL: &str =
    "SELECT * FROM emoji_reactions WHERE user_id = $1 AND ap_id = $2";

/// The [`EmojiReaction`] of `user_id` whose `EmojiReact` activity carries
/// `ap_id`, `None` when the account has no such reaction.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_ap_id(
    pool: &PgPool,
    user_id: Uuid,
    ap_id: &str,
) -> AppResult<Option<EmojiReaction>> {
    let reaction = sqlx::query_as::<_, EmojiReaction>(REACTION_BY_AP_ID_SQL)
        .bind(user_id)
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(reaction)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        ACTIVITY_PREFIX, BASE, CAT, CAT_URL, HEART, THUMBS, instant, names, post, post_by,
        react_with, rollup, stamp, stored, summary, user,
    };
    use super::*;

    #[tokio::test]
    async fn a_reaction_is_written_and_read_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post_by(&pool, "bob").await;

        let row = react(
            &pool,
            BASE,
            &NewReaction {
                user_id: alice.id,
                status_id,
                emoji: THUMBS,
                url: None,
            },
        )
        .await
        .expect("the first reaction stores");

        assert_eq!(row.user_id, alice.id, "who reacted");
        assert_eq!(row.status_id, status_id, "what they reacted to");
        assert_eq!(row.emoji, THUMBS, "with what");
        assert_eq!(row.url, None, "a unicode reaction carries no image");
        let ap_id = row.ap_id.expect("a local reaction mints an activity id");
        let activity = ap_id
            .strip_prefix(&format!("{BASE}{ACTIVITY_PREFIX}"))
            .unwrap_or_default();
        assert!(
            crate::util::id::parse_id(activity).is_some(),
            "the id is this instance's origin, `/activities/` and a uuid, \
             not {ap_id}"
        );
        assert_eq!(
            stored(&pool).await,
            vec![(alice.id, status_id, THUMBS.to_owned(), None)],
            "one row"
        );
    }

    #[tokio::test]
    async fn reacting_twice_is_one_row_that_keeps_its_mint() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post_by(&pool, "bob").await;
        let first = react(
            &pool,
            BASE,
            &NewReaction {
                user_id: alice.id,
                status_id,
                emoji: CAT,
                url: Some(CAT_URL),
            },
        )
        .await
        .expect("the first reaction stores");

        let again = react(
            &pool,
            BASE,
            &NewReaction {
                user_id: alice.id,
                status_id,
                emoji: CAT,
                url: Some("https://test.example/emoji/cats/new.png"),
            },
        )
        .await
        .expect("repeating a reaction is not an error");

        assert_eq!(again.id, first.id, "the pair keeps its one row");
        assert_eq!(again.ap_id, first.ap_id, "and its activity id");
        assert_eq!(
            again.inserted_at, first.inserted_at,
            "and the time that orders the roll-up"
        );
        assert_eq!(
            again.url.as_deref(),
            Some("https://test.example/emoji/cats/new.png"),
            "the image URL the repeat brought is picked up"
        );
        assert_eq!(
            stored(&pool).await,
            vec![(
                alice.id,
                status_id,
                CAT.to_owned(),
                Some("https://test.example/emoji/cats/new.png".to_owned())
            )],
            "still one row in the table"
        );

        react_with(&pool, &alice, status_id, THUMBS, None).await;
        let both = rollup(&pool, status_id, Some(&alice)).await;
        assert_eq!(
            names(&both),
            vec![CAT, THUMBS],
            "a second emoji is a second row, ordered by its own first reaction"
        );
        assert_eq!(
            both[0].url.as_deref(),
            Some("https://test.example/emoji/cats/new.png"),
            "the custom emoji shows the image its latest write stored"
        );
        assert_eq!(
            both[1].url, None,
            "and the unicode one stays without an image"
        );
    }

    /// The mirror's writes: [`react_with_ap_id`] stores the delivered
    /// activity id as the row's own, a repeat of the triple under a new id
    /// keeps the first row's id (and only refreshes the image), and
    /// [`get_by_ap_id`] reads the row by that id — for its maker only.
    #[tokio::test]
    async fn a_mirrored_reaction_keeps_the_delivered_activity_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "mirror-alice").await;
        let carol = user(&pool, "mirror-carol").await;
        let status_id = post_by(&pool, "mirror-bob").await;

        let stored = react_with_ap_id(
            &pool,
            &NewReaction {
                user_id: alice.id,
                status_id,
                emoji: CAT,
                url: Some(CAT_URL),
            },
            "https://peer.example/activities/react-1",
        )
        .await
        .expect("the mirror write stores");
        assert_eq!(
            stored.ap_id.as_deref(),
            Some("https://peer.example/activities/react-1"),
            "the row carries the delivered id, not a minted one"
        );

        let restated = react_with_ap_id(
            &pool,
            &NewReaction {
                user_id: alice.id,
                status_id,
                emoji: CAT,
                url: Some("https://test.example/emoji/cats/new.png"),
            },
            "https://peer.example/activities/react-2",
        )
        .await
        .expect("restating a reaction is not an error");
        assert_eq!(restated.id, stored.id, "the triple keeps its one row");
        assert_eq!(
            restated.ap_id.as_deref(),
            Some("https://peer.example/activities/react-1"),
            "and the first delivery's id"
        );
        assert_eq!(
            restated.url.as_deref(),
            Some("https://test.example/emoji/cats/new.png"),
            "while the image the repeat brought is picked up"
        );

        let found = get_by_ap_id(&pool, alice.id, "https://peer.example/activities/react-1")
            .await
            .expect("the activity-id read runs")
            .expect("the id names the row");
        assert_eq!(found.id, stored.id, "the maker finds its own reaction");
        assert!(
            get_by_ap_id(&pool, carol.id, "https://peer.example/activities/react-1")
                .await
                .expect("the activity-id read runs")
                .is_none(),
            "and another account quoting it names none of its own"
        );
    }

    #[tokio::test]
    async fn reactions_are_per_account_per_status_and_per_emoji() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let first = post(&pool, &carol).await;
        let second = post(&pool, &carol).await;

        react_with(&pool, &alice, first, THUMBS, None).await;
        react_with(&pool, &bob, first, THUMBS, None).await;
        react_with(&pool, &alice, first, HEART, None).await;
        react_with(&pool, &alice, second, THUMBS, None).await;
        stamp(
            &pool,
            first,
            &alice,
            THUMBS,
            instant("2024-01-01T00:00:00.000Z"),
        )
        .await;
        stamp(
            &pool,
            first,
            &alice,
            HEART,
            instant("2024-02-01T00:00:00.000Z"),
        )
        .await;

        assert_eq!(
            rollup(&pool, first, Some(&alice)).await,
            vec![
                summary(THUMBS, 2, true, None, &[alice.id, bob.id]),
                summary(HEART, 1, true, None, &[alice.id]),
            ],
            "the first status counts both 👍 reactors and alice's heart"
        );
        assert_eq!(
            rollup(&pool, second, Some(&bob)).await,
            vec![summary(THUMBS, 1, false, None, &[alice.id])],
            "the second holds only alice's reaction, and bob is not in it"
        );
        assert_eq!(
            rollup(&pool, first, None).await,
            vec![
                summary(THUMBS, 2, false, None, &[alice.id, bob.id]),
                summary(HEART, 1, false, None, &[alice.id]),
            ],
            "an anonymous viewer is never `me`"
        );
    }
}
