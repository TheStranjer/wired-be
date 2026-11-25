//! Fixtures the emoji reaction model's unit tests share.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    NewReaction, ReactionSummary,
    read::for_statuses,
    write::{react, react_in_domain},
};
use crate::{
    config::TEST_BASE_URL,
    models::{
        status::{NewStatus, insert},
        user::User,
    },
    util::{emoji::canonical, time::parse_time},
};

pub(super) use crate::test_fixtures::local_user as user;

/// The instance origin the fixtures react on.
pub(super) const BASE: &str = TEST_BASE_URL;
/// The activity-id prefix [`react`] mints.
pub(super) const ACTIVITY_PREFIX: &str = "/activities/";
/// A unicode emoji most fixtures use.
pub(super) const THUMBS: &str = "👍";
/// A second unicode emoji, for the multi-emoji assertions.
pub(super) const HEART: &str = "❤️";
/// A custom emoji's stored spelling.
pub(super) const CAT: &str = ":cat:";
/// The name the API gives [`CAT`]: its bare shortcode.
pub(super) const CAT_NAME: &str = "cat";
/// The image URL stored beside [`CAT`].
pub(super) const CAT_URL: &str = "https://test.example/emoji/cats/cat.png";

/// The stored spelling of a remote custom emoji.
pub(super) const REMOTE_CAT: &str = ":misskeycat:";
/// The instance [`REMOTE_CAT`] belongs to.
pub(super) const REMOTE_DOMAIN: &str = "misskey.test";
/// The image URL stored beside [`REMOTE_CAT`].
pub(super) const REMOTE_CAT_URL: &str = "https://misskey.test/files/misskeycat.png";

/// A public post by `author`, as its id.
pub(super) async fn post(pool: &PgPool, author: &User) -> Uuid {
    insert(pool, BASE, &NewStatus::local(author, "hello"))
        .await
        .expect("the fixture post inserts")
        .id
}

/// A public post by a fresh local author named `nickname`, as its id.
pub(super) async fn post_by(pool: &PgPool, nickname: &str) -> Uuid {
    post(pool, &user(pool, nickname).await).await
}

/// Reacts to `status_id` as `reactor` with `emoji`, whose custom image is
/// `url`.
pub(super) async fn react_with(
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
        stored_row.emoji,
        canonical(emoji),
        "the fixture row is the emoji asked for, as its grouping key"
    );
    assert_eq!(stored_row.user_id, reactor.id, "and the reactor's own");
}

/// Reacts to `status_id` as `reactor` with [`REMOTE_CAT`] of
/// [`REMOTE_DOMAIN`], and answers the stored row.
pub(super) async fn react_remote(
    pool: &PgPool,
    reactor: &User,
    status_id: Uuid,
) -> super::EmojiReaction {
    react_in_domain(
        pool,
        &NewReaction {
            user_id: reactor.id,
            status_id,
            emoji: REMOTE_CAT,
            url: Some(REMOTE_CAT_URL),
        },
        &format!("https://{REMOTE_DOMAIN}/activities/{}", Uuid::now_v7()),
        Some(REMOTE_DOMAIN),
    )
    .await
    .expect("the remote fixture reaction stores")
}

/// Every stored reaction as `(user, status, emoji, url)` ordered by row id,
/// so an assertion says what the table holds rather than what a loader
/// chose to show.
pub(super) async fn stored(pool: &PgPool) -> Vec<(Uuid, Uuid, String, Option<String>)> {
    sqlx::query_as::<_, (Uuid, Uuid, String, Option<String>)>(
        "SELECT user_id, status_id, emoji, url FROM emoji_reactions ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("the table reads")
}

/// The instant a fixture stamps a reaction with, so group order never
/// depends on how fast two inserts ran.
pub(super) fn instant(text: &str) -> DateTime<Utc> {
    parse_time(text).expect("a fixture timestamp parses")
}

/// Moves `reactor`'s reaction `emoji` on `status_id` to `when`, and nothing
/// else.
pub(super) async fn stamp(
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
    .bind(&*canonical(emoji))
    .execute(pool)
    .await
    .expect("the stamp updates")
    .rows_affected();
    assert_eq!(moved, 1, "the fixture reaction was there to stamp");
}

/// A [`ReactionSummary`] to compare against, in one line.
pub(super) fn summary(
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
pub(super) async fn rollup(
    pool: &PgPool,
    status_id: Uuid,
    viewer: Option<&User>,
) -> Vec<ReactionSummary> {
    for_statuses(pool, &[status_id], viewer.map(|who| who.id))
        .await
        .expect("the roll-up reads")
        .remove(&status_id)
        .unwrap_or_default()
}

/// The emoji names of `summaries`, in order, for the ordering assertions.
pub(super) fn names(summaries: &[ReactionSummary]) -> Vec<&str> {
    summaries
        .iter()
        .map(|summary| summary.name.as_str())
        .collect()
}
