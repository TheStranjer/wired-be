//! `GET /api/v1/custom_emojis`: the list of this instance's custom
//! emojis (Pleroma's `CustomEmojiController.index`,
//! `lib/pleroma/web/mastodon_api/controllers/custom_emoji_controller.ex`).
//!
//! The endpoint needs no authentication — Pleroma's controller `plug`s
//! `skip_auth` for `index` — and takes no parameters. It serves every
//! local, enabled emoji ordered by category then shortcode
//! ([`list_local`](crate::models::custom_emoji::list_local)), rendered as
//! the Mastodon `CustomEmoji` entity extended with Pleroma's `tags`
//! ([`crate::entities::custom_emoji`]); with no emojis stored the body is
//! the empty list `[]`.

use axum::{Json, Router, extract::State, routing::get};

use crate::{
    entities::custom_emoji::CustomEmoji, error::AppResult, models::custom_emoji, state::AppState,
};

/// The `/api/v1/custom_emojis` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new().route("/custom_emojis", get(list_custom_emojis))
}

/// `GET /api/v1/custom_emojis` — the local, enabled emojis as a JSON
/// array, ordered by category then shortcode. No authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn list_custom_emojis(State(state): State<AppState>) -> AppResult<Json<Vec<CustomEmoji>>> {
    let emojis = custom_emoji::list_local(&state.db).await?;
    Ok(Json(
        emojis
            .iter()
            .map(|row| CustomEmoji::from_model(row, &state.config))
            .collect(),
    ))
}
