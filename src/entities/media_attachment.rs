//! The Mastodon `Attachment` entity: one rendered media attachment.
//!
//! The reference shape is Pleroma's `StatusView.render("attachment.json")`
//! (`lib/pleroma/web/mastodon_api/views/status_view.ex`); the JSON keys are
//! emitted exactly as that view names them. [`MediaAttachment::from_model`]
//! is pure (decision 3): it renders a
//! [`media_attachments`](crate::models::media_attachment) row with no
//! database and no config, because every URL the entity carries is already
//! stored on the row.
//!
//! Deliberate differences from the Pleroma view, per the ticket: the
//! `type` is derived from the stored `content_type` prefix
//! (`image/*` is `image`, `video/*` is `video`, `audio/*` is `audio`,
//! anything else `unknown`; the `gifv` case — a `GIF` previewing as
//! `video/mp4` — needs transcoding no ticket has stored yet, so nothing
//! derives it), media proxying does not exist here so `url` is
//! the stored public URL and `remote_url` the stored origin, and `meta`
//! is assembled from the detection columns the row actually has, empty
//! when none are filled.

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::models::media_attachment::MediaAttachment as AttachmentRow;

/// What kind of media an attachment is, as Mastodon names it in the
/// entity's `type` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentType {
    /// An `image/*` upload or remote image (including `GIF`s, which this
    /// instance serves as plain images).
    Image,
    /// A `video/*` file.
    Video,
    /// An animated `GIF` previewing as `video/mp4`. Mastodon's type; kept
    /// for the API surface, but nothing derives it yet because the row
    /// stores one `MIME` type per attachment.
    Gifv,
    /// An `audio/*` file.
    Audio,
    /// Anything else (byte soup, unknown types).
    Unknown,
}

/// The Mastodon `Attachment` entity.
///
/// Build one with [`MediaAttachment::from_model`]; its [`Serialize`] impl
/// emits `id`, `type`, `url`, `preview_url`, `remote_url`, `text_url`,
/// `meta`, `description`, `blurhash` and the `pleroma` extension object.
#[derive(Debug, Serialize)]
pub struct MediaAttachment {
    /// The attachment id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    pub id: String,
    /// The kind of media, derived from the stored content type; the JSON
    /// key is `type`.
    #[serde(rename = "type")]
    pub media_type: AttachmentType,
    /// The public URL of the media file.
    pub url: String,
    /// The preview/thumbnail URL; falls back to [`url`](Self::url) when
    /// the row stores none.
    pub preview_url: String,
    /// The original URL of remote media; `null` for local uploads.
    pub remote_url: Option<String>,
    /// A URL clients may link to as plain text: the same as
    /// [`url`](Self::url) (Pleroma's `text_url`).
    pub text_url: String,
    /// Metadata from media detection: `original` (dimensions, `WxH` size,
    /// aspect and, for video and audio, duration), and `focus`
    /// (`{"x", "y"}`) — only the parts the row actually knows; `{}` when
    /// nothing is. A `small` variant appears once thumbnailing stores it.
    pub meta: Value,
    /// Alt text; `null` when unset or stored empty.
    pub description: Option<String>,
    /// The `Blurhash` placeholder string, if detected.
    pub blurhash: Option<String>,
    /// Pleroma's extension object: `{"mime_type": content_type}`.
    pub pleroma: Value,
}

impl MediaAttachment {
    /// Renders `row` as an `Attachment` entity.
    ///
    /// Pure (decision 3): every value comes from the row — the media type
    /// from its content type, `preview_url` falling back to its public
    /// URL, `meta` assembled from whichever detection columns are filled.
    #[must_use]
    pub fn from_model(row: &AttachmentRow) -> Self {
        let media_type = type_for_content_type(&row.content_type);
        let preview_url = row.preview_url.clone().unwrap_or_else(|| row.url.clone());
        Self {
            id: row.id.to_string(),
            media_type,
            url: row.url.clone(),
            preview_url,
            remote_url: row.remote_url.clone(),
            text_url: row.url.clone(),
            meta: meta_for(row, media_type),
            description: row.description.clone().filter(|text| !text.is_empty()),
            blurhash: row.blurhash.clone(),
            pleroma: json!({ "mime_type": row.content_type }),
        }
    }
}

/// The declared kind of `content_type`: the top-level type decides,
/// ignoring any `; codecs=…` parameters and letter case. `GIF`s stay
/// `image`; the `gifv` distinction needs a stored preview type this
/// schema does not have.
fn type_for_content_type(content_type: &str) -> AttachmentType {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    if media_type.starts_with("image/") {
        AttachmentType::Image
    } else if media_type.starts_with("video/") {
        AttachmentType::Video
    } else if media_type.starts_with("audio/") {
        AttachmentType::Audio
    } else {
        AttachmentType::Unknown
    }
}

/// Inserts `value` at `key` into a JSON object builder, discarding any
/// previous value: these maps write each key at most once.
fn put(map: &mut Map<String, Value>, key: &str, value: Value) {
    let previous = map.insert(key.to_owned(), value);
    drop(previous);
}

/// The `meta` object of an attachment: an `original` variant when any of
/// its parts are known and a `focus` one when both axes are, `{}` when
/// nothing is.
fn meta_for(row: &AttachmentRow, kind: AttachmentType) -> Value {
    let mut meta = Map::new();
    let original = original_meta(row, kind);
    if !original.is_empty() {
        put(&mut meta, "original", Value::Object(original));
    }
    if let (Some(x), Some(y)) = (row.focus_x, row.focus_y) {
        put(&mut meta, "focus", json!({ "x": x, "y": y }));
    }
    Value::Object(meta)
}

/// The `original` part of [`meta_for`]: width, height, the `WxH` size
/// string and the aspect when both dimensions are stored (the aspect is
/// skipped when the height is zero, which would divide by it), plus the
/// duration for the video-like kinds. Unknown parts are simply absent.
fn original_meta(row: &AttachmentRow, kind: AttachmentType) -> Map<String, Value> {
    let mut original = Map::new();
    if let (Some(width), Some(height)) = (row.width, row.height) {
        put(&mut original, "width", json!(width));
        put(&mut original, "height", json!(height));
        put(&mut original, "size", json!(format!("{width}x{height}")));
        if height != 0 {
            let aspect = f64::from(width) / f64::from(height);
            put(&mut original, "aspect", json!(aspect));
        }
    }
    if let Some(duration) = row.duration
        && matches!(
            kind,
            AttachmentType::Video | AttachmentType::Gifv | AttachmentType::Audio
        )
    {
        put(&mut original, "duration", json!(duration));
    }
    original
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::util::time::parse_time;

    /// The hyphenated lowercase id every fixture row carries.
    const ATTACHMENT_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// The instance origin fixture URLs point at.
    const BASE: &str = "https://test.example";

    /// Every JSON key the entity must emit, sorted.
    const EXPECTED_KEYS: [&str; 10] = [
        "blurhash",
        "description",
        "id",
        "meta",
        "pleroma",
        "preview_url",
        "remote_url",
        "text_url",
        "type",
        "url",
    ];

    /// 2024-01-31T12:34:56.789Z, the instant every fixture row is
    /// inserted and last updated at.
    fn stamped() -> DateTime<Utc> {
        parse_time("2024-01-31T12:34:56.789Z").unwrap()
    }

    /// A [`AttachmentRow`] for media stored as `key` under [`BASE`] with
    /// `content_type`: an unattached local upload by default, with
    /// everything the detection columns may hold unset.
    fn base_row(content_type: &str) -> AttachmentRow {
        AttachmentRow {
            id: Uuid::parse_str(ATTACHMENT_ID).unwrap(),
            user_id: None,
            status_id: None,
            position: 0,
            storage_key: Some("9b3c1d0e4f5a6b7c".to_owned()),
            remote_url: None,
            url: format!("{BASE}/media/9b3c1d0e4f5a6b7c"),
            preview_url: None,
            content_type: content_type.to_owned(),
            description: None,
            blurhash: None,
            width: None,
            height: None,
            duration: None,
            focus_x: None,
            focus_y: None,
            file_size: None,
            sensitive_hint: false,
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// `row` rendered to a JSON value; these structs always serialize.
    fn render(row: &AttachmentRow) -> Value {
        serde_json::to_value(MediaAttachment::from_model(row)).unwrap()
    }

    /// The rendered object's keys, sorted, for exact key-set assertions.
    fn json_keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn image_with_dimensions_and_focus_renders_the_full_snapshot() {
        let mut row = base_row("image/png");
        row.user_id = Some(Uuid::now_v7());
        row.preview_url = Some(format!("{BASE}/media/9b3c-small.png"));
        row.description = Some("A cat on a mat".to_owned());
        row.blurhash = Some("UBLHZ;9a?wWB%%.8t7Rj".to_owned());
        row.width = Some(1024);
        row.height = Some(512);
        row.focus_x = Some(0.5);
        row.focus_y = Some(-0.25);
        row.file_size = Some(345_678);
        row.sensitive_hint = true;

        let value = render(&row);
        assert_eq!(
            value,
            serde_json::json!({
                "id": ATTACHMENT_ID,
                "type": "image",
                "url": "https://test.example/media/9b3c1d0e4f5a6b7c",
                "preview_url": "https://test.example/media/9b3c-small.png",
                "remote_url": null,
                "text_url": "https://test.example/media/9b3c1d0e4f5a6b7c",
                "meta": {
                    "original": {
                        "width": 1024,
                        "height": 512,
                        "size": "1024x512",
                        "aspect": 2.0,
                    },
                    "focus": { "x": 0.5, "y": -0.25 },
                },
                "description": "A cat on a mat",
                "blurhash": "UBLHZ;9a?wWB%%.8t7Rj",
                "pleroma": { "mime_type": "image/png" },
            }),
            "image attachment snapshot"
        );
        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exact key set");
    }

    #[test]
    fn video_carries_its_duration_in_the_original_meta() {
        let mut row = base_row("video/mp4");
        row.width = Some(640);
        row.height = Some(480);
        row.duration = Some(3.75);
        row.description = Some(String::new());

        let value = render(&row);
        assert_eq!(value["type"], "video", "video/* renders as video");
        assert_eq!(
            value["meta"],
            serde_json::json!({
                "original": {
                    "width": 640,
                    "height": 480,
                    "size": "640x480",
                    "aspect": 640.0 / 480.0,
                    "duration": 3.75,
                },
            }),
            "video meta keeps dimensions and adds the duration"
        );
        assert_eq!(
            value["description"],
            Value::Null,
            "an empty stored description renders as null"
        );
        assert_eq!(
            value["preview_url"], value["url"],
            "a row without a preview falls back to the file url"
        );
        assert_eq!(
            value["pleroma"],
            serde_json::json!({ "mime_type": "video/mp4" }),
            "pleroma carries the mime type"
        );
    }

    #[test]
    fn audio_without_dimensions_renders_just_its_duration() {
        let mut row = base_row("audio/mpeg");
        row.duration = Some(61.5);

        let value = render(&row);
        assert_eq!(value["type"], "audio", "audio/* renders as audio");
        assert_eq!(
            value["meta"],
            serde_json::json!({ "original": { "duration": 61.5 } }),
            "the only known part of the meta is the duration"
        );
    }

    #[test]
    fn unknown_types_get_no_meta_and_no_duration_slot() {
        let mut row = base_row("application/octet-stream");
        row.duration = Some(9.5);
        row.focus_x = Some(0.5);

        let value = render(&row);
        assert_eq!(value["type"], "unknown", "anything else is unknown");
        assert_eq!(
            value["meta"],
            serde_json::json!({}),
            "a duration on non-av media and a half a focus point are both dropped"
        );
        assert_eq!(
            value["description"],
            Value::Null,
            "an unset description is null"
        );
        assert_eq!(value["blurhash"], Value::Null, "an unset blurhash is null");
        assert_eq!(
            value["remote_url"],
            Value::Null,
            "a local upload has no origin"
        );
    }

    #[test]
    fn remote_media_keeps_its_origin_url_and_gains_image_type() {
        let mut row = base_row("image/gif");
        row.storage_key = None;
        row.remote_url = Some("https://remote.example/media/cat.gif".to_owned());
        row.url = format!("{BASE}/media/proxy/cat.gif");
        row.width = Some(480);
        row.height = Some(270);

        let value = render(&row);
        assert_eq!(
            value["type"], "image",
            "a gif stays an image, not a gifv, until something previews it as mp4"
        );
        assert_eq!(
            value["remote_url"], "https://remote.example/media/cat.gif",
            "the stored origin passes through"
        );
        assert_eq!(
            value["url"], "https://test.example/media/proxy/cat.gif",
            "the public url is the proxied one"
        );
        assert_eq!(
            value["meta"]["original"],
            serde_json::json!({
                "width": 480,
                "height": 270,
                "size": "480x270",
                "aspect": 480.0 / 270.0,
            }),
            "detected dimensions arrive with the aspect"
        );
    }

    #[test]
    fn the_content_type_prefix_and_parameters_decide_the_type() {
        for (content_type, expected) in [
            ("image/webp", "image"),
            ("VIDEO/MP4; codecs=avc1", "video"),
            (" audio/ogg ", "audio"),
            ("text/plain", "unknown"),
            ("", "unknown"),
            ("image", "unknown"),
        ] {
            assert_eq!(
                serde_json::to_value(type_for_content_type(content_type)).unwrap(),
                serde_json::json!(expected),
                "`{content_type}` derives {expected}"
            );
        }
    }

    #[test]
    fn zero_height_skips_the_aspect_but_keeps_the_rest() {
        let mut row = base_row("image/png");
        row.width = Some(100);
        row.height = Some(0);

        assert_eq!(
            render(&row)["meta"],
            serde_json::json!({
                "original": { "width": 100, "height": 0, "size": "100x0" },
            }),
            "nothing divides by the stored zero height"
        );
    }

    #[test]
    fn the_gifv_variant_serializes_as_mastodon_names_it() {
        assert_eq!(
            serde_json::to_value(AttachmentType::Gifv).unwrap(),
            serde_json::json!("gifv"),
            "the reserved variant matches the Mastodon spelling"
        );
    }
}
