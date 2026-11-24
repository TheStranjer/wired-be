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
//!   [`uploads::media_key`](crate::uploads::media_key) mints for the
//!   uploader, and the row's `url` is
//!   [`UploaderBackend::public_url`](crate::uploads::UploaderBackend::public_url)
//!   of that key, so the `url` of the response is exactly the address
//!   [`crate::api::media_files`] serves those bytes from;
//! - the `media_attachments` row is inserted unattached (`status_id` is
//!   `NULL`); a later `POST /api/v1/statuses` claims it through
//!   [`crate::models::media_attachment::attach_to_status`].
//!
//! An `image/*` upload is analysed through [`crate::services::media_processing`]
//! on the blocking pool before anything is stored: its header must decode
//! (an undecodable one is a 422 "Invalid image") and its pixel count must
//! stay within `MAX_IMAGE_PIXELS` (over it is a 422 "Image too large").
//! The decoded dimensions are recorded on the row, and the endpoint then
//! generates the attachment's preview — the same key's stem with a
//! `_small.jpg` / `_small.png` suffix, encoded JPEG for an opaque image,
//! PNG when it has transparency to keep — and a `Blurhash` string. Those
//! two extras are best-effort: a thumbnail or hash that fails is logged
//! and the upload answers without it. A `thumbnail` parameter or file part
//! on upload is still accepted and ignored: the preview is generated, not
//! supplied.
//!
//! `GET /media/{id}` answers the
//! [`MediaAttachment`](crate::entities::media_attachment::MediaAttachment)
//! entity of an UNATTACHED upload owned by the viewer — Mastodon's
//! semantics: once a status claims the media it is no longer retrievable
//! here, and Pleroma additionally edits an owner's ATTACHED description
//! only through the status, so attached media stays read-only on these
//! routes. An unknown, foreign or attached id is one 404 "Record not
//! found". Mastodon answers 206 while a media row has no processed meta
//! yet; uploads here are processed synchronously, so the answer is always
//! 200. Per the ticket the read scope is `read:statuses` (where Pleroma's
//! `show` asks `read:media`).
//!
//! `PUT /media/{id}` updates `description` (at most `MAX_MEDIA_DESCRIPTION`
//! characters, a longer one a 422 "Description too long"), `focus`
//! (`"x,y"`, two floats in `[-1.0, 1.0]`, anything else a 422 "Invalid
//! focus") and the `thumbnail` file part, present-only: fields the body
//! does not carry keep their stored values. A supplied thumbnail's bytes
//! are stored as-sent (the analysis and thumbnailing services run on
//! upload, not on every edit), and only `preview_url` points at them.
//! Neither route deletes anything: there is no `DELETE /media/{id}`.
//!
//! The rejections are those of decision 8: 401 / 403 from
//! [`AuthUser`](crate::auth::AuthUser) and the scope check, 404 "Record not
//! found" on the `{id}` routes for anything that is not the caller's own
//! unattached media, 413 for a file over `UPLOAD_LIMIT_BYTES` — enforced
//! by [`crate::extract::multipart`], which [`upload_body_limit`] must free
//! from axum's 2 MiB extractor default first — and 422 for a missing or
//! empty `file` part, an undetectable type, an undecodable or oversized
//! image, a `focus` that is not two floats in `[-1.0, 1.0]`, or an update
//! `description` over `MAX_MEDIA_DESCRIPTION`.
//!
//! This module is a tree. `upload` holds the upload endpoint, the shared
//! processing pipeline and the request shapes a posted file passes
//! through; `analyze` holds the image analysis, the generated preview and
//! the magic-byte type gate; `update` holds the `{id}` metadata routes,
//! their merge over the stored row and the `focus` parser the upload
//! endpoint borrows from it; `test_support` holds the unit-test fixtures
//! the children share. This root keeps the constants, the router and the
//! re-exports that keep the pipeline importable from here.

use axum::{
    Router,
    routing::{get, post},
};

use crate::{
    config::Config,
    error::{AppError, AppResult},
    extract::multipart::upload_body_limit,
    state::AppState,
};

use self::{
    update::{get_media, update_media},
    upload::upload_media,
};

pub(crate) use self::upload::{UploadParams, store_upload};

mod analyze;
#[cfg(test)]
mod test_support;
mod update;
pub mod upload;

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
/// 422 body for bytes
/// [`detect_media_type`](analyze::detect_media_type) cannot name an
/// accepted media type for.
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
/// Edge bound of the generated preview image, in pixels (the ticket's 400):
/// the longest edge of a thumbnail is scaled down to this, never up.
const THUMBNAIL_MAX_EDGE: u32 = 400;

/// Refuses alt text longer than `config.max_media_description` characters,
/// the one rule the upload and the update path both enforce.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`DESCRIPTION_TOO_LONG`] when `text` has
/// more characters (not bytes) than the configured limit.
fn check_description(config: &Config, text: &str) -> AppResult<()> {
    if text.chars().count() > config.max_media_description {
        return Err(AppError::Unprocessable(DESCRIPTION_TOO_LONG.to_owned()));
    }
    Ok(())
}

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
