//! The custom emojis a delivered chat message announces in its `tag` array.
//!
//! The same `Emoji` entries a note carries: each lands in `custom_emojis`
//! under the sender's domain, so the chat API renders the message's
//! `:shortcode:` text as an emoji the way a status does.

use serde_json::Value;

use crate::{
    federation::{
        dispatch::FederationError,
        note_parser::{RemoteEmoji, emoji_tags},
    },
    models::{custom_emoji, user::User},
    state::AppState,
};

/// The `Emoji` tags of the delivered `object`, each filed under `sender`'s
/// domain; none for a document without a usable `tag`.
pub(super) fn announced(object: &Value, sender: &User) -> Vec<RemoteEmoji> {
    emoji_tags(object, sender)
}

/// Upserts `emojis` as the sender's remote custom emojis.
///
/// Idempotent, like the note ingest's: a redelivered message rewrites the
/// images it wrote the first time and leaves the operator's flags alone.
///
/// # Errors
///
/// [`FederationError::Transient`] for a database failure the retry may
/// outlive.
pub(super) async fn upsert(
    state: &AppState,
    emojis: &[RemoteEmoji],
) -> Result<(), FederationError> {
    for emoji in emojis {
        custom_emoji::upsert_remote(
            &state.db,
            &emoji.shortcode,
            emoji.domain.as_deref(),
            &emoji.url,
        )
        .await
        .map_err(|err| {
            FederationError::transient("the chat message fold failed at the emoji upsert", &err)
        })?;
    }
    Ok(())
}
