//! The `media_attachments` table: uploaded and remotely fetched media.
//!
//! One row per attachment (Pleroma keeps these as a jsonb array inside
//! each activity, `lib/pleroma/activity.ex`; the relational shape follows
//! `docs/ARCHITECTURE.md` decision 6). Local uploads carry a
//! [`storage_key`](MediaAttachment::storage_key) into
//! [`crate::uploads`] and no [`remote_url`](MediaAttachment::remote_url);
//! remote media is the other way round and has no owner until federation
//! claims one — `user_id` is `NULL` for it, and survives the deletion of
//! an uploader through `ON DELETE SET NULL`.
//!
//! [`status_id`](MediaAttachment::status_id) is a foreign key into
//! `statuses` (added by the statuses migration) and cascades on a hard
//! delete. [`attach_to_status`] is the only writer that claims media for a
//! status after the fact: it attaches exactly the rows the given user owns
//! that no status has claimed yet, each one at the position its id holds in
//! the requested list, so the array a client posted is the order the status
//! renders its attachments in.
//!
//! [`insert`] mints the primary key (`UUIDv7`, decision 4); everything
//! else comes from a [`NewMediaAttachment`], whose
//! [`Default`](NewMediaAttachment::default) mirrors the column defaults.

use chrono::{DateTime, Utc};
use uuid::Uuid;

mod inserts;
mod reads;
#[cfg(test)]
mod test_support;
mod writes;

pub use inserts::insert;
pub use reads::{for_statuses, get_by_id, get_chat_attachment, get_many, get_own_unattached};
pub use writes::{
    MEDIA_NOT_FOUND, attach_to_status, delete, delete_all_for_user, update_own_metadata,
};

/// One row of `media_attachments`: a single attached media file.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct MediaAttachment {
    /// `UUIDv7` primary key, minted by [`insert`] (decision 4).
    pub id: Uuid,
    /// Uploading user; `None` for remote media (and for rows whose
    /// uploader has since been deleted).
    pub user_id: Option<Uuid>,
    /// Status this attachment belongs to; `None` while unposted. A foreign
    /// key, so a status that is hard-deleted takes its media with it.
    pub status_id: Option<Uuid>,
    /// Order within the status's attachment list.
    pub position: i32,
    /// Key into the upload storage for a local file; `None` for remote
    /// media.
    pub storage_key: Option<String>,
    /// Original URL of remote media; `None` for local uploads.
    pub remote_url: Option<String>,
    /// Public URL clients read.
    pub url: String,
    /// Preview/thumbnail URL; `None` means clients fall back to
    /// [`url`](Self::url).
    pub preview_url: Option<String>,
    /// `MIME` type of the media, e.g. `image/png`.
    pub content_type: String,
    /// Alt text describing the media for accessibility.
    pub description: Option<String>,
    /// `Blurhash` string for the placeholder preview.
    pub blurhash: Option<String>,
    /// Width of the original media in pixels.
    pub width: Option<i32>,
    /// Height of the original media in pixels.
    pub height: Option<i32>,
    /// Width of the stored preview (the `_small` thumbnail) in pixels;
    /// `None` when the attachment has no generated preview (yet).
    pub preview_width: Option<i32>,
    /// Height of the stored preview (the `_small` thumbnail) in pixels.
    pub preview_height: Option<i32>,
    /// Length of audio and video in seconds.
    pub duration: Option<f64>,
    /// Horizontal focal point for cropping, in `[-1.0, 1.0]`.
    pub focus_x: Option<f32>,
    /// Vertical focal point for cropping, in `[-1.0, 1.0]`.
    pub focus_y: Option<f32>,
    /// Size of the local file in bytes.
    pub file_size: Option<i64>,
    /// Whether the uploader flagged the media as sensitive.
    pub sensitive_hint: bool,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every update here.
    pub updated_at: DateTime<Utc>,
}

/// What to insert: every `media_attachments` column except the `id`
/// [`insert`] mints itself and the timestamps the database stamps.
///
/// [`Default::default`] mirrors the column defaults, so a caller sets
/// only what it knows; the usual minimum is `url`, `content_type` and the
/// uploader's `user_id`.
#[derive(Clone, Debug, Default)]
pub struct NewMediaAttachment {
    /// Uploading user; `None` for remote media.
    pub user_id: Option<Uuid>,
    /// Status to attach to straight away; `None` for unposted media.
    pub status_id: Option<Uuid>,
    /// Order within the status's attachment list; `0` by default.
    pub position: i32,
    /// Upload storage key for a local file.
    pub storage_key: Option<String>,
    /// Original URL for remote media.
    pub remote_url: Option<String>,
    /// Public URL; required, empty in the default.
    pub url: String,
    /// Preview/thumbnail URL.
    pub preview_url: Option<String>,
    /// `MIME` type; required, empty in the default.
    pub content_type: String,
    /// Alt text.
    pub description: Option<String>,
    /// `Blurhash` string.
    pub blurhash: Option<String>,
    /// Width in pixels.
    pub width: Option<i32>,
    /// Height in pixels.
    pub height: Option<i32>,
    /// Width of the stored preview in pixels.
    pub preview_width: Option<i32>,
    /// Height of the stored preview in pixels.
    pub preview_height: Option<i32>,
    /// Duration in seconds (audio and video).
    pub duration: Option<f64>,
    /// Horizontal focal point.
    pub focus_x: Option<f32>,
    /// Vertical focal point.
    pub focus_y: Option<f32>,
    /// File size in bytes.
    pub file_size: Option<i64>,
    /// Sensitive flag; `false` by default, like the column.
    pub sensitive_hint: bool,
}

/// The metadata `PUT /api/v1/media/{id}` writes onto a row: alt text, the
/// focal point and the preview URL.
///
/// Like [`NewMediaAttachment`], every column is written wholesale — a `None`
/// clears what was stored. The endpoint merges present-only parameters over
/// the stored row before building one, so an absent parameter arrives here
/// as the value already kept.
#[derive(Clone, Debug, Default)]
pub struct MediaMetadata {
    /// Alt text.
    pub description: Option<String>,
    /// Horizontal focal point.
    pub focus_x: Option<f32>,
    /// Vertical focal point.
    pub focus_y: Option<f32>,
    /// Preview/thumbnail URL.
    pub preview_url: Option<String>,
}
