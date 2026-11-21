//! The row assembly and the one-transaction write of an ingested status.
//!
//! `build_new_status` folds the parsed document and the resolved
//! references into the [`NewStatus`] the insert takes; `write_ingested`
//! then lands the status, its remote media (`insert_media`, the same
//! function the inbound edit handler writes through) and its poll
//! (`insert_poll`) on one transaction. These are steps 7 and 8 of the
//! [parent module](crate::services::ingest).

use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    federation::{
        dispatch::FederationError,
        note_parser::{ParsedAttachment, ParsedNote, ParsedPoll},
    },
    models::{
        media_attachment::{self, NewMediaAttachment},
        poll::{self, PollParams},
        status::{self, NewStatus, Status},
        user::User,
    },
    state::AppState,
};

use super::{ReplyLink, transient, whole};

/// Steps 4 to 8 as data: the [`NewStatus`] the parsed document plus the
/// resolved references describe.
///
/// The parse filled everything the document alone states (author, text,
/// `uri`, `url`, visibility, timestamps); this moves the tag lists onto the
/// row's columns, answers, quotes and mentions included, fixes the status
/// as remote, and hands the conversation the inherited or [`None`] answer
/// [`status::insert`] derives. The counters stay at the insert's zero
/// defaults: a remote status' boosts and favourites are learned from the
/// activities this instance receives, never from what the object carried.
pub(super) fn build_new_status(
    parsed: &ParsedNote,
    reply: &ReplyLink,
    quote_of_id: Option<Uuid>,
    mentioned: &[User],
) -> NewStatus {
    NewStatus {
        ap_context: parsed.ap_context.clone(),
        hashtags: parsed.hashtags.clone(),
        mentioned_user_ids: mentioned.iter().map(|account| account.id).collect(),
        in_reply_to_id: reply.parent.as_ref().map(|parent| parent.id),
        in_reply_to_account_id: reply.parent.as_ref().map(|parent| parent.user_id),
        in_reply_to_ap_id: reply.unresolved_ap_id.clone(),
        conversation_id: reply
            .parent
            .as_ref()
            .map(|parent| parent.conversation_id)
            .or(reply.context_conversation),
        quote_of_id,
        ..parsed.status.clone()
    }
}

/// Step 7 and 8's write: status, remote media and poll on one
/// transaction, so the row goes visible with everything it shows or not at
/// all. The [module docs](crate::services::ingest) own the ordering rules
/// each arm keeps.
///
/// # Errors
///
/// [`FederationError::Transient`] for any database failure; the rollback
/// takes the half-written status, media and poll with it.
pub(super) async fn write_ingested(
    state: &AppState,
    new: &NewStatus,
    parsed: &ParsedNote,
) -> Result<Status, FederationError> {
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|err| transient("the ingest transaction", &err))?;
    let row = status::insert_conn(&mut tx, &state.config.base_url, new)
        .await
        .map_err(|err| transient("the status insert", &err))?;
    insert_media(&mut tx, row.id, &parsed.attachments).await?;
    insert_poll(&mut tx, row.id, parsed.poll.as_ref()).await?;
    tx.commit()
        .await
        .map_err(|err| transient("the ingest commit", &err))?;
    Ok(row)
}

/// The attachment arm of [`write_ingested`]: one `media_attachments` row
/// per kept document entry, on the status's open transaction.
///
/// `user_id` is `None` — nobody uploaded these, the remote post named them
/// — and the document url goes into both `remote_url` (where the media
/// proxy mirrors from) and `url` (what the entity serves until it has),
/// with the stated content type, description, `blurhash` and dimensions
/// kept as the parse read them. `position` is the document order.
///
/// The inbound edit handler writes an edited post's attachments through this
/// same function ([`crate::federation::handlers::note`]), so a mirror's
/// media looks the same whichever activity brought it.
pub(crate) async fn insert_media(
    conn: &mut PgConnection,
    status_id: Uuid,
    attachments: &[ParsedAttachment],
) -> Result<(), FederationError> {
    for (index, media) in attachments.iter().enumerate() {
        let position = i32::try_from(index).unwrap_or(i32::MAX);
        let new = NewMediaAttachment {
            user_id: None,
            status_id: Some(status_id),
            position,
            remote_url: Some(media.url.clone()),
            url: media.url.clone(),
            content_type: media.content_type.clone(),
            description: media.description.clone(),
            blurhash: media.blurhash.clone(),
            width: media.width,
            height: media.height,
            focus_x: media.focus_x,
            focus_y: media.focus_y,
            ..NewMediaAttachment::default()
        };
        media_attachment::insert(&mut *conn, &new)
            .await
            .map(drop)
            .map_err(|err| transient("the media insert", &err))?;
    }
    Ok(())
}

/// The poll arm of [`write_ingested`]: a `Question`'s [`ParsedPoll`] lands
/// exactly as the polls model defines a create — one `polls` row and one
/// `poll_options` row per option on the status's transaction — except that
/// a mirrored poll carries the counts the remote reported: each option its
/// `replies.totalItems`, the poll its `votersCount` and the options' sum.
/// A document that carried no poll writes nothing here.
///
/// The inbound edit handler grows a poll on a mirrored `Question` that
/// arrived without one through this same arm
/// ([`crate::federation::handlers::note`]).
pub(crate) async fn insert_poll(
    conn: &mut PgConnection,
    status_id: Uuid,
    question: Option<&ParsedPoll>,
) -> Result<(), FederationError> {
    let Some(poll_data) = question else {
        return Ok(());
    };
    let option_votes: Vec<i32> = poll_data
        .options
        .iter()
        .map(|option| whole(option.votes_count))
        .collect();
    let total = option_votes
        .iter()
        .fold(0_i64, |sum, votes| sum.saturating_add(i64::from(*votes)));
    let params = PollParams {
        options: poll_data
            .options
            .iter()
            .map(|option| option.title.clone())
            .collect(),
        expires_at: poll_data.expires_at,
        multiple: poll_data.multiple,
        option_votes,
        voters_count: poll_data.voters_count.map_or(0, whole),
        votes_count: whole(total),
        ..PollParams::default()
    };
    poll::create(conn, status_id, &params)
        .await
        .map_err(|err| transient("the poll insert", &err))
        .map(drop)
}

#[cfg(test)]
mod tests;
