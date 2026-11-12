//! The `emoji_reactions` table: one account reacting to one status with one
//! emoji.
//!
//! Reference: Pleroma's `EmojiReact` activities
//! (`lib/pleroma/web/common_api.ex`, `react_with_emoji` and
//! `unreact_with_emoji`), which it stores as an activity per reaction plus an
//! `Undo` per withdrawal and reads back out of the reacted-to object's
//! `reactions` JSONB (`Object.get_emoji_reactions`, `lib/pleroma/object.ex`).
//! This table is that fact laid out relationally
//! (`docs/ARCHITECTURE.md` decision 3): one row per `(user_id, status_id,
//! emoji)`, the emoji's image URL beside it instead of Pleroma's activity
//! `tag` list.
//!
//! [`react`] and [`unreact`] are both idempotent on that key — reacting twice
//! is one row that keeps its id, its `ap_id` and the insertion time that
//! orders the roll-up, and withdrawing a reaction that is not there (or no
//! longer is) removes nothing — the same pair of moves
//! [`crate::models::block`] makes. What a reaction *may* name (one emoji
//! grapheme, or a shortcode this instance has) is validated before it gets
//! here: see [`crate::util::emoji`] and the endpoint that resolves a shortcode
//! against [`crate::models::custom_emoji`].
//!
//! The two reads are both batched by design because the Status entity needs a
//! reaction roll-up per rendered status: [`for_statuses`] answers every emoji
//! of a whole page of statuses in one statement, keyed by status, and
//! [`reactors`] is its single-status counterpart for the reactions endpoint,
//! which also hands the reactors themselves over. Both keep the same order —
//! an emoji's *first* reaction time, then the emoji text — so a status's array
//! reads the same wherever it is rendered.
//!
//! Deleting a user or a status takes its reactions through the foreign key's
//! cascade, and [`delete_all_for_user`] is the sweep for the accounts whose
//! row survives deletion. The [`EmojiReaction::ap_id`] awaits the federation
//! ticket that sends the `EmojiReact` activity (decision 6); [`react`] mints
//! a local one.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

/// How many reactors one `pleroma.emoji_reactions` entry names.
///
/// Pleroma's status renderer lists every reactor's id there; this instance
/// keeps the summary short and leaves the full list to
/// [`GET …/reactions`](crate::api::emoji_reactions), which renders the
/// accounts themselves.
pub const MAX_SUMMARY_REACTORS: usize = 5;

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

/// [`unreact`]: one account's one emoji on one status; a pair that never
/// reacted (or already withdrew) deletes nothing, so withdrawal is idempotent.
/// `RETURNING *` hands the caller the row that went, which is what the write
/// hook reports.
const UNREACT_SQL: &str = "DELETE FROM emoji_reactions
      WHERE user_id = $1 AND status_id = $2 AND emoji = $3
   RETURNING *";

/// [`for_statuses`]: every reaction of a batch of statuses, one row per
/// reaction. `first_at` is the earliest reaction *of the same emoji on the
/// same status*, so ordering by it and then by the emoji text puts the groups
/// in the order the roll-up promises and keeps every row of one group next to
/// its own — which is what [`group`] relies on. The reaction time and then the
/// row id break ties inside a group, making the reactor order stable. Only the
/// four columns [`ReactionRow`] names are read; `first_at` exists to be
/// ordered by.
const FOR_STATUSES_SQL: &str = "SELECT r.status_id, r.emoji, r.url, r.user_id,
        min(r.inserted_at) OVER (PARTITION BY r.status_id, r.emoji) AS first_at
    FROM emoji_reactions r
   WHERE r.status_id = ANY($1)
ORDER BY r.status_id, first_at, r.emoji, r.inserted_at, r.id";

/// [`reactors`]: one status's reactions, optionally narrowed to the single
/// emoji `emoji` names (`$2` is that emoji or `NULL` for all of them), in the
/// same order [`for_statuses`] uses, and for the same reason.
const REACTORS_SQL: &str = "SELECT r.status_id, r.emoji, r.url, r.user_id,
        min(r.inserted_at) OVER (PARTITION BY r.status_id, r.emoji) AS first_at
    FROM emoji_reactions r
   WHERE r.status_id = $1 AND ($2::text IS NULL OR r.emoji = $2::text)
ORDER BY first_at, r.emoji, r.inserted_at, r.id";

/// One row of `emoji_reactions`: `user_id` reacted to `status_id` with
/// `emoji`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct EmojiReaction {
    /// `UUIDv7` primary key, minted by [`react`] (decision 4).
    pub id: Uuid,
    /// The reacting account.
    pub user_id: Uuid,
    /// The status reacted to.
    pub status_id: Uuid,
    /// The emoji as stored: one unicode emoji grapheme, or a custom emoji
    /// written `:shortcode:` (see [`crate::util::emoji`]).
    pub emoji: String,
    /// Image URL of the custom emoji as [`react`] recorded it; `None` for a
    /// unicode reaction.
    pub url: Option<String>,
    /// `ActivityPub` id of the `EmojiReact` activity behind this row
    /// (decision 6): a `{BASE_URL}/activities/{uuid}` id minted for a local
    /// reaction, to be replaced by the remote id for a mirrored one.
    pub ap_id: Option<String>,
    /// When the reaction was first made; a repeat reaction of the same emoji
    /// keeps this, which is what orders the roll-up.
    pub inserted_at: DateTime<Utc>,
}

/// What to record: every `emoji_reactions` column except the `id` and `ap_id`
/// [`react`] mints and the `inserted_at` the column stamps.
#[derive(Clone, Debug)]
pub struct NewReaction<'a> {
    /// The reacting account.
    pub user_id: Uuid,
    /// The status it reacts to.
    pub status_id: Uuid,
    /// The emoji, already validated and in its stored spelling — one unicode
    /// grapheme or `:shortcode:`.
    pub emoji: &'a str,
    /// The custom emoji's image URL; `None` for a unicode emoji.
    pub url: Option<&'a str>,
}

/// One emoji on one status, summarised for the Status entity's
/// `pleroma.emoji_reactions`: how many reacted, whether the viewer did, the
/// custom emoji's image, and the first [`MAX_SUMMARY_REACTORS`] reactors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionSummary {
    /// The emoji as stored, which is the text a client sends back to react.
    pub name: String,
    /// How many accounts reacted with this emoji — all of them, not only the
    /// ones [`account_ids`](Self::account_ids) names.
    pub count: i64,
    /// Whether the `viewer_id` [`for_statuses`] was given is among the
    /// reactors; `false` for an anonymous page.
    pub me: bool,
    /// The custom emoji's image URL, `None` for a unicode emoji.
    pub url: Option<String>,
    /// The reactors, earliest reaction first, at most
    /// [`MAX_SUMMARY_REACTORS`] of them.
    pub account_ids: Vec<Uuid>,
}

/// One emoji on one status with *every* account that reacted: what
/// [`reactors`] answers for the reactions endpoint, which renders all of them
/// instead of summarising.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactorGroup {
    /// The emoji as stored.
    pub name: String,
    /// The custom emoji's image URL, `None` for a unicode emoji: the image of
    /// this emoji's earliest reaction.
    pub url: Option<String>,
    /// The reactors, earliest reaction first.
    pub reactor_ids: Vec<Uuid>,
}

impl ReactorGroup {
    /// This group as the Status entity's summary for `viewer_id` (`None`:
    /// nobody, so `me` is false): the whole count, whether the viewer is among
    /// the reactors, and only the first [`MAX_SUMMARY_REACTORS`] ids.
    fn into_summary(self, viewer_id: Option<Uuid>) -> ReactionSummary {
        let me = viewer_id.is_some_and(|viewer| self.reactor_ids.contains(&viewer));
        let count = i64::try_from(self.reactor_ids.len()).unwrap_or(i64::MAX);
        ReactionSummary {
            name: self.name,
            count,
            me,
            url: self.url,
            account_ids: self
                .reactor_ids
                .into_iter()
                .take(MAX_SUMMARY_REACTORS)
                .collect(),
        }
    }
}

/// One fetched row of [`FOR_STATUSES_SQL`] / [`REACTORS_SQL`]: a single
/// reaction, in the order those statements return them (the window column they
/// order by is not part of it).
#[derive(Debug, sqlx::FromRow)]
struct ReactionRow {
    /// The reacted-to status: the outer key of the grouping.
    status_id: Uuid,
    /// The emoji: the inner key.
    emoji: String,
    /// The custom emoji's image URL, if any.
    url: Option<String>,
    /// The reactor: one entry of its group's reactor list.
    user_id: Uuid,
}

/// Folds [`for_statuses`]'s and [`reactors`]' flat rows into one
/// [`ReactorGroup`] per `(status_id, emoji)`, in the order the statements
/// returned them: a group starts at its first row and a later row joins the
/// group the walk stands in, which both loaders' `ORDER BY` keeps contiguous.
fn group(rows: Vec<ReactionRow>) -> Vec<(Uuid, ReactorGroup)> {
    let mut grouped: Vec<(Uuid, ReactorGroup)> = Vec::with_capacity(rows.len());
    for row in rows {
        if !join_open_group(&mut grouped, &row) {
            grouped.push((
                row.status_id,
                ReactorGroup {
                    name: row.emoji,
                    url: row.url,
                    reactor_ids: vec![row.user_id],
                },
            ));
        }
    }
    grouped
}

/// Appends `row`'s reactor to the group `grouped` last holds, answering whether
/// it fitted.
///
/// Only that last group can take a row: the statements order a status's rows by
/// their emoji's first reaction time and then by the emoji text, so the rows of
/// one `(status_id, emoji)` arrive together and a row that does not match the
/// open group starts a new one — which is what [`group`] does on `false`.
fn join_open_group(grouped: &mut [(Uuid, ReactorGroup)], row: &ReactionRow) -> bool {
    let Some((status, open)) = grouped.last_mut() else {
        return false;
    };
    if *status != row.status_id || open.name != row.emoji {
        return false;
    }
    open.reactor_ids.push(row.user_id);
    true
}

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
    let row = sqlx::query_as::<_, EmojiReaction>(REACT_SQL)
        .bind(new_id())
        .bind(reaction.user_id)
        .bind(reaction.status_id)
        .bind(reaction.emoji)
        .bind(reaction.url)
        .bind(local_activity_id(base_url))
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Removes `user_id`'s reaction `emoji` from `status_id`, reporting the row
/// that went.
///
/// Withdrawing a reaction that is not there — never made, or already withdrawn
/// — removes nothing and is not an error, so the write is idempotent; the
/// answer is `None` either way and the caller tells a withdrawal from a no-op
/// by it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unreact(
    pool: &PgPool,
    user_id: Uuid,
    status_id: Uuid,
    emoji: &str,
) -> AppResult<Option<EmojiReaction>> {
    let removed = sqlx::query_as::<_, EmojiReaction>(UNREACT_SQL)
        .bind(user_id)
        .bind(status_id)
        .bind(emoji)
        .fetch_optional(pool)
        .await?;
    Ok(removed)
}

/// The `DELETE` behind [`delete_all_for_user`]: the account's own reactions.
const DELETE_ALL_FOR_USER_SQL: &str = "DELETE FROM emoji_reactions WHERE user_id = $1";

/// Removes every reaction made by user `user_id`: the reaction step of the
/// [`users.delete_remote`](crate::jobs::deletion::KIND_DELETE_REMOTE) job.
///
/// That job keeps the account's row, so it cannot rely on the `user_id`
/// foreign key's cascade the way a hard delete does.
///
/// Reactions *on* the account's statuses are other accounts' rows and stay:
/// their targets are tombstones by the time this runs, and every read filters
/// those out, exactly as [`crate::models::favourite::delete_all_for_user`]
/// leaves the likes of a purged account's posts.
///
/// Returns how many rows went.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_all_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<u64> {
    let deleted = sqlx::query(DELETE_ALL_FOR_USER_SQL)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

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
    let rows = sqlx::query_as::<_, ReactionRow>(FOR_STATUSES_SQL)
        .bind(status_ids)
        .fetch_all(pool)
        .await?;
    let mut rollups: HashMap<Uuid, Vec<ReactionSummary>> = HashMap::new();
    for (status_id, reactions) in group(rows) {
        rollups
            .entry(status_id)
            .or_default()
            .push(reactions.into_summary(viewer_id));
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

/// The `ap_id` [`react`] stores: a freshly minted
/// `{base_url}/activities/{uuid}` (decision 6). Every reaction this model
/// writes comes from a local account through the API; the federation ticket
/// that mirrors a remote `EmojiReact` records that activity's own id.
fn local_activity_id(base_url: &str) -> String {
    format!("{base_url}/activities/{}", new_id())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::{
            status::{NewStatus, insert},
            user::{NewUser, User, insert as insert_user, local_ap_id},
        },
        util::time::parse_time,
    };

    /// The instance origin the fixtures react on.
    const BASE: &str = TEST_BASE_URL;
    /// The activity-id prefix [`react`] mints.
    const ACTIVITY_PREFIX: &str = "/activities/";
    /// A unicode emoji most fixtures use.
    const THUMBS: &str = "👍";
    /// A second unicode emoji, for the multi-emoji assertions.
    const HEART: &str = "❤️";
    /// A custom emoji's stored spelling.
    const CAT: &str = ":cat:";
    /// The image URL stored beside [`CAT`].
    const CAT_URL: &str = "https://test.example/emoji/cats/cat.png";

    /// A local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the fixture user inserts")
    }

    /// A public post by `author`, as its id.
    async fn post(pool: &PgPool, author: &User) -> Uuid {
        insert(pool, BASE, &NewStatus::local(author, "hello"))
            .await
            .expect("the fixture post inserts")
            .id
    }

    /// A public post by a fresh local author named `nickname`, as its id.
    async fn post_by(pool: &PgPool, nickname: &str) -> Uuid {
        post(pool, &user(pool, nickname).await).await
    }

    /// Reacts to `status_id` as `reactor` with `emoji`, whose custom image is
    /// `url`.
    async fn react_with(
        pool: &PgPool,
        reactor: &User,
        status_id: Uuid,
        emoji: &str,
        url: Option<&str>,
    ) {
        let stored_row = react(
            pool,
            BASE,
            &NewReaction {
                user_id: reactor.id,
                status_id,
                emoji,
                url,
            },
        )
        .await
        .expect("the fixture reaction stores");
        assert_eq!(
            stored_row.emoji, emoji,
            "the fixture row is the emoji asked for"
        );
        assert_eq!(stored_row.user_id, reactor.id, "and the reactor's own");
    }

    /// Every stored reaction as `(user, status, emoji, url)` ordered by row id,
    /// so an assertion says what the table holds rather than what a loader
    /// chose to show.
    async fn stored(pool: &PgPool) -> Vec<(Uuid, Uuid, String, Option<String>)> {
        sqlx::query_as::<_, (Uuid, Uuid, String, Option<String>)>(
            "SELECT user_id, status_id, emoji, url FROM emoji_reactions ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .expect("the table reads")
    }

    /// The instant a fixture stamps a reaction with, so group order never
    /// depends on how fast two inserts ran.
    fn instant(text: &str) -> DateTime<Utc> {
        parse_time(text).expect("a fixture timestamp parses")
    }

    /// Moves `reactor`'s reaction `emoji` on `status_id` to `when`, and nothing
    /// else.
    async fn stamp(
        pool: &PgPool,
        status_id: Uuid,
        reactor: &User,
        emoji: &str,
        when: DateTime<Utc>,
    ) {
        let moved = sqlx::query(
            "UPDATE emoji_reactions SET inserted_at = $3
              WHERE status_id = $1 AND user_id = $2 AND emoji = $4",
        )
        .bind(status_id)
        .bind(reactor.id)
        .bind(when)
        .bind(emoji)
        .execute(pool)
        .await
        .expect("the stamp updates")
        .rows_affected();
        assert_eq!(moved, 1, "the fixture reaction was there to stamp");
    }

    /// A [`ReactionSummary`] to compare against, in one line.
    fn summary(
        name: &str,
        count: i64,
        me: bool,
        url: Option<&str>,
        account_ids: &[Uuid],
    ) -> ReactionSummary {
        ReactionSummary {
            name: name.to_owned(),
            count,
            me,
            url: url.map(str::to_owned),
            account_ids: account_ids.to_vec(),
        }
    }

    /// The roll-up of `status_id` for `viewer`, or the empty `Vec` when it has
    /// no reactions.
    async fn rollup(pool: &PgPool, status_id: Uuid, viewer: Option<&User>) -> Vec<ReactionSummary> {
        for_statuses(pool, &[status_id], viewer.map(|who| who.id))
            .await
            .expect("the roll-up reads")
            .remove(&status_id)
            .unwrap_or_default()
    }

    /// The emoji names of `summaries`, in order, for the ordering assertions.
    fn names(summaries: &[ReactionSummary]) -> Vec<&str> {
        summaries
            .iter()
            .map(|summary| summary.name.as_str())
            .collect()
    }

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

    #[tokio::test]
    async fn unreacting_removes_the_row_and_is_idempotent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post_by(&pool, "bob").await;
        react_with(&pool, &alice, status_id, THUMBS, None).await;

        let removed = unreact(&pool, alice.id, status_id, THUMBS)
            .await
            .expect("the withdrawal runs")
            .expect("the reaction was there");
        assert_eq!(removed.user_id, alice.id, "the row that went");
        assert_eq!(removed.emoji, THUMBS, "and its emoji");

        assert!(
            unreact(&pool, alice.id, status_id, THUMBS)
                .await
                .expect("repeating a withdrawal runs")
                .is_none(),
            "withdrawing what is already gone removes nothing"
        );
        assert!(
            unreact(&pool, alice.id, status_id, ":never:")
                .await
                .expect("an unreacted emoji runs")
                .is_none(),
            "and an emoji that was never reacted with is no error either"
        );
        assert!(stored(&pool).await.is_empty(), "the table is empty again");
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

    #[tokio::test]
    async fn emojis_order_by_first_reaction_then_name() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let status_id = post_by(&pool, "carol").await;
        react_with(&pool, &alice, status_id, ":zebra:", None).await;
        react_with(&pool, &bob, status_id, ":zebra:", None).await;
        react_with(&pool, &alice, status_id, ":apple:", None).await;
        react_with(&pool, &bob, status_id, THUMBS, None).await;

        // :apple: is alice's second row yet the third group: what orders a
        // group is its *first* reaction, not any later one, and zebra's later
        // second reaction cannot move it either.
        stamp(
            &pool,
            status_id,
            &alice,
            ":zebra:",
            instant("2024-01-01T00:00:00.000Z"),
        )
        .await;
        stamp(
            &pool,
            status_id,
            &bob,
            ":zebra:",
            instant("2024-05-01T00:00:00.000Z"),
        )
        .await;
        stamp(
            &pool,
            status_id,
            &bob,
            THUMBS,
            instant("2024-02-01T00:00:00.000Z"),
        )
        .await;
        stamp(
            &pool,
            status_id,
            &alice,
            ":apple:",
            instant("2024-03-01T00:00:00.000Z"),
        )
        .await;

        let rollups = for_statuses(&pool, &[status_id], Some(alice.id))
            .await
            .expect("the roll-up reads");
        assert_eq!(
            names(&rollups[&status_id]),
            vec![":zebra:", THUMBS, ":apple:"],
            "january's zebra leads, then february's thumb, then march's \
             apple, which nothing later moved"
        );
        assert_eq!(
            rollups[&status_id][0].account_ids,
            vec![alice.id, bob.id],
            "and a group's reactors stay in their own reaction order"
        );

        // Two groups that began at the same instant order by emoji text.
        let twin = post_by(&pool, "dave").await;
        let same = instant("2024-04-01T00:00:00.000Z");
        react_with(&pool, &alice, twin, ":b:", None).await;
        react_with(&pool, &alice, twin, ":a:", None).await;
        for emoji in [":a:", ":b:"] {
            stamp(&pool, twin, &alice, emoji, same).await;
        }
        let twins = for_statuses(&pool, &[twin], Some(alice.id))
            .await
            .expect("the second roll-up reads");
        assert_eq!(
            names(&twins[&twin]),
            vec![":a:", ":b:"],
            "one first-reaction time leaves the emoji text to order"
        );
    }

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

    #[tokio::test]
    async fn reactors_groups_keep_every_reactor_in_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let status_id = post_by(&pool, "carol").await;
        react_with(&pool, &alice, status_id, THUMBS, None).await;
        react_with(&pool, &bob, status_id, THUMBS, None).await;
        react_with(&pool, &bob, status_id, CAT, Some(CAT_URL)).await;
        stamp(
            &pool,
            status_id,
            &alice,
            THUMBS,
            instant("2024-01-01T00:00:00.000Z"),
        )
        .await;
        stamp(
            &pool,
            status_id,
            &bob,
            THUMBS,
            instant("2024-02-01T00:00:00.000Z"),
        )
        .await;
        stamp(
            &pool,
            status_id,
            &bob,
            CAT,
            instant("2024-03-01T00:00:00.000Z"),
        )
        .await;

        let groups = reactors(&pool, status_id, None)
            .await
            .expect("the reactors read");
        assert_eq!(groups.len(), 2, "one group per emoji");
        assert_eq!(groups[0].name, THUMBS, "the earlier emoji first");
        assert_eq!(
            groups[0].reactor_ids,
            vec![alice.id, bob.id],
            "every reactor, not the capped five, in reaction order"
        );
        assert_eq!(groups[0].url, None, "a unicode group carries no image");
        assert_eq!(
            groups[1],
            ReactorGroup {
                name: CAT.to_owned(),
                url: Some(CAT_URL.to_owned()),
                reactor_ids: vec![bob.id],
            },
            "the custom group keeps its image and its one reactor"
        );

        assert_eq!(
            reactors(&pool, status_id, Some(CAT))
                .await
                .expect("the filtered read runs"),
            vec![ReactorGroup {
                name: CAT.to_owned(),
                url: Some(CAT_URL.to_owned()),
                reactor_ids: vec![bob.id],
            }],
            "the single-emoji filter answers that emoji only"
        );
        assert!(
            reactors(&pool, status_id, Some(":none:"))
                .await
                .expect("an unreacted emoji reads")
                .is_empty(),
            "an emoji nobody reacted with answers nothing"
        );
        assert!(
            reactors(&pool, post_by(&pool, "dave").await, None)
                .await
                .expect("an unreacted status reads")
                .is_empty(),
            "a status nobody reacted to answers nothing"
        );
    }

    #[tokio::test]
    async fn an_unknown_status_or_a_deleted_row_refuses_or_cascades() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;

        let err = react(
            &pool,
            BASE,
            &NewReaction {
                user_id: alice.id,
                status_id: Uuid::now_v7(),
                emoji: THUMBS,
                url: None,
            },
        )
        .await
        .expect_err("a status that does not exist cannot be reacted to");
        assert!(
            matches!(err, AppError::Database(_)),
            "the foreign key refuses, not a silent no-op: {err}"
        );

        let status_id = post_by(&pool, "bob").await;
        react_with(&pool, &alice, status_id, THUMBS, None).await;
        let gone = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(status_id)
            .execute(&pool)
            .await
            .expect("the fixture status deletes")
            .rows_affected();
        assert_eq!(gone, 1, "the post was there to delete");
        assert!(
            stored(&pool).await.is_empty(),
            "deleting the status cascaded to its reaction"
        );

        let mine = post_by(&pool, "carol").await;
        let me = user(&pool, "dave").await;
        react_with(&pool, &me, mine, THUMBS, None).await;
        let erased = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(me.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes")
            .rows_affected();
        assert_eq!(erased, 1, "the reactor was there to delete");
        assert!(
            stored(&pool).await.is_empty(),
            "deleting the reactor cascaded to their reaction too"
        );
    }

    #[test]
    fn group_folds_contiguous_rows_and_starts_a_new_group() {
        let status = Uuid::now_v7();
        let other = Uuid::now_v7();
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let rows = vec![
            row(status, THUMBS, None, a),
            row(status, THUMBS, Some(CAT_URL), b),
            row(status, CAT, Some(CAT_URL), b),
            row(other, THUMBS, None, a),
        ];

        let grouped = group(rows);
        assert_eq!(
            grouped.len(),
            3,
            "two emojis on one status, one on the other"
        );
        assert_eq!(
            grouped[0].1.reactor_ids,
            vec![a, b],
            "the two 👍 rows folded into one group"
        );
        assert_eq!(
            grouped[0].1.url, None,
            "and the group keeps the image of its earliest reaction"
        );
        assert_eq!(
            (grouped[1].0, grouped[1].1.name.clone()),
            (status, CAT.to_owned()),
            "the cat group is its own entry of the same status"
        );
        assert_eq!(
            (grouped[2].0, grouped[2].1.name.clone()),
            (other, THUMBS.to_owned()),
            "and the same emoji on another status starts over"
        );
        assert!(
            group(Vec::new()).is_empty(),
            "nothing in answers nothing out"
        );
    }

    #[test]
    fn a_summary_counts_every_reactor_and_lists_the_first_five() {
        let me = Uuid::now_v7();
        let listed: Vec<Uuid> = (0..MAX_SUMMARY_REACTORS).map(|_| Uuid::now_v7()).collect();
        let late = Uuid::now_v7();
        let group = ReactorGroup {
            name: CAT.to_owned(),
            url: Some(CAT_URL.to_owned()),
            reactor_ids: [listed.as_slice(), &[late, me]].concat(),
        };

        assert_eq!(
            group.clone().into_summary(Some(me)),
            ReactionSummary {
                name: CAT.to_owned(),
                count: 7,
                me: true,
                url: Some(CAT_URL.to_owned()),
                account_ids: listed,
            },
            "the seventh reactor is `me` without being listed"
        );
        assert!(
            !group.into_summary(None).me,
            "an anonymous viewer is never `me`"
        );
    }

    /// The sweep of an account whose row is kept: its own reactions go,
    /// another account's reaction on the same status stays, and an account
    /// that never reacted removes nothing.
    #[tokio::test]
    async fn delete_all_for_user_takes_only_the_accounts_own_reactions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let status = post(&pool, &alice).await;
        react_with(&pool, &alice, status, THUMBS, None).await;
        react_with(&pool, &alice, status, HEART, None).await;
        react_with(&pool, &bob, status, CAT, Some(CAT_URL)).await;

        let removed = delete_all_for_user(&pool, alice.id)
            .await
            .expect("the sweep runs");

        assert_eq!(removed, 2, "both of alice's reactions went");
        let left = reactors(&pool, status, None)
            .await
            .expect("the reactors read");
        assert_eq!(left.len(), 1, "only the custom emoji is left");
        assert_eq!(
            left[0].reactor_ids,
            vec![bob.id],
            "and it is bob's row on alice's status"
        );
        assert_eq!(
            delete_all_for_user(&pool, alice.id)
                .await
                .expect("the rerun runs"),
            0,
            "and a second pass removes nothing"
        );
    }

    /// One fetched [`ReactionRow`], for [`group`]'s unit test.
    fn row(status_id: Uuid, emoji: &str, url: Option<&str>, user_id: Uuid) -> ReactionRow {
        ReactionRow {
            status_id,
            emoji: emoji.to_owned(),
            url: url.map(str::to_owned),
            user_id,
        }
    }
}
