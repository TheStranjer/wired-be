//! # The history and the source
//!
//! The reads behind the edit: the [`StatusEdit`] versions of a status and
//! the raw source text it was written from, both gated the show route's
//! way.
//!
//! [`StatusEdit`]: crate::entities::status_edit::StatusEdit
//! `GET /api/v1/statuses/{id}/history` is Pleroma's `show_history`:
//! optional auth (`read:statuses` on a presented user token), the same
//! status gate, and the answer the [`StatusEdit`]
//! array — the superseded versions oldest-authored-first with the current
//! one last, each carrying its content, warning, flag, author, media, and
//! the custom emojis its text used (one batched read per answer) and the
//! `poll` — Mastodon's `{"options": [{"title": …}]}`, the options that
//! version asked, `null` for a version without a poll. A status nobody
//! edited is the one-element array of its current version.
//!
//! `GET /api/v1/statuses/{id}/source` is Pleroma's `show_source` with the
//! author-only gate this ticket pins: a user token with `read:statuses`,
//! the shared status gate, 403 `"Record not found"` for a visible post of
//! somebody else's, and the answer `{"id", "text", "spoiler_text",
//! "content_type"}` — the stored raw source text and the type it was
//! written in, which is how a client re-edits a post without its HTML.
//!
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Serialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::status_for_viewer_or_404,
    auth::{AuthUser, MaybeAuthUser},
    config::Config,
    entities::{
        account::Account,
        custom_emoji::CustomEmoji,
        media_attachment::MediaAttachment,
        status_edit::{StatusEdit, StatusEditSource},
    },
    error::{AppError, AppResult},
    models::{
        custom_emoji::CustomEmoji as EmojiRow,
        media_attachment::{self, MediaAttachment as AttachmentRow},
        poll,
        status::Status as StatusRow,
        status_edit,
        user::{self, User},
    },
    oauth::scopes::READ_STATUSES,
    services::{
        account_emojis::AccountEmojis,
        render_status::{EmojiText, load_emojis, row_texts},
    },
    state::AppState,
};

use super::RECORD_NOT_FOUND;

/// `GET /api/v1/statuses/{id}/history` — every version of the status `id`,
/// oldest authored first and the CURRENT one last, as the viewer behind an
/// optional token may see it.
///
/// The gate is the show route's ([`status_for_viewer_or_404`], with
/// `read:statuses` on a presented user token); Pleroma's `show_history`
/// renders the object's `state` list plus the live object the same way, a
/// never-edited status as the one-element array. The superseded versions
/// come from [`status_edit::list_for_status`], the live row supplies the
/// last one — its `edited_at` the moment that version was authored, its
/// creation time before any edit — and the author's [`Account`] entity,
/// rendered as this viewer sees them, names every element. The media each
/// version wore resolves in one batch read ([`history_attachments`]); ids
/// whose attachment row is gone render nothing beside the ones still
/// stored.
///
/// # Errors
///
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] from the extractor for a
/// token that does not resolve; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status, and for the author an account
/// purge left behind (a foreign key the revisions keep, the live row's
/// author this read cannot render without). Any database failure arrives
/// through `?` as [`AppError::Database`].
pub(super) async fn get_status_history(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<StatusEdit>>> {
    auth.require_user_scope(READ_STATUSES)?;
    let viewer = auth.0.as_ref();
    let row = status_for_viewer_or_404(&state, viewer, &id).await?;
    let revisions = status_edit::list_for_status(&state.db, row.id).await?;
    let author = user::get_by_id(&state.db, row.user_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let worn = media_attachment::for_statuses(&state.db, &[row.id]).await?;
    let current: Vec<Uuid> = worn
        .get(&row.id)
        .map_or_default(|rows| rows.iter().map(|attachment| attachment.id).collect());
    let attachments = history_attachments(&state, &revisions, &current).await?;
    let emojis = history_emojis(&state, &author, &row, &revisions).await?;
    let author_emojis = AccountEmojis::load_one(&state.db, &author).await?;
    let render_author = || {
        author_emojis.fill(
            Account::from_user_for(viewer, &author, &state.config, None),
            &author,
            &state.config,
        )
    };
    let mut history = Vec::with_capacity(revisions.len() + 1);
    for revision in &revisions {
        history.push(StatusEdit::from_source(StatusEditSource {
            content: &revision.content,
            spoiler_text: &revision.spoiler_text,
            sensitive: revision.sensitive,
            created_at: revision.created_at,
            account: render_author(),
            media: revision_media(revision, &attachments, &state.config),
            emojis: emoji_entities(revision.id, &emojis, &state.config),
            poll_options: revision.poll_options.clone(),
        }));
    }
    history.push(StatusEdit::from_source(StatusEditSource {
        content: &row.content,
        spoiler_text: &row.spoiler_text,
        sensitive: row.sensitive,
        created_at: row.edited_at.unwrap_or(row.inserted_at),
        account: render_author(),
        media: media_entities(&current, &attachments, &state.config),
        emojis: emoji_entities(row.id, &emojis, &state.config),
        poll_options: current_poll_options(&state, row.id).await?,
    }));
    debug!(
        status = %row.id,
        versions = history.len(),
        "status history read"
    );
    Ok(Json(history))
}

/// The option titles of the poll `status_id` carries now, in order; `None`
/// for a status without one.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn current_poll_options(state: &AppState, status_id: Uuid) -> AppResult<Option<Vec<String>>> {
    let Some(stored) = poll::get_for_status(&state.db, status_id).await? else {
        return Ok(None);
    };
    let options = poll::options(&state.db, stored.id).await?;
    Ok(Some(
        options.into_iter().map(|option| option.title).collect(),
    ))
}

/// Every attachment one history answer may render: the ids the revisions
/// name plus the ones the live post wears, read in one batch and keyed by
/// id ([`media_attachment::get_many`]). An id whose row has since been
/// deleted is simply absent, and the entry that named it renders one
/// attachment fewer.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn history_attachments(
    state: &AppState,
    revisions: &[status_edit::StatusEdit],
    current: &[Uuid],
) -> AppResult<HashMap<Uuid, AttachmentRow>> {
    let asked: Vec<Uuid> = revisions
        .iter()
        .flat_map(|revision| revision.media_ids.iter().copied())
        .chain(current.iter().copied())
        .collect();
    if asked.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = media_attachment::get_many(&state.db, &asked).await?;
    Ok(rows.into_iter().map(|found| (found.id, found)).collect())
}

/// The custom emojis every version of one history answer uses: each
/// revision's content and warning, and the live row's, looked up under the
/// author's domain in one batch ([`load_emojis`]) and keyed by the
/// revision's id — the live row by its own. A version using none has no
/// entry.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn history_emojis(
    state: &AppState,
    author: &User,
    row: &StatusRow,
    revisions: &[status_edit::StatusEdit],
) -> AppResult<HashMap<Uuid, Vec<EmojiRow>>> {
    let wanted: Vec<EmojiText<'_>> = revisions
        .iter()
        .map(|revision| EmojiText {
            key: revision.id,
            domain: author.domain.as_deref(),
            texts: vec![revision.content.as_str(), revision.spoiler_text.as_str()],
        })
        .chain(std::iter::once(row_texts(row, author, &[])))
        .collect();
    load_emojis(&state.db, &wanted).await
}

/// The `emojis` array of the history entry keyed `key`: the batch's rows as
/// [`CustomEmoji`] entities, the empty array for a version using none.
fn emoji_entities(
    key: Uuid,
    by_key: &HashMap<Uuid, Vec<EmojiRow>>,
    config: &Config,
) -> Vec<CustomEmoji> {
    by_key.get(&key).map_or_default(|found| {
        found
            .iter()
            .map(|emoji| CustomEmoji::from_model(emoji, config))
            .collect()
    })
}

/// The `media_attachments` array of one superseded version: its ids in their
/// stored order, each answered by its batched row dressed in the alt text and
/// focal point that version carried ([`status_edit::StatusEdit::dress`]).
///
/// The snapshot is aligned with the stored ids, so an attachment whose row
/// is gone drops out without shifting the others' metadata; a revision
/// recorded before the snapshot existed renders the rows as stored now.
fn revision_media(
    revision: &status_edit::StatusEdit,
    by_id: &HashMap<Uuid, AttachmentRow>,
    config: &Config,
) -> Vec<MediaAttachment> {
    revision
        .media_ids
        .iter()
        .enumerate()
        .filter_map(|(index, id)| {
            let mut row = by_id.get(id)?.clone();
            revision.dress(index, &mut row);
            Some(MediaAttachment::from_model(&row, config))
        })
        .collect()
}

/// The `media_attachments` array of one history entry: `ids` in their
/// stored order, each answered by its batched row, and an id the batch
/// could not answer contributing nothing.
pub(super) fn media_entities(
    ids: &[Uuid],
    by_id: &HashMap<Uuid, AttachmentRow>,
    config: &Config,
) -> Vec<MediaAttachment> {
    ids.iter()
        .filter_map(|id| by_id.get(id))
        .map(|row| MediaAttachment::from_model(row, config))
        .collect()
}

/// `GET /api/v1/statuses/{id}/source` — the source text behind the status
/// `id`, for its author alone.
///
/// The gate is the shared [`status_for_viewer_or_404`] (so the malformed,
/// unknown, deleted and invisible are the module's one 404) and then
/// ownership: Pleroma's `show_source` asks only visibility, and the ticket
/// sharpens that to author-only — a stranger may read a public post's HTML
/// but never its source, and the 403 keeps the `"Record not found"` body
/// the delete and the edit use for the same refusal.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:statuses` check; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status; [`AppError::Forbidden`] for a
/// visible status that is not the caller's own. Any database failure
/// arrives through `?` as [`AppError::Database`].
pub(super) async fn get_status_source(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<SourceBody>> {
    auth.require(READ_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if row.user_id != auth.0.id {
        return Err(AppError::Forbidden(RECORD_NOT_FOUND.to_owned()));
    }
    Ok(Json(SourceBody {
        id: row.id.to_string(),
        text: row.raw_text,
        spoiler_text: row.spoiler_text,
        content_type: row.content_type,
    }))
}

/// The answer of `GET /api/v1/statuses/{id}/source`: the status's stored
/// source text beside the warning and the `MIME` type that text was
/// written in — Pleroma's `source.json` shape
/// (`StatusView.render("source.json")`), whose `content_type` key
/// Mastodon's own answer lacks and the ticket pins. It carries no render:
/// what the client reads here is exactly what an edit sends back.
#[derive(Debug, Serialize)]
pub(super) struct SourceBody {
    /// The status id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    id: String,
    /// The stored source text ([`raw_text`](StatusRow::raw_text)).
    text: String,
    /// The stored content warning; `""` when there is none.
    spoiler_text: String,
    /// The `MIME` type [`text`](Self::text) was written in.
    content_type: String,
}
