//! The batched preload queries of a page render that read statuses, their
//! authors and the per-viewer flags — one id-keyed read per part, each
//! skipping the database when its id list is empty. The poll and card
//! loaders live in [`super::loaders_polls_cards`].

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    entities::status::QuoteSlot,
    error::AppResult,
    models::{
        activitypub_actor::{self, ActivityPubActor},
        bookmark, conversation, conversation_mute,
        emoji_reaction::{self, ReactionSummary},
        favourite,
        hashtag::{self, Hashtag},
        media_attachment::{self, MediaAttachment as AttachmentRow},
        mention, pinned_status,
        quote_request::{self, QuoteRequestState},
        status::{self, Status as StatusRow},
        status_visibility,
        user::{self, User},
    },
};

use super::ViewerMode;

/// [`status::get_many`] keyed by id — one query for a page's reblogged
/// originals or quoted originals, and no query at all for an empty list.
/// Tombstones are already excluded by the loader, which is how a reblog
/// of a deleted original falls out of the render.
pub(super) async fn load_statuses_by_id(
    pool: &PgPool,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, StatusRow>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = status::get_many(pool, ids).await?;
    Ok(rows.into_iter().map(|row| (row.id, row)).collect())
}

/// [`status_visibility::filter_visible`] over the batch-loaded quoted
/// originals — the one read that decides which quotes this viewer may
/// see, and the only statement step 3 adds: a page whose rows quote
/// nothing skips it entirely, and the filter itself runs its visibility
/// clause in the database, so a private or blocked quote falls out of
/// the set and renders `quote: null` with `pleroma.quote_visible` false
/// (its `quote_id` still names the target). The quoted rows' authors are
/// the page's strangers' by nature, so the filter's follow/mention/
/// block `EXISTS` arms run for real here.
///
/// [`ViewerMode::Admin`] runs no filter at all — Pleroma's
/// `AdminAPI.StatusView` asks the visibility question of nobody — so every
/// quoted row on the page is in the set and the database is not asked, and
/// [`ViewerMode::User`] is the ordinary read.
async fn load_visible_quotes(
    pool: &PgPool,
    quoted: &HashMap<Uuid, StatusRow>,
    mode: ViewerMode<'_>,
) -> AppResult<HashSet<Uuid>> {
    let candidates: Vec<StatusRow> = quoted.values().cloned().collect();
    if mode.bypasses_visibility() {
        return Ok(candidates.iter().map(|row| row.id).collect());
    }
    let visible = status_visibility::filter_visible(pool, candidates, mode.viewer()).await?;
    Ok(visible.into_iter().map(|row| row.id).collect())
}

/// The [module docs]' steps 2 and 3 as one question — what does this view
/// see of what these posts quote? — answered as the two maps the batch
/// keeps: every quoted original the page names, and the subset admitted to
/// nest. Split out of [`Batch::load`] because the pair always travels
/// together.
///
/// [module docs]: super
/// [`Batch::load`]: super::batch::Batch::load
pub(super) async fn load_quotes(
    pool: &PgPool,
    quote_ids: &[Uuid],
    mode: ViewerMode<'_>,
) -> AppResult<(HashMap<Uuid, StatusRow>, HashSet<Uuid>)> {
    let quoted = load_statuses_by_id(pool, quote_ids).await?;
    let visible_quotes = load_visible_quotes(pool, &quoted, mode).await?;
    debug!(
        quotes = quoted.len(),
        visible = visible_quotes.len(),
        "quoted originals batch-loaded; the visible ones render nested, one level deep"
    );
    Ok((quoted, visible_quotes))
}

/// The quotes among `quoting_ids` whose author has not said yes, as render
/// slots.
///
/// A request still waiting for its answer, a refused one and a withdrawn
/// grant become the [`QuoteSlot`] that renders each. A quote with no
/// request (of a local post) or with an accepted one has no slot. One read
/// per page; an empty list asks nothing.
pub(super) async fn load_quote_denials(
    pool: &PgPool,
    quoting_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, QuoteSlot>> {
    let unaccepted = quote_request::unaccepted_for_statuses(pool, quoting_ids).await?;
    Ok(unaccepted
        .into_iter()
        .filter_map(|(id, state)| match state {
            QuoteRequestState::Pending => Some((id, QuoteSlot::Pending)),
            QuoteRequestState::Rejected => Some((id, QuoteSlot::Rejected)),
            QuoteRequestState::Revoked => Some((id, QuoteSlot::Revoked)),
            QuoteRequestState::Accepted => None,
        })
        .collect())
}

/// The ids among `parent_ids` — the `in_reply_to_id`s the page names —
/// this viewer may see: [`status::get_many`] loads the parents, then one
/// [`status_visibility::filter_visible`] decides which stand. A parent that
/// is gone (a tombstone, or never stored) is simply absent, so its replies
/// render `pleroma.parent_visible` false. [`ViewerMode::Admin`] runs no
/// filter, like [`load_quotes`]: every stored parent is admitted. An empty
/// list asks the database nothing.
pub(super) async fn load_visible_parents(
    pool: &PgPool,
    parent_ids: &[Uuid],
    mode: ViewerMode<'_>,
) -> AppResult<HashSet<Uuid>> {
    let parents = load_statuses_by_id(pool, parent_ids).await?;
    let visible = load_visible_quotes(pool, &parents, mode).await?;
    debug!(
        parents = parents.len(),
        visible = visible.len(),
        "replied-to statuses batch-loaded; the visible ones set parent_visible"
    );
    Ok(visible)
}

/// [`user::get_many`] keyed by id — one query for every author and
/// replied-to author on the page.
pub(super) async fn load_users(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, User>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(user::get_many(pool, ids)
        .await?
        .into_iter()
        .map(|account| (account.id, account))
        .collect())
}

/// The `activitypub_actors` rows of the remote accounts among `users`, keyed by account id.
///
/// A remote quote policy names the author's followers and following
/// collections, which the row holds. One query reads them all; a page of
/// local accounts skips it, since nothing on a page reads a local account's row.
pub(super) async fn load_remote_actors(
    pool: &PgPool,
    users: &HashMap<Uuid, User>,
) -> AppResult<HashMap<Uuid, ActivityPubActor>> {
    let remote: Vec<Uuid> = users
        .values()
        .filter(|account| !account.local.get())
        .map(|account| account.id)
        .collect();
    if remote.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(activitypub_actor::get_many(pool, &remote)
        .await?
        .into_iter()
        .map(|row| (row.user_id, row))
        .collect())
}

/// [`media_attachment::for_statuses`]: one query for every attachment.
pub(super) async fn load_media(
    pool: &PgPool,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<AttachmentRow>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    media_attachment::for_statuses(pool, ids).await
}

/// [`mention::for_statuses`]: one query for every @-mention.
pub(super) async fn load_mentions(
    pool: &PgPool,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<User>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    mention::for_statuses(pool, ids).await
}

/// [`hashtag::for_statuses`]: one query for every hashtag.
pub(super) async fn load_tags(
    pool: &PgPool,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<Hashtag>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    hashtag::for_statuses(pool, ids).await
}

/// [`emoji_reaction::for_statuses`]: one query for every reaction of the
/// page, the viewer riding along only to answer each roll-up's `me` (an
/// anonymous viewer has no id to pass, which makes every `me` false).
pub(super) async fn load_reactions(
    pool: &PgPool,
    ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashMap<Uuid, Vec<ReactionSummary>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    emoji_reaction::for_statuses(pool, ids, viewer.map(|who| who.id)).await
}

/// [`favourite::favourited_status_ids`]: one query for the viewer's likes
/// among the page's ids (reblogged originals included, since that is
/// where a reblog's like lives). An anonymous viewer has no rows to ask
/// about, and the empty set makes every flag false — no query at all.
pub(super) async fn load_favourites(
    pool: &PgPool,
    ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashSet<Uuid>> {
    let Some(viewer) = viewer else {
        return Ok(HashSet::new());
    };
    favourite::favourited_status_ids(pool, viewer.id, ids).await
}

/// [`status::reblogged_status_ids`]: one query for the originals among the
/// page's ids the viewer has a live reblog of (reblogged originals
/// included, since that is where a reblog's boost lives and a render
/// answers the nested original's flag too). An anonymous viewer has no
/// rows to ask about, and the empty set makes every flag false — no query
/// at all.
pub(super) async fn load_reblogs(
    pool: &PgPool,
    ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashSet<Uuid>> {
    let Some(viewer) = viewer else {
        return Ok(HashSet::new());
    };
    status::reblogged_status_ids(pool, viewer.id, ids).await
}

/// [`bookmark::bookmarked_folders`]: one query for the viewer's bookmarks
/// among the page's ids, each carrying the folder it is filed in (the ids
/// of reblogged originals ride along, since a render answers their flags too
/// and one of them may be bookmarked). An anonymous viewer has no rows to
/// ask about, and the empty map makes every flag false and every folder
/// `null` — no query at all.
pub(super) async fn load_bookmarks(
    pool: &PgPool,
    ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashMap<Uuid, Option<Uuid>>> {
    let Some(viewer) = viewer else {
        return Ok(HashMap::new());
    };
    bookmark::bookmarked_folders(pool, viewer.id, ids).await
}

/// [`pinned_status::pinned_ids`]: one query for the viewer's pins among the
/// page's ids, keyed by status and carrying the moment each pin happened
/// (the ids of reblogged originals ride along, since a render answers their
/// flags too and one of them may be the viewer's own pinned post). An
/// anonymous viewer has no rows to ask about, and the empty map makes every
/// flag false and every stamp `null` — no query at all.
pub(super) async fn load_pins(
    pool: &PgPool,
    ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashMap<Uuid, DateTime<Utc>>> {
    let Some(viewer) = viewer else {
        return Ok(HashMap::new());
    };
    pinned_status::pinned_ids(pool, viewer.id, ids).await
}

/// [`conversation_mute::muted_conversation_ids`]: one query for the threads
/// among the page's conversation ids the viewer has muted (the ids of
/// reblogged originals ride along, though a reblog shares its original's
/// conversation, so the pair never diverges). An anonymous viewer has no
/// mutes to ask about, and the empty set makes every flag false — no query
/// at all.
pub(super) async fn load_thread_mutes(
    pool: &PgPool,
    conversation_ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashSet<Uuid>> {
    let Some(viewer) = viewer else {
        return Ok(HashSet::new());
    };
    conversation_mute::muted_conversation_ids(pool, viewer.id, conversation_ids).await
}

/// [`conversation::participation_ids`]: one query for the DM threads among
/// the page's direct-conversation ids the viewer participates in, keyed by
/// thread with the viewer's own participation id. An anonymous viewer has
/// no rows to ask about, and the empty map makes every
/// `pleroma.direct_conversation_id` `null` — no query at all.
pub(super) async fn load_direct_participations(
    pool: &PgPool,
    direct_ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashMap<Uuid, Uuid>> {
    let Some(viewer) = viewer else {
        return Ok(HashMap::new());
    };
    conversation::participation_ids(pool, viewer.id, direct_ids).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::activitypub_actor::ActivityPubActorWrite,
        test_fixtures::{federating_user, remote_user, write_actor},
    };

    /// A remote account's row is loaded; a local one's is not, since
    /// nothing on a page reads it.
    #[tokio::test]
    async fn only_remote_accounts_load_their_actor_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = federating_user(&pool, "alice").await;
        let far = remote_user(&pool, "far").await;
        let _row = write_actor(
            &pool,
            &ActivityPubActorWrite {
                followers_url: Some("https://row.example/followers".to_owned()),
                ..ActivityPubActorWrite::bare(far.id)
            },
        )
        .await;

        let users = load_users(&pool, &[alice.id, far.id])
            .await
            .expect("the users load");
        let actors = load_remote_actors(&pool, &users)
            .await
            .expect("the rows load");

        assert_eq!(actors.len(), 1, "one remote account on the page");
        assert_eq!(
            actors[&far.id].followers_url.as_deref(),
            Some("https://row.example/followers"),
            "the remote account's collections are its actor row's"
        );
    }
}
