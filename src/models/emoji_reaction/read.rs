//! The reaction roll-ups a status is rendered with: one batch for a page
//! of statuses, and the full reactor lists of one status.

use std::collections::HashMap;

use sqlx::PgPool;
use uuid::Uuid;

use super::{
    MAX_SUMMARY_REACTORS, ReactionSummary,
    groups::{ReactionRow, ReactorGroup, group},
};
use crate::error::AppResult;

/// [`for_statuses`]: one aggregated row per `(status, emoji)` of a batch of
/// statuses (`$1`), computed in the database so a status with thousands of
/// reactions still costs one small row per emoji. `$2` is the viewer (`NULL`:
/// nobody, so `me` is false), `$3` the most reactor ids to list. `url` is the
/// image of the emoji's earliest reaction that has one, `account_ids` the
/// earliest reactors (reaction time, then row id, for a stable order) and
/// `first_at` the emoji's first reaction time, which together with the emoji
/// text orders a status's groups the way the roll-up promises.
pub(super) const FOR_STATUSES_SQL: &str = "SELECT status_id, emoji,
        (array_agg(url ORDER BY inserted_at) FILTER (WHERE url IS NOT NULL))[1] AS url,
        count(*) AS count,
        COALESCE(bool_or(user_id = $2::uuid), false) AS me,
        (array_agg(user_id ORDER BY inserted_at, id))[1:$3::int4] AS account_ids,
        min(inserted_at) AS first_at
    FROM emoji_reactions
   WHERE status_id = ANY($1)
GROUP BY status_id, emoji
ORDER BY status_id, first_at, emoji";

/// One fetched row of [`FOR_STATUSES_SQL`]: the roll-up of one emoji on one
/// status. `first_at` is only ordered by and is not read.
#[derive(Debug, sqlx::FromRow)]
struct SummaryRow {
    /// The reacted-to status, which keys the result.
    status_id: Uuid,
    /// The emoji as stored.
    emoji: String,
    /// The custom emoji's image URL, if any.
    url: Option<String>,
    /// How many accounts reacted with this emoji.
    count: i64,
    /// Whether the viewer is among them.
    me: bool,
    /// The earliest reactors, at most [`MAX_SUMMARY_REACTORS`].
    account_ids: Vec<Uuid>,
}

impl From<SummaryRow> for ReactionSummary {
    /// The summary of `row`, its status key dropped.
    fn from(row: SummaryRow) -> Self {
        Self {
            name: row.emoji,
            count: row.count,
            me: row.me,
            url: row.url,
            account_ids: row.account_ids,
        }
    }
}

/// [`reactors`]: one status's reactions, optionally narrowed to the single
/// emoji `emoji` names (`$2` is that emoji or `NULL` for all of them), in the
/// same order [`for_statuses`] uses, and for the same reason.
pub(super) const REACTORS_SQL: &str = "SELECT r.status_id, r.emoji, r.url, r.user_id,
        min(r.inserted_at) OVER (PARTITION BY r.status_id, r.emoji) AS first_at
    FROM emoji_reactions r
   WHERE r.status_id = $1 AND ($2::text IS NULL OR r.emoji = $2::text)
ORDER BY first_at, r.emoji, r.inserted_at, r.id";

/// The reaction roll-ups of every status in `status_ids`, keyed by status.
///
/// One [`ReactionSummary`] per emoji the status has reactions with, ordered by
/// the emoji's first reaction time and then by the emoji text.
///
/// This is the batched read the render path is built on
/// (`docs/ARCHITECTURE.md` decision 13): one statement for a whole page of
/// statuses whatever its size, so
/// [`crate::services::render_status`] adds exactly one query to its fixed set,
/// with `viewer_id` riding along to answer each summary's `me` (`None`: an
/// anonymous page, where nobody is `me`). A status nobody reacted to has no
/// entry at all, and an empty `status_ids` asks the database nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn for_statuses(
    pool: &PgPool,
    status_ids: &[Uuid],
    viewer_id: Option<Uuid>,
) -> AppResult<HashMap<Uuid, Vec<ReactionSummary>>> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, SummaryRow>(FOR_STATUSES_SQL)
        .bind(status_ids)
        .bind(viewer_id)
        .bind(i32::try_from(MAX_SUMMARY_REACTORS).unwrap_or(i32::MAX))
        .fetch_all(pool)
        .await?;
    let mut rollups: HashMap<Uuid, Vec<ReactionSummary>> = HashMap::new();
    for row in rows {
        rollups
            .entry(row.status_id)
            .or_default()
            .push(ReactionSummary::from(row));
    }
    Ok(rollups)
}

/// Every reaction of `status_id` as one [`ReactorGroup`] per emoji, in the same
/// order [`for_statuses`] uses, optionally narrowed to the single emoji `emoji`
/// names (`None`: all of them).
///
/// Unlike the batched roll-up this keeps the whole reactor list, because the
/// endpoint it serves renders those accounts, and it knows nothing about the
/// viewer: who may see which reactor is the endpoint's filter (Pleroma's
/// `filter_allowed_users`, which drops the viewer's blocked accounts and —
/// unless the client asked for them — the muted ones).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn reactors(
    pool: &PgPool,
    status_id: Uuid,
    emoji: Option<&str>,
) -> AppResult<Vec<ReactorGroup>> {
    let rows = sqlx::query_as::<_, ReactionRow>(REACTORS_SQL)
        .bind(status_id)
        .bind(emoji)
        .fetch_all(pool)
        .await?;
    Ok(group(rows)
        .into_iter()
        .map(|(_, reactions)| reactions)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::super::{
        MAX_SUMMARY_REACTORS,
        test_support::{
            CAT, CAT_URL, HEART, THUMBS, post, post_by, react_with, rollup, summary, user,
        },
    };
    use super::*;

    #[tokio::test]
    async fn the_reactor_list_is_capped_but_the_count_is_not() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let status_id = post_by(&pool, "author").await;
        let mut reactors = Vec::new();
        for n in 0..8 {
            let reactor = user(&pool, &format!("reactor{n}")).await;
            react_with(&pool, &reactor, status_id, THUMBS, None).await;
            reactors.push(reactor);
        }

        let summaries = rollup(&pool, status_id, Some(&reactors[7])).await;
        assert_eq!(summaries.len(), 1, "one emoji, one entry");
        assert_eq!(summaries[0].count, 8, "all eight are counted");
        assert_eq!(
            summaries[0].account_ids.len(),
            MAX_SUMMARY_REACTORS,
            "and only the first {MAX_SUMMARY_REACTORS} are listed"
        );
        let first_five: Vec<Uuid> = reactors.iter().take(5).map(|who| who.id).collect();
        assert_eq!(
            summaries[0].account_ids, first_five,
            "the listed ids are the earliest reactors, in reaction order"
        );
        assert!(summaries[0].me, "the eighth reactor is still `me`");
    }

    #[tokio::test]
    async fn seven_reactors_are_counted_and_five_listed_in_reaction_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let status_id = post_by(&pool, "author").await;
        let bystander = user(&pool, "bystander").await;
        let mut reactors = Vec::new();
        for n in 0..7 {
            let reactor = user(&pool, &format!("reactor{n}")).await;
            react_with(&pool, &reactor, status_id, CAT, Some(CAT_URL)).await;
            reactors.push(reactor);
        }

        let summaries = rollup(&pool, status_id, Some(&reactors[6])).await;
        assert_eq!(summaries.len(), 1, "one row for the (status, emoji) pair");
        assert_eq!(summaries[0].count, 7, "all seven are counted");
        assert_eq!(
            summaries[0].account_ids,
            reactors
                .iter()
                .take(5)
                .map(|who| who.id)
                .collect::<Vec<_>>(),
            "five ids, the earliest reactors, in reaction order"
        );
        assert_eq!(
            summaries[0].url.as_deref(),
            Some(CAT_URL),
            "the image survives the aggregation"
        );
        assert!(summaries[0].me, "the unlisted seventh reactor is `me`");
        assert!(
            !rollup(&pool, status_id, Some(&bystander)).await[0].me,
            "a viewer who did not react is not `me`"
        );
        assert!(
            !rollup(&pool, status_id, None).await[0].me,
            "an anonymous viewer is never `me`"
        );
    }

    #[tokio::test]
    async fn a_page_rolls_up_every_status_at_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = post(&pool, &bob).await;
        let second = post(&pool, &bob).await;
        let third = post(&pool, &bob).await;
        react_with(&pool, &alice, second, CAT, Some(CAT_URL)).await;
        react_with(&pool, &alice, third, HEART, None).await;

        let rollups = for_statuses(&pool, &[first, second, third], Some(alice.id))
            .await
            .expect("the page rolls up");
        assert_eq!(rollups.len(), 2, "the unreacted status has no entry");
        assert!(
            !rollups.contains_key(&first),
            "`first` was reacted to by nobody"
        );
        assert_eq!(
            rollups[&second],
            vec![summary(CAT, 1, true, Some(CAT_URL), &[alice.id])],
            "the second status carries its custom reaction with its image"
        );
        assert_eq!(
            rollups[&third],
            vec![summary(HEART, 1, true, None, &[alice.id])],
            "and the third only its own"
        );
        assert_eq!(
            for_statuses(&pool, &[], Some(alice.id))
                .await
                .expect("an empty page reads"),
            HashMap::new(),
            "an empty id list asks nothing and answers nothing"
        );
    }
}
