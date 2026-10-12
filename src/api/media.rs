//! Media over HTTP: uploading a file and editing what an upload stored.
//!
//! The upload endpoints `POST /api/v1/media` and `POST /api/v2/media` take
//! the file a client posts before it names the returned id in a status; the
//! metadata endpoints `GET` and `PUT` on `/api/v1/media/{id}` and
//! `/api/v2/media/{id}` read and edit an upload before it is posted.
//!
//! The reference is Pleroma's `MediaController`
//! (`lib/pleroma/web/mastodon_api/controllers/media_controller.ex`:
//! `create` / `create2` / `show` / `update`) and the
//! `ActivityPub.upload` the create actions call
//! (`lib/pleroma/web/activity_pub/activity_pub.ex`): a user token, a
//! multipart body whose only required part is `file`, and the Mastodon
//! `Attachment` entity of what was stored in return. Both create versions
//! answer `200 OK` with that entity: Pleroma has no asynchronous pipeline,
//! so `/api/v2/media` is not Mastodon's `202 Accepted`, it is the same
//! synchronous call on a second path.
//!
//! What happens to the bytes:
//!
//! - the type comes from the file, not from the `content-type` the part was
//!   sent with: the endpoint reads the magic bytes itself, and a file whose
//!   type it cannot name — or names outside `image/*`, `video/*` and
//!   `audio/*` — is a 422, whatever the client claimed;
//! - the file goes into [`crate::uploads`] under the key
//!   [`uploads::media_key`] mints for the uploader, and the row's `url` is
//!   [`UploaderBackend::public_url`] of that key, so the `url` of the response
//!   is exactly the address [`crate::api::media_files`] serves those bytes
//!   from;
//! - the `media_attachments` row is inserted unattached (`status_id` is
//!   `NULL`); a later `POST /api/v1/statuses` claims it through
//!   [`crate::models::media_attachment::attach_to_status`].
//!
//! Dimensions and `blurhash` belong to later tickets, so nothing fills the
//! detection columns and the entity's `meta` carries only the focal point
//! until something detects an `original`. A `thumbnail` parameter is
//! accepted and ignored on upload for the same reason.
//!
//! `GET /media/{id}` answers the [`MediaAttachment`] entity of an
//! UNATTACHED upload owned by the viewer — Mastodon's semantics: once a
//! status claims the media it is no longer retrievable here, and Pleroma
//! additionally edits an owner's ATTACHED description only through the
//! status, so attached media stays read-only on these routes. An unknown,
//! foreign or attached id is one 404 "Record not found". Mastodon answers
//! 206 while a media row has no processed meta yet; uploads here are
//! processed synchronously, so the answer is always 200. Per the ticket the
//! read scope is `read:statuses` (where Pleroma's `show` asks
//! `read:media`).
//!
//! `PUT /media/{id}` updates `description` (at most `MAX_MEDIA_DESCRIPTION`
//! characters, a longer one a 422 "Description too long"), `focus`
//! (`"x,y"`, two floats in `[-1.0, 1.0]`, anything else a 422 "Invalid
//! focus") and the `thumbnail` file part, present-only: fields the body
//! does not carry keep their stored values. No image-analysis or
//! thumbnailing service exists yet, so a thumbnail's bytes are stored
//! as-sent and only `preview_url` points at them. Neither route deletes
//! anything: there is no `DELETE /media/{id}`.
//!
//! The rejections are those of decision 8: 401 / 403 from [`AuthUser`] and
//! the scope check, 404 "Record not found" on the `{id}` routes for anything
//! that is not the caller's own unattached media, 413 for a file over
//! `UPLOAD_LIMIT_BYTES` — enforced by [`crate::extract::multipart`], which
//! [`upload_body_limit`] must free from axum's 2 MiB extractor default first
//! — and 422 for a missing or empty `file` part, an undetectable type, a
//! `focus` that is not two floats in `[-1.0, 1.0]`, or an update
//! `description` over `MAX_MEDIA_DESCRIPTION`.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    config::Config,
    entities::media_attachment::MediaAttachment,
    error::{AppError, AppResult},
    extract::multipart::{MultipartForm, UploadedFile, UploadedFiles as _, upload_body_limit},
    models::media_attachment::{self, MediaMetadata, NewMediaAttachment},
    oauth::scopes::{READ_STATUSES, WRITE_MEDIA},
    state::AppState,
    uploads::{self, UploaderBackend},
    util::id::parse_id,
};

/// The form field the file is read from (Pleroma's required `:file`).
const FILE_FIELD: &str = "file";
/// 422 body when no file part arrived under [`FILE_FIELD`], the wording the
/// ticket fixes for Pleroma's missing-required-parameter rejection.
const MISSING_FILE: &str = "Missing parameter: file";
/// 422 body for a file part that carries no bytes (Mastodon's changeset
/// wording for a blank attachment).
const BLANK_FILE: &str = "Validation failed: File can't be blank";
/// 422 body for a focal point that is not `"x,y"` with both axes in
/// `[-1.0, 1.0]` (Mastodon's changeset wording again).
const FOCUS_INVALID: &str = "Validation failed: Focus is invalid";
/// 422 body for bytes [`detect_media_type`] cannot name an accepted media
/// type for.
const UNSUPPORTED_FILE_TYPE: &str = "Unsupported file type";
/// Lower bound of a focal-point axis, Mastodon's inclusive range.
const MIN_AXIS: f32 = -1.0;
/// Upper bound of a focal-point axis, likewise inclusive.
const MAX_AXIS: f32 = 1.0;
/// The file part `PUT /media/{id}` reads its replacement preview from
/// (Mastodon's `thumbnail`).
const THUMBNAIL_FIELD: &str = "thumbnail";
/// 422 body when an update's `description` is longer than
/// `MAX_MEDIA_DESCRIPTION`, the wording the ticket fixes.
const DESCRIPTION_TOO_LONG: &str = "Description too long";
/// 422 body when an update's `focus` is not two floats in `[-1.0, 1.0]`,
/// likewise — the update endpoint's own wording, not [`FOCUS_INVALID`]'s
/// changeset phrase.
const UPDATE_FOCUS_INVALID: &str = "Invalid focus";

/// The `/media` router, merged into both [`crate::api::v1`] and
/// [`crate::api::v2`]: the two upload paths and the `{id}` metadata routes
/// `get_media` and `update_media` serve on either version.
///
/// `config` is what [`upload_body_limit`] sizes the routes' body limit from:
/// axum's multipart extractor refuses bodies over 2 MiB unless the route
/// raises that, and what it must raise it to is `UPLOAD_LIMIT_BYTES` times
/// `UPLOAD_MAX_FILES`. `PUT /media/{id}` takes a `thumbnail` file through
/// the same extractor, so the limit covers its routes too. This is the one
/// area router that cannot be built from nothing.
pub fn router(config: &Config) -> Router<AppState> {
    Router::new()
        .route("/media", post(upload_media))
        .route("/media/{id}", get(get_media).put(update_media))
        .layer(upload_body_limit(config))
}

/// `POST /api/v1/media` and `POST /api/v2/media` — store the uploaded file,
/// record it as the caller's unattached media and answer its entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:media` check; [`AppError::PayloadTooLarge`] from the multipart
/// extractor for a file over `UPLOAD_LIMIT_BYTES`;
/// [`AppError::Unprocessable`] from [`Upload::from_form`] for a missing or
/// empty file, an undetectable type or a malformed `focus`; storage and
/// database failures arrive through `?` as
/// [`AppError::Internal`] / [`AppError::Database`].
async fn upload_media(
    State(state): State<AppState>,
    auth: AuthUser,
    form: MultipartForm<UploadParams>,
) -> AppResult<Json<MediaAttachment>> {
    auth.require(WRITE_MEDIA)?;
    let upload = Upload::from_form(form)?;
    let key = uploads::media_key(
        auth.0.id,
        upload.file.filename.as_deref(),
        upload.content_type,
    );
    state
        .uploader
        .put(&key, upload.file.bytes.clone(), upload.content_type)
        .await?;
    let new = upload.new_attachment(auth.0.id, &state.uploader, &key)?;
    let row = media_attachment::insert(&state.db, &new).await?;
    debug!(
        media = %row.id,
        user = %auth.0.id,
        %upload.content_type,
        "media uploaded"
    );
    Ok(Json(MediaAttachment::from_model(&row)))
}

/// The text parameters of a media upload: everything beside the file.
///
/// A `thumbnail` part is not a field of this struct on purpose: serde
/// ignores unknown keys, so the parameter (and a file part of that name,
/// which never reaches `fields`) is accepted and dropped until thumbnailing
/// exists.
#[derive(Debug, Deserialize)]
struct UploadParams {
    /// Alt text describing the media for accessibility.
    #[serde(default)]
    description: Option<String>,
    /// The focal point as Mastodon sends it: `"x,y"`.
    #[serde(default)]
    focus: Option<String>,
}

impl UploadParams {
    /// The focal point to store: [`parse_focus`] of `focus` when one was
    /// sent, `None` for an absent or blank one — clients send `focus=` for
    /// "no focal point", which is not an invalid value.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] with [`FOCUS_INVALID`] for a string that
    /// is not two floats in [`MIN_AXIS`]..=[`MAX_AXIS`].
    fn focal_point(&self) -> AppResult<Option<(f32, f32)>> {
        self.focus
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| parse_focus(text, FOCUS_INVALID))
            .transpose()
    }
}

/// An upload that passed validation, ready to be stored.
#[derive(Debug)]
struct Upload {
    /// The `file` part: name, bytes and the client's own file name.
    file: UploadedFile,
    /// Alt text as sent.
    description: Option<String>,
    /// Focal point as sent.
    focus: Option<(f32, f32)>,
    /// Media type read from the file's bytes.
    content_type: &'static str,
}

impl Upload {
    /// Validates the parts of `form`: the `file` part must be there, must
    /// carry bytes, and must be bytes that name an accepted media type;
    /// `focus`, if present, must parse.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] with [`MISSING_FILE`], [`BLANK_FILE`],
    /// [`UNSUPPORTED_FILE_TYPE`] or [`FOCUS_INVALID`], in that order.
    fn from_form(mut form: MultipartForm<UploadParams>) -> AppResult<Self> {
        let file = form
            .files
            .take(FILE_FIELD)
            .ok_or_else(|| AppError::Unprocessable(MISSING_FILE.to_owned()))?;
        if file.bytes.is_empty() {
            return Err(AppError::Unprocessable(BLANK_FILE.to_owned()));
        }
        let content_type = detect_media_type(&file.bytes)?;
        let focus = form.fields.focal_point()?;
        let description = form.fields.description;
        Ok(Self {
            file,
            description,
            focus,
            content_type,
        })
    }

    /// The row this upload inserts: the caller's media, stored at `key` and
    /// served at the uploader's public URL for it, attached to no status.
    ///
    /// # Errors
    ///
    /// [`AppError::Internal`] when the file's size is too large for the
    /// `bigint` column.
    fn new_attachment(
        &self,
        user_id: Uuid,
        uploader: &UploaderBackend,
        key: &str,
    ) -> AppResult<NewMediaAttachment> {
        let file_size = i64::try_from(self.file.bytes.len()).map_err(|err| {
            AppError::Internal(format!("uploaded file size is unrecordable: {err}"))
        })?;
        let (focus_x, focus_y) = self.focus.unzip();
        Ok(NewMediaAttachment {
            user_id: Some(user_id),
            status_id: None,
            storage_key: Some(key.to_owned()),
            url: uploader.public_url(key),
            content_type: self.content_type.to_owned(),
            description: self.description.clone(),
            focus_x,
            focus_y,
            file_size: Some(file_size),
            ..NewMediaAttachment::default()
        })
    }
}

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
async fn get_media(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<MediaAttachment>> {
    auth.require(READ_STATUSES)?;
    let row = own_unattached_media(&state, auth.0.id, &id).await?;
    Ok(Json(MediaAttachment::from_model(&row)))
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
/// with [`UNSUPPORTED_FILE_TYPE`] for thumbnail bytes [`detect_media_type`]
/// cannot name; [`AppError::PayloadTooLarge`] for a thumbnail over
/// `UPLOAD_LIMIT_BYTES`; storage and database failures arrive through `?` as
/// [`AppError::Internal`] / [`AppError::Database`].
async fn update_media(
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
    Ok(Json(MediaAttachment::from_model(&updated)))
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
struct UpdateParams {
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
/// URL. No image-analysis or thumbnailing service exists yet, so the bytes
/// are stored as they arrived; the [`detect_media_type`] gate and the
/// detected type's extension in [`uploads::media_key`] are shared with the
/// upload endpoint, so a preview is only ever a file the media route will
/// serve as the type its bytes carry.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`UNSUPPORTED_FILE_TYPE`] for bytes no
/// accepted matcher names; storage failures arrive as
/// [`AppError::Internal`].
async fn store_preview(state: &AppState, user_id: Uuid, file: UploadedFile) -> AppResult<String> {
    let content_type = detect_media_type(&file.bytes)?;
    let key = uploads::media_key(user_id, file.filename.as_deref(), content_type);
    state.uploader.put(&key, file.bytes, content_type).await?;
    Ok(state.uploader.public_url(&key))
}

/// The media type of an upload's bytes, from the bytes themselves: the first
/// `infer` match whose category is image, video or audio.
///
/// Not the part's `content-type`: a client may claim anything, and what is
/// stored here is what [`crate::uploads`] names the key after, what the
/// entity reports and what a later status federates.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`UNSUPPORTED_FILE_TYPE`] when nothing
/// matches the bytes or what matches is not one of the three accepted
/// categories.
fn detect_media_type(bytes: &[u8]) -> AppResult<&'static str> {
    let Some(detected) = infer::get(bytes).filter(|kind| is_accepted(kind.matcher_type())) else {
        return Err(AppError::Unprocessable(UNSUPPORTED_FILE_TYPE.to_owned()));
    };
    Ok(detected.mime_type())
}

/// Whether an `infer` category is one of the three a media upload may be.
const fn is_accepted(category: infer::MatcherType) -> bool {
    matches!(
        category,
        infer::MatcherType::Image | infer::MatcherType::Video | infer::MatcherType::Audio
    )
}

/// Mastodon's `focus` string — `"x,y"`, two floats in `[-1.0, 1.0]` — as its
/// two axes, rejected with the endpoint's own `invalid` message.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `invalid` for anything else, including a
/// missing comma, a non-numeric or non-finite axis, or an axis outside the
/// range.
fn parse_focus(text: &str, invalid: &str) -> AppResult<(f32, f32)> {
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
    use axum::body::Bytes;

    use super::*;

    // The fixtures name a type for the detector and nothing more: none of
    // them is a complete file, because detection reads magic bytes and this
    // module never opens the container.
    /// A `PNG` signature and the start of an `IHDR` chunk.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
    /// A `JPEG`: `SOI` followed by the `APP0` marker.
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01";
    /// An `MP3`: an `ID3v2` tag header with a zero-length tag.
    const MP3: &[u8] = b"ID3\x03\x00\x00\x00\x00\x00\x00";
    /// An `MP4`: an `ftyp` box with the `mp42` brand.
    const MP4: &[u8] = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom";
    /// A `PDF`: bytes with a type, just not one of the three accepted ones.
    const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
    /// Byte soup: nothing `infer` can name.
    const NOTHING: &[u8] = b"just some plain text, definitely not media\n";
    /// A truncated `PNG` signature: the start of a file every matcher needs
    /// more of.
    const TOO_SHORT: &[u8] = b"\x89P";
    /// No bytes at all, which is what a zero-length part holds.
    const EMPTY: &[u8] = b"";

    /// The message `bytes` is rejected with, panicking when they are not.
    fn rejection(bytes: &[u8]) -> String {
        detect_media_type(bytes)
            .expect_err("the fixture is not accepted media")
            .to_string()
    }

    /// The message `focus` is rejected with, panicking when it is not.
    fn focus_rejection(focus: &str) -> String {
        UploadParams {
            description: None,
            focus: Some(focus.to_owned()),
        }
        .focal_point()
        .expect_err("the fixture is not a focal point")
        .to_string()
    }

    /// The focal point `focus` reads as, panicking when it does not parse.
    fn focus_axes(focus: &str) -> Option<(f32, f32)> {
        UploadParams {
            description: None,
            focus: Some(focus.to_owned()),
        }
        .focal_point()
        .expect("the fixture is a valid focal point")
    }

    /// A file part named `file` holding `bytes`, sent as `client_type`.
    fn file_part(filename: &str, client_type: &str, bytes: &[u8]) -> UploadedFile {
        UploadedFile {
            field: FILE_FIELD.to_owned(),
            filename: Some(filename.to_owned()),
            content_type: Some(client_type.to_owned()),
            bytes: Bytes::copy_from_slice(bytes),
        }
    }

    /// An upload of `bytes` under `filename`, with `description` and `focus`
    /// as the body's text fields.
    fn form(
        description: Option<&str>,
        focus: Option<&str>,
        file: UploadedFile,
    ) -> MultipartForm<UploadParams> {
        let mut fields = Vec::new();
        if let Some(text) = description {
            fields.push(("description".to_owned(), text.to_owned()));
        }
        if let Some(text) = focus {
            fields.push(("focus".to_owned(), text.to_owned()));
        }
        let params = crate::extract::into_params(crate::extract::parse_pairs(fields))
            .expect("the fixture parameters deserialize");
        MultipartForm {
            fields: params,
            files: vec![file],
        }
    }

    /// The uploader an attachment is built with: a prefix only, because
    /// building a row reads and writes nothing.
    fn uploader() -> UploaderBackend {
        UploaderBackend::from(&Config::for_tests())
    }

    /// The row shape an upload of `bytes` named `filename` produces.
    fn attachment_for(filename: &str, bytes: &[u8]) -> NewMediaAttachment {
        let upload = Upload::from_form(form(
            Some("a cat on a mat"),
            Some("0.25,-0.5"),
            file_part(filename, "application/octet-stream", bytes),
        ))
        .expect("the fixture is a valid upload");
        upload
            .new_attachment(Uuid::now_v7(), &uploader(), "u/9b3c1d0e4f5a6b7c.png")
            .expect("the fixture size is recordable")
    }

    #[test]
    fn the_detected_type_is_the_media_type_of_the_bytes() {
        for (bytes, content_type) in [
            (PNG, "image/png"),
            (JPEG, "image/jpeg"),
            (MP3, "audio/mpeg"),
            (MP4, "video/mp4"),
        ] {
            assert_eq!(
                detect_media_type(bytes).expect("the fixture is accepted media"),
                content_type,
                "the type comes from the magic bytes"
            );
        }
    }

    #[test]
    fn anything_that_is_not_image_video_or_audio_is_rejected() {
        for bytes in [PDF, NOTHING, TOO_SHORT, EMPTY] {
            assert_eq!(
                rejection(bytes),
                UNSUPPORTED_FILE_TYPE,
                "undetectable or non-media bytes are a 422"
            );
        }
    }

    #[test]
    fn focus_parses_the_spellings_clients_send() {
        assert_eq!(focus_axes("0,0"), Some((0.0, 0.0)), "the centre");
        assert_eq!(
            focus_axes("-0.5,0.25"),
            Some((-0.5, 0.25)),
            "negative x, fractional y"
        );
        assert_eq!(
            focus_axes(" 1.0 , -1 "),
            Some((1.0, -1.0)),
            "whitespace is trimmed and both ends of the range are inclusive"
        );
        assert_eq!(
            focus_axes("0.333333,0.666667"),
            Some((0.333_333, 0.666_667)),
            "a fraction is kept as parsed"
        );
    }

    #[test]
    fn an_absent_or_blank_focus_stores_nothing() {
        assert_eq!(
            UploadParams {
                description: None,
                focus: None,
            }
            .focal_point()
            .expect("absent is not invalid"),
            None,
            "no parameter, no focal point"
        );
        assert_eq!(focus_axes("   "), None, "a blank parameter reads as absent");
    }

    #[test]
    fn an_invalid_focus_is_rejected_with_mastodon_wording() {
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
                focus_rejection(focus),
                FOCUS_INVALID,
                "`{focus}` is not a focal point"
            );
        }
    }

    #[test]
    fn a_valid_upload_carries_its_detected_type_and_fields() {
        let new = attachment_for("cat.png", PNG);
        assert!(new.status_id.is_none(), "fresh media is unattached");
        assert!(
            new.storage_key.is_some(),
            "the storage key is what the row records"
        );
        assert_eq!(
            new.url, "https://test.example/media/u/9b3c1d0e4f5a6b7c.png",
            "the url is the uploader's public url for the key"
        );
        assert_eq!(
            new.content_type, "image/png",
            "the stored type is the detected one, not the sent one"
        );
        assert_eq!(
            new.description.as_deref(),
            Some("a cat on a mat"),
            "alt text is stored as sent"
        );
        assert_eq!(new.focus_x, Some(0.25), "the x axis is stored");
        assert_eq!(new.focus_y, Some(-0.5), "and the y axis");
        assert_eq!(
            new.file_size,
            Some(i64::try_from(PNG.len()).expect("the fixture size is recordable")),
            "the byte count of the stored file"
        );
        assert!(new.preview_url.is_none(), "thumbnailing is later");
        assert!(new.remote_url.is_none(), "a local upload has no origin");
    }

    #[test]
    fn the_key_extension_follows_the_detected_type() {
        for (bytes, extension) in [(PNG, "png"), (JPEG, "jpg"), (MP3, "mp3"), (MP4, "mp4")] {
            let upload =
                Upload::from_form(form(None, None, file_part("photo.png", "image/png", bytes)))
                    .expect("the fixture is a valid upload");
            let key = uploads::media_key(
                Uuid::now_v7(),
                upload.file.filename.as_deref(),
                upload.content_type,
            );
            let (_, tail) = key.rsplit_once('.').expect("a key has an extension");
            assert_eq!(tail, extension, "detected type wins over the name: {key}");
        }
    }

    #[test]
    fn a_missing_file_part_names_the_parameter_it_wanted() {
        let empty: MultipartForm<UploadParams> = MultipartForm {
            fields: UploadParams {
                description: Some("no file".to_owned()),
                focus: None,
            },
            files: Vec::new(),
        };
        let err = Upload::from_form(empty).expect_err("a body without a file");
        assert_eq!(err.to_string(), MISSING_FILE, "the 422 names `file`");
    }

    #[test]
    fn a_file_part_without_bytes_is_blank_not_missing() {
        let err = Upload::from_form(form(None, None, file_part("empty.png", "image/png", b"")))
            .expect_err("a zero-length file is rejected");
        assert_eq!(err.to_string(), BLANK_FILE, "the 422 is about the content");
    }

    #[test]
    fn a_file_under_another_field_name_is_no_file_at_all() {
        let mut file = file_part("cat.png", "image/png", PNG);
        file.field = "attachment".to_owned();
        let err = Upload::from_form(form(None, None, file)).expect_err("only `file` counts");
        assert_eq!(
            err.to_string(),
            MISSING_FILE,
            "a differently named part does not fill in"
        );
    }

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
