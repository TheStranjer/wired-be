//! The stream names and the id rosters [`super`] assembles its audience
//! answers from: the public timeline set, the hashtag timelines and the
//! `user:{id}` names, and the home rule's followers and local mentions.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{follow, mention, status::Status, user::User},
};

/// The public timeline stream names of `row`: the global `public` stream
/// and its local or remote variant, and — once the status carries
/// attachments — the same pair again on the `public:media` timelines.
pub(super) fn public_timeline_streams(row: &Status, author: &User, has_media: bool) -> Vec<String> {
    let mut streams = vec!["public".to_owned()];
    if row.local {
        streams.push("public:local".to_owned());
    } else {
        streams.push("public:remote".to_owned());
        if let Some(domain) = &author.domain {
            streams.push(format!("public:remote:{domain}"));
        }
    }
    if has_media {
        streams.push("public:media".to_owned());
        if row.local {
            streams.push("public:local:media".to_owned());
        } else {
            streams.push("public:remote:media".to_owned());
        }
    }
    streams
}

/// The hashtag streams of a federated visibility (`public`, `unlisted`):
/// every tag's global `hashtag:{tag}` timeline, and — for a local status,
/// which is all this instance's tag timelines ever carry — its
/// `hashtag:local:{tag}` beside it.
pub(super) fn hashtag_streams(tags: &[String], local_status: bool) -> Vec<String> {
    let mut streams = Vec::new();
    for tag in tags {
        streams.push(format!("hashtag:{tag}"));
        if local_status {
            streams.push(format!("hashtag:local:{tag}"));
        }
    }
    streams
}

/// The `local` visibility's hashtag streams: each tag's
/// `hashtag:local:{tag}` and nothing global — the post never leaves this
/// instance, so it belongs in no global tag timeline.
pub(super) fn local_hashtag_streams(tags: &[String]) -> Vec<String> {
    tags.iter()
        .map(|tag| format!("hashtag:local:{tag}"))
        .collect()
}

/// The `user:{id}` stream names of `ids`, in the order given.
pub(super) fn user_streams(ids: &[Uuid]) -> Vec<String> {
    ids.iter().map(|id| format!("user:{id}")).collect()
}

/// The home rule's ids: the author, the followers whose home timeline
/// still sees this post (the [boost-narrowed](crate::services::streaming#the-home-rule)
/// set for a reblog row), and the local users it mentions — sorted and
/// dedup, since one account is often both follower and mention.
pub(super) async fn home_ids(pool: &PgPool, row: &Status, author: &User) -> AppResult<Vec<Uuid>> {
    let mut ids = vec![author.id];
    ids.extend(followers_of(pool, row, author).await?);
    ids.extend(mentioned_local_ids(pool, row.id).await?);
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// The follower half of the home and private rules: the ids of the local,
/// active, accepted follows of the author, narrowed to the ones still
/// seeing boosts when `row` is itself a reblog.
pub(super) async fn followers_of(
    pool: &PgPool,
    row: &Status,
    author: &User,
) -> AppResult<Vec<Uuid>> {
    follow::accepted_local_follower_ids(pool, author.id, row.reblog_of_id.is_some()).await
}

/// The `status_mentions` accounts of `row` (silent links included) that this instance owns: the
/// local half of a direct audience and of the home rule's mention side (a
/// remote mention holds no socket here).
pub(super) async fn mentioned_local_ids(pool: &PgPool, status_id: Uuid) -> AppResult<Vec<Uuid>> {
    let mut grouped = mention::access_for_statuses(pool, &[status_id]).await?;
    let some_users = grouped.remove(&status_id).unwrap_or_default();
    Ok(some_users
        .into_iter()
        .filter(|user| user.local.get())
        .map(|user| user.id)
        .collect())
}
