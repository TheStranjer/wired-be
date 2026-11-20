//! The batched preload queries of a page render for polls, rich-media
//! cards and the viewer's relationships to the page's authors — the
//! second half of the loaders in [`super::loaders`].

use std::collections::HashMap;

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    entities::relationship::{Relationship, RelationshipMap},
    error::AppResult,
    models::{
        link_card::{self, LinkCard},
        poll::{self, Poll as PollRow, PollOption as PollOptionRow},
        user::User,
    },
};

/// [`poll::for_statuses`]: one query for every poll of the page (reblogged
/// originals included, since a reblog nests its original's poll). A row
/// with no poll stays absent from the map, which is the entity's `null`.
pub(super) async fn load_polls(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, PollRow>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    poll::for_statuses(pool, ids).await
}

/// [`poll::options_for_statuses`]: one query for the options of the page's
/// polls, keyed by status and in position order — the choices every poll
/// entity lists.
pub(super) async fn load_poll_options(
    pool: &PgPool,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<PollOptionRow>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    poll::options_for_statuses(pool, ids).await
}

/// [`poll::votes_by_status`]: one query for the viewer's marks on the
/// page's polls, keyed by status with positions ascending. An anonymous
/// viewer has no marks to ask about, and the empty map makes every poll
/// render `voted: false` with `own_votes: []` — no query at all.
pub(super) async fn load_poll_votes(
    pool: &PgPool,
    ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashMap<Uuid, Vec<i32>>> {
    let Some(viewer) = viewer else {
        return Ok(HashMap::new());
    };
    poll::votes_by_status(pool, ids, viewer.id).await
}

/// [`link_card::for_statuses`]: one query for the rich-media card of every
/// row on the page and of the originals nested in reblogs (the rendered
/// id list covers both), keyed to each row's *own* id — `status_cards`
/// keys on the row. Rows that link no cached card are simply absent from
/// the map, and an empty id list skips the database — no query at all.
pub(super) async fn load_cards(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, LinkCard>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    link_card::for_statuses(pool, ids).await
}

/// The viewer's relationships to the page's authors, keyed by account id:
/// [`Relationship::load_many`] runs its own fixed set of queries once per
/// page; an anonymous viewer has none to load, and an empty author list
/// skips even that set.
pub(super) async fn load_relationships(
    pool: &PgPool,
    viewer: Option<&User>,
    ids: &[Uuid],
) -> AppResult<Option<RelationshipMap>> {
    let Some(viewer) = viewer else {
        return Ok(None);
    };
    if ids.is_empty() {
        return Ok(Some(RelationshipMap::new()));
    }
    let loaded = Relationship::load_many(pool, viewer.id, ids).await?;
    Ok(Some(ids.iter().copied().zip(loaded).collect()))
}
