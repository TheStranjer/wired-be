//! The `attachment` array: the entries that become media rows, their
//! declared or guessed media types, pixel dimensions and focal points.

use serde_json::Value;
use url::Url;

use crate::federation::json::{as_array, first_string};

use super::{ATTACHMENT_TYPES, MAX_ATTACHMENTS, envelope::is_http_url, types::ParsedAttachment};

/// The kept `attachment` entries, at most [`MAX_ATTACHMENTS`] of them, in
/// document order.
pub(super) fn parse_attachments(json: &Value) -> Vec<ParsedAttachment> {
    json.get("attachment")
        .map_or_default(as_array)
        .iter()
        .filter_map(parse_attachment)
        .take(MAX_ATTACHMENTS)
        .collect()
}

/// One `attachment` entry: of an [`ATTACHMENT_TYPES`] kind, carrying an
/// `http(s)` url and either a declared `mediaType` or one guessable from
/// the url's extension; anything else is skipped rather than rejected.
fn parse_attachment(entry: &Value) -> Option<ParsedAttachment> {
    let kind = entry.get("type").and_then(Value::as_str)?;
    if !ATTACHMENT_TYPES.contains(&kind) {
        return None;
    }
    let url_value = entry.get("url")?;
    let url = first_string(url_value).filter(|candidate| is_http_url(candidate))?;
    let content_type =
        declared_media_type(entry, url_value).or_else(|| guess_content_type(&url))?;
    let (focus_x, focus_y) = focal_point(entry.get("focalPoint"));
    Some(ParsedAttachment {
        url,
        content_type,
        description: entry.get("name").and_then(Value::as_str).map(str::to_owned),
        blurhash: entry
            .get("blurhash")
            .and_then(Value::as_str)
            .map(str::to_owned),
        width: pixel_dimension(entry.get("width")),
        height: pixel_dimension(entry.get("height")),
        focus_x,
        focus_y,
    })
}

/// The declared `mediaType`: the entry's own, or the one its `url` object
/// carries (Mastodon links can hide it there), `None` when neither is a
/// non-empty string.
fn declared_media_type(entry: &Value, url_value: &Value) -> Option<String> {
    entry
        .get("mediaType")
        .or_else(|| url_value.get("mediaType"))
        .and_then(Value::as_str)
        .filter(|declared| !declared.trim().is_empty())
        .map(str::to_owned)
}

/// The `MIME` type guessed from the last path segment's file extension of
/// an `http(s)` url, `None` for an unknown one (such an attachment is
/// skipped: no type, no row).
fn guess_content_type(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let file = parsed.path().rsplit('/').next()?;
    let (_, extension) = file.rsplit_once('.')?;
    let guessed = match extension.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        _ => return None,
    };
    Some(guessed.to_owned())
}

/// A positive whole pixel measure, `None` for a missing, non-numeric,
/// zero, negative or unrepresentable value. Also read by the chat handler,
/// whose attachment entries state dimensions the same way.
pub(crate) fn pixel_dimension(value: Option<&Value>) -> Option<i32> {
    let whole = value?.as_i64()?;
    if whole <= 0 {
        return None;
    }
    i32::try_from(whole).ok()
}

/// The `focalPoint` `[x, y]` pair, each axis `None` unless the array
/// carries a JSON number at that position; numbers are read at the `f32`
/// width the media columns store, so strings and other shapes drop the
/// axis rather than failing the entry.
fn focal_point(value: Option<&Value>) -> (Option<f32>, Option<f32>) {
    let points = value.map_or_default(as_array);
    let axis = |index: usize| {
        points
            .get(index)
            .and_then(|number| serde_json::from_value::<f32>(number.clone()).ok())
    };
    (axis(0), axis(1))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::note_parser::test_support::*;

    #[test]
    fn the_mastodon_note_keeps_three_of_five_attachments() {
        let parsed = parse(&fixture(MASTODON));
        assert_eq!(
            parsed.attachments.len(),
            3,
            "the unknown extension and the Tombstone are skipped"
        );
        assert_eq!(
            parsed.attachments[0],
            ParsedAttachment {
                url: "https://remote.example/media/a.png".to_owned(),
                content_type: "image/png".to_owned(),
                description: Some("A chart".to_owned()),
                blurhash: Some("UBLf:XfQ00fQ".to_owned()),
                width: Some(1920),
                height: Some(1080),
                focus_x: Some(0.25),
                focus_y: Some(-0.5),
            },
            "the full Document entry"
        );
        assert_eq!(
            parsed.attachments[1].content_type, "video/mp4",
            "the mediaType under the url Link object is read"
        );
        assert_eq!(
            parsed.attachments[2],
            ParsedAttachment {
                url: "https://example.com/article".to_owned(),
                content_type: "text/html".to_owned(),
                description: Some("An article".to_owned()),
                blurhash: None,
                width: None,
                height: None,
                focus_x: None,
                focus_y: None,
            },
            "the Link entry keeps its declared type"
        );
    }

    /// An attachment entry with only a type and url.
    fn attachment_entry(kind: &str, url: &Value) -> Value {
        json!({"type": kind, "url": url})
    }

    #[test]
    fn attachment_shapes_are_the_loose_ones() {
        let mut document = minimal_note();
        document["attachment"] = json!([
            attachment_entry("Document", &json!(["https://x.example/a.png", "junk"])),
            attachment_entry("Image", &json!({"href": "https://x.example/b.GIF"})),
            attachment_entry("Document", &json!({"url": "https://x.example/c.webp"})),
            attachment_entry("Document", &json!("https://x.example/no-dot")),
            attachment_entry("Document", &json!("mailto:whomever@example.org")),
            attachment_entry("PropertyValue", &json!("https://x.example/d.png")),
            {"type": "Document"},
            "a bare string",
        ]);
        let parsed = parse(&document);
        assert_eq!(
            parsed
                .attachments
                .iter()
                .map(|kept| (kept.url.as_str(), kept.content_type.as_str()))
                .collect::<Vec<(&str, &str)>>(),
            [
                ("https://x.example/a.png", "image/png"),
                ("https://x.example/b.GIF", "image/gif"),
                ("https://x.example/c.webp", "image/webp"),
            ],
            "arrays and Link objects resolve; unknown extensions, non-http \
             urls, foreign types and non-objects drop out; case is folded"
        );
    }

    #[test]
    fn only_twenty_attachments_survive() {
        let mut document = minimal_note();
        document["attachment"] = json!(
            (0..25)
                .map(|n| {
                    attachment_entry("Document", &json!(format!("https://x.example/f{n}.png")))
                })
                .collect::<Vec<Value>>()
        );
        assert_eq!(parse(&document).attachments.len(), 20, "the cap is twenty");
    }
}
