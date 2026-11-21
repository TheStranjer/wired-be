//! The `{id}` metadata routes: the read and the edit of one upload.
//!
//! `GET` and `PUT` on `/api/v1/media/{id}` and `/api/v2/media/{id}` read
//! and edit the caller's own unattached upload — the present-only merge
//! `edited_metadata` performs over the stored row, the replacement
//! preview `store_preview` writes, and `parse_focus`, the `"x,y"` parser
//! the upload endpoint reads its `focus` through too. The type gate a
//! supplied thumbnail passes is `detect_media_type` in the sibling
//! `analyze`; the constants they all read live in this module's parent.

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    config::Config,
    entities::media_attachment::MediaAttachment,
    error::{AppError, AppResult},
    extract::multipart::{MultipartForm, UploadedFile, UploadedFiles as _},
    models::media_attachment::{self, MediaMetadata},
    oauth::scopes::{READ_STATUSES, WRITE_MEDIA},
    state::AppState,
    uploads,
    util::id::parse_id,
};

use super::{
    DESCRIPTION_TOO_LONG, MAX_AXIS, MIN_AXIS, THUMBNAIL_FIELD, UPDATE_FOCUS_INVALID,
    analyze::detect_media_type,
};

/// `GET /api/v1/media/{id}` and `GET /api/v2/media/{id}` — the entity of
/// the caller's own unattached upload; unknown, foreign and already-attached
/// media are one 404.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:statuses` check; [`AppError::NotFound`] for an id that is
/// malformed, names no media, names another user's or an ownerless one, or
/// names media a status has claimed. Any database failure arrives through
/// `?` as [`AppError::Database`].
pub(super) async fn get_media(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<MediaAttachment>> {
    auth.require(READ_STATUSES)?;
    let row = own_unattached_media(&state, auth.0.id, &id).await?;
    Ok(Json(MediaAttachment::from_model(&row, &state.config)))
}

/// `PUT /api/v1/media/{id}` and `PUT /api/v2/media/{id}` — write the
/// present-only `description` and `focus`, and store a `thumbnail` file as
/// the attachment's preview, answering the updated entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:media` check; [`AppError::NotFound`] as in [`get_media`],
/// attached media included: only the status edit reaches it;
/// [`AppError::Unprocessable`] with [`DESCRIPTION_TOO_LONG`] or
/// [`UPDATE_FOCUS_INVALID`] for a body field [`edited_metadata`] refuses, or
/// with [`UNSUPPORTED_FILE_TYPE`](super::UNSUPPORTED_FILE_TYPE) for
/// thumbnail bytes [`detect_media_type`] cannot name;
/// [`AppError::PayloadTooLarge`] for a thumbnail over `UPLOAD_LIMIT_BYTES`;
/// storage and database failures arrive through `?` as
/// [`AppError::Internal`] / [`AppError::Database`].
pub(super) async fn update_media(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    mut form: MultipartForm<UpdateParams>,
) -> AppResult<Json<MediaAttachment>> {
    auth.require(WRITE_MEDIA)?;
    let current = own_unattached_media(&state, auth.0.id, &id).await?;
    let mut metadata = edited_metadata(&state.config, &current, &form.fields)?;
    if let Some(thumbnail) = form.files.take(THUMBNAIL_FIELD) {
        metadata.preview_url = Some(store_preview(&state, auth.0.id, thumbnail).await?);
    }
    // Repeat the ownership gate inside the write: media a status claimed
    // between the read and the update is no longer this endpoint's to edit.
    let Some(media_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    let updated = media_attachment::update_own_metadata(&state.db, media_id, auth.0.id, &metadata)
        .await?
        .ok_or(AppError::NotFound)?;
    debug!(
        media = %updated.id,
        user = %auth.0.id,
        "media metadata updated"
    );
    Ok(Json(MediaAttachment::from_model(&updated, &state.config)))
}

/// The row `id` names when it is the user `user_id`'s own unattached media.
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed id or a row outside the endpoint's
/// reach; [`AppError::Database`] from the lookup.
async fn own_unattached_media(
    state: &AppState,
    user_id: Uuid,
    id: &str,
) -> AppResult<media_attachment::MediaAttachment> {
    let Some(media_id) = parse_id(id) else {
        return Err(AppError::NotFound);
    };
    media_attachment::get_own_unattached(&state.db, media_id, user_id)
        .await?
        .ok_or(AppError::NotFound)
}

/// The text parameters of `PUT /media/{id}`. Everything is optional and a
/// present value replaces what is stored — a `description` or `focus` the
/// body does not carry keeps its stored value ([`edited_metadata`] merges
/// them); an empty `description` clears the alt text and an empty `focus`
/// the focal point, which is how Mastodon's clients remove either.
#[derive(Debug, Deserialize)]
pub(super) struct UpdateParams {
    /// Replacement alt text.
    #[serde(default)]
    description: Option<String>,
    /// Replacement focal point as Mastodon sends it: `"x,y"`.
    #[serde(default)]
    focus: Option<String>,
}

/// The metadata an update writes: [`UpdateParams`] merged over the stored
/// `current` row, the body's present fields winning, `description` measured
/// first: an over-long one reports [`DESCRIPTION_TOO_LONG`] before a bad
/// `focus` reports [`UPDATE_FOCUS_INVALID`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] with either message named above.
fn edited_metadata(
    config: &Config,
    current: &media_attachment::MediaAttachment,
    params: &UpdateParams,
) -> AppResult<MediaMetadata> {
    let description = updated_description(config, current, params.description.as_deref())?;
    let (focus_x, focus_y) = updated_focus(current, params.focus.as_deref())?;
    Ok(MediaMetadata {
        description,
        focus_x,
        focus_y,
        preview_url: current.preview_url.clone(),
    })
}

/// The alt text to write: `sent` when the body carried a `description`
/// (`Some("")` clears the stored text), the stored one when it did not.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`DESCRIPTION_TOO_LONG`] when `sent` is
/// longer than `config.max_media_description` characters.
fn updated_description(
    config: &Config,
    current: &media_attachment::MediaAttachment,
    sent: Option<&str>,
) -> AppResult<Option<String>> {
    match sent {
        Some(text) if text.chars().count() > config.max_media_description => {
            Err(AppError::Unprocessable(DESCRIPTION_TOO_LONG.to_owned()))
        }
        Some(text) => Ok(Some(text.to_owned())),
        None => Ok(current.description.clone()),
    }
}

/// The focal point an update writes, as a pair of optional axes: both
/// `Some` is a focal point, both `None` is none — [`updated_focus`] merges
/// the present-only parameter with the stored values, which keeps the two
/// axes in step.
type UpdatedFocus = (Option<f32>, Option<f32>);

/// The focal point to write: `sent` parsed by [`parse_focus`] (`Some("")`
/// or any blank value clears both axes, like the upload endpoint reads it),
/// the stored axes when the body carried no `focus`.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`UPDATE_FOCUS_INVALID`].
fn updated_focus(
    current: &media_attachment::MediaAttachment,
    sent: Option<&str>,
) -> AppResult<UpdatedFocus> {
    let Some(text) = sent else {
        return Ok((current.focus_x, current.focus_y));
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok((None, None));
    }
    let (x, y) = parse_focus(trimmed, UPDATE_FOCUS_INVALID)?;
    Ok((Some(x), Some(y)))
}

/// Store `file` as the attachment's preview image, answering its public
/// URL. A client-supplied preview is stored as it arrived — the analysis
/// and thumbnailing services run on upload, not on every edit — but the
/// [`detect_media_type`] gate and the detected type's extension in
/// [`uploads::media_key`] are shared with the upload endpoint, so a preview
/// is only ever a file the media route will serve as the type its bytes
/// carry. The row's stored preview dimensions keep describing the generated
/// preview; re-detecting a replaced one belongs to a later ticket.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`UNSUPPORTED_FILE_TYPE`](super::UNSUPPORTED_FILE_TYPE)
/// for bytes no accepted matcher names; storage failures arrive as
/// [`AppError::Internal`].
async fn store_preview(state: &AppState, user_id: Uuid, file: UploadedFile) -> AppResult<String> {
    let content_type = detect_media_type(&file.bytes)?;
    let key = uploads::media_key(user_id, file.filename.as_deref(), content_type);
    state.uploader.put(&key, file.bytes, content_type).await?;
    Ok(state.uploader.public_url(&key))
}

/// Mastodon's `focus` string — `"x,y"`, two floats in `[-1.0, 1.0]` — as its
/// two axes, rejected with the endpoint's own `invalid` message.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `invalid` for anything else, including a
/// missing comma, a non-numeric or non-finite axis, or an axis outside the
/// range.
pub(super) fn parse_focus(text: &str, invalid: &str) -> AppResult<(f32, f32)> {
    let Some((x, y)) = text.split_once(',') else {
        return Err(invalid_focus(invalid));
    };
    Ok((parse_axis(x, invalid)?, parse_axis(y, invalid)?))
}

/// One axis of [`parse_focus`], trimmed and inside [`MIN_AXIS`]..=[`MAX_AXIS`]
/// (`NaN` and the infinities fail the range, so they fail here).
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `invalid`.
fn parse_axis(text: &str, invalid: &str) -> AppResult<f32> {
    let Ok(value) = text.trim().parse::<f32>() else {
        return Err(invalid_focus(invalid));
    };
    if (MIN_AXIS..=MAX_AXIS).contains(&value) {
        Ok(value)
    } else {
        Err(invalid_focus(invalid))
    }
}

/// The 422 for a focal point an endpoint cannot store, in its own wording.
fn invalid_focus(message: &str) -> AppError {
    AppError::Unprocessable(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored row an update merges over: alt text, a focal point and a
    /// preview, every column [`edited_metadata`] reads filled.
    fn stored_row() -> media_attachment::MediaAttachment {
        media_attachment::MediaAttachment {
            id: Uuid::now_v7(),
            user_id: Some(Uuid::now_v7()),
            status_id: None,
            position: 0,
            storage_key: Some("9b3c1d0e4f5a6b7c.png".to_owned()),
            remote_url: None,
            url: "https://test.example/media/9b3c1d0e4f5a6b7c.png".to_owned(),
            preview_url: Some("https://test.example/media/9b3c-small.png".to_owned()),
            content_type: "image/png".to_owned(),
            description: Some("the stored alt text".to_owned()),
            blurhash: None,
            width: None,
            height: None,
            preview_width: None,
            preview_height: None,
            duration: None,
            focus_x: Some(0.25),
            focus_y: Some(-0.5),
            file_size: None,
            sensitive_hint: false,
            inserted_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// A [`Config`] capping a description at `limit` characters.
    fn config_capping_description_at(limit: usize) -> Config {
        let mut config = Config::for_tests();
        config.max_media_description = limit;
        config
    }

    /// The metadata an update of `description` and `focus` writes over
    /// [`stored_row`], panicking on a rejection the test did not expect.
    fn merged(description: Option<&str>, focus: Option<&str>) -> MediaMetadata {
        let params = UpdateParams {
            description: description.map(str::to_owned),
            focus: focus.map(str::to_owned),
        };
        edited_metadata(&Config::for_tests(), &stored_row(), &params)
            .expect("the fixture update is accepted")
    }

    /// The message an update of `description` is rejected with, panicking
    /// when it is accepted.
    fn description_rejection(description: &str) -> String {
        let params = UpdateParams {
            description: Some(description.to_owned()),
            focus: None,
        };
        edited_metadata(&config_capping_description_at(10), &stored_row(), &params)
            .expect_err("the fixture description is over the limit")
            .to_string()
    }

    /// The message an update of `focus` is rejected with, panicking when it
    /// is accepted.
    fn focus_update_rejection(focus: &str) -> String {
        let params = UpdateParams {
            description: None,
            focus: Some(focus.to_owned()),
        };
        edited_metadata(&Config::for_tests(), &stored_row(), &params)
            .expect_err("the fixture is not a focal point")
            .to_string()
    }

    #[test]
    fn an_update_without_fields_keeps_every_stored_value() {
        let metadata = merged(None, None);
        assert_eq!(
            metadata.description.as_deref(),
            Some("the stored alt text"),
            "an absent description is the stored one"
        );
        assert_eq!(metadata.focus_x, Some(0.25), "and so are the focus axes");
        assert_eq!(metadata.focus_y, Some(-0.5), "both of them");
        assert_eq!(
            metadata.preview_url.as_deref(),
            Some("https://test.example/media/9b3c-small.png"),
            "and the preview, which only a thumbnail part replaces"
        );
    }

    #[test]
    fn a_present_update_field_replaces_the_stored_one() {
        let metadata = merged(Some("a new caption"), Some("1,-1"));
        assert_eq!(
            metadata.description.as_deref(),
            Some("a new caption"),
            "the sent alt text wins"
        );
        assert_eq!(metadata.focus_x, Some(1.0), "the sent axes win");
        assert_eq!(metadata.focus_y, Some(-1.0), "both of them");

        let captioned = merged(Some("only the text"), None);
        assert_eq!(
            captioned.focus_x,
            Some(0.25),
            "and an absent focus stays stored"
        );

        let focused = merged(None, Some("0,0"));
        assert_eq!(
            focused.description.as_deref(),
            Some("the stored alt text"),
            "and an absent caption stays stored"
        );
    }

    #[test]
    fn empty_fields_clear_rather_than_fail() {
        let cleared = merged(Some(""), Some(""));
        assert_eq!(
            cleared.description.as_deref(),
            Some(""),
            "an empty caption is stored empty, which the entity renders null"
        );
        assert_eq!(cleared.focus_x, None, "an empty focus clears x");
        assert_eq!(cleared.focus_y, None, "and y");

        let spaced = merged(None, Some("   "));
        assert_eq!(
            spaced.focus_x, None,
            "a whitespace-only focus reads as empty, like the upload endpoint's"
        );
    }

    #[test]
    fn an_over_long_update_description_is_the_ticket_message() {
        // The default limit the config ships with, in characters: 1500 fit
        // and 1501 do not. The multibyte é counts once, whatever its bytes.
        let config = Config::for_tests();
        assert_eq!(config.max_media_description, 1_500, "the ticket default");
        for (description, accepted) in [
            ("x".repeat(1_500), true),
            ("é".repeat(1_500), true),
            ("x".repeat(1_501), false),
            ("é".repeat(1_501), false),
        ] {
            let params = UpdateParams {
                description: Some(description.clone()),
                focus: None,
            };
            let result = edited_metadata(&config, &stored_row(), &params);
            assert_eq!(
                result.is_ok(),
                accepted,
                "a description of {} characters is {}",
                description.chars().count(),
                if accepted { "accepted" } else { "refused" },
            );
            if !accepted {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    DESCRIPTION_TOO_LONG,
                    "the 422 is the ticket's exact message"
                );
            }
        }
    }

    #[test]
    fn the_configured_limit_decides_what_is_too_long() {
        let params = UpdateParams {
            description: Some("0123456789".to_owned()),
            focus: None,
        };
        let metadata = edited_metadata(&config_capping_description_at(10), &stored_row(), &params)
            .expect("exactly the configured limit fits");
        assert_eq!(
            metadata.description.as_deref(),
            Some("0123456789"),
            "and is stored"
        );
        assert_eq!(
            description_rejection("01234567890"),
            DESCRIPTION_TOO_LONG,
            "one character over the configured limit is the 422"
        );
        assert_eq!(
            description_rejection("éééééééééé0"),
            DESCRIPTION_TOO_LONG,
            "eleven characters are eleven whatever their bytes"
        );
    }

    #[test]
    fn an_invalid_update_focus_is_the_ticket_message() {
        for focus in [
            "0.5",
            "x,y",
            "0.5,0.5,0.5",
            "1.5,0",
            "0,-1.5",
            "nan,nan",
            "inf,0",
            ",0.5",
            "0.5,",
            "abc,def",
        ] {
            assert_eq!(
                focus_update_rejection(focus),
                UPDATE_FOCUS_INVALID,
                "`{focus}` is not a focal point, and the update says so its own way"
            );
        }
    }
}
