//! The `attachment` array: the entries that become media rows, their
//! declared or guessed media types, pixel dimensions and focal points.

use serde_json::Value;
use url::Url;

use crate::{
    federation::json::{as_array, first_string},
    util::{format::truncate_chars, url::is_http_url},
};

use super::{ATTACHMENT_TYPES, MAX_ATTACHMENTS, types::ParsedAttachment};

/// The longest alt text kept, in characters; longer ones are cut.
const MAX_DESCRIPTION_CHARS: usize = 1500;

/// The blurhash base-83 alphabet, in digit order.
const BLURHASH_ALPHABET: &str =
    "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~";

/// The most components per axis a kept blurhash may have (Mastodon's limit).
const MAX_BLURHASH_COMPONENTS: usize = 5;

/// Whether `s` is a well-formed blurhash a client can safely decode: only
/// base-83 characters, at least six of them, a size flag giving 1 to 5
/// components per axis, and exactly the `4 + 2 * x * y` length those
/// components imply.
fn valid_blurhash(s: &str) -> bool {
    let digits: Option<Vec<usize>> = s.chars().map(|c| BLURHASH_ALPHABET.find(c)).collect();
    let Some(digits) = digits else {
        return false;
    };
    let Some(&size_flag) = digits.first() else {
        return false;
    };
    let x = size_flag % 9 + 1;
    let y = size_flag.div_euclid(9) + 1;
    digits.len() >= 6
        && x <= MAX_BLURHASH_COMPONENTS
        && y <= MAX_BLURHASH_COMPONENTS
        && digits.len() == 4 + 2 * x * y
}

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
        description: entry
            .get("name")
            .and_then(Value::as_str)
            .map(|name| truncate_chars(name.trim(), MAX_DESCRIPTION_CHARS)),
        blurhash: entry
            .get("blurhash")
            .and_then(Value::as_str)
            .filter(|candidate| valid_blurhash(candidate))
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

    /// A real 4x3-component blurhash (28 characters).
    const GOOD_BLURHASH: &str = "LEHV6nWB2yk8pyo0adR*.7kCMdnj";

    /// The blurhash `parse` keeps for an entry stating `blurhash`.
    fn parsed_blurhash(blurhash: &str) -> Option<String> {
        let mut document = minimal_note();
        document["attachment"] = json!([{
            "type": "Document",
            "url": "https://x.example/a.png",
            "blurhash": blurhash,
        }]);
        parse(&document).attachments.remove(0).blurhash
    }

    #[test]
    fn a_well_formed_blurhash_is_kept() {
        assert_eq!(
            parsed_blurhash(GOOD_BLURHASH).as_deref(),
            Some(GOOD_BLURHASH),
            "the known-good hash survives"
        );
        assert!(valid_blurhash("000000"), "1x1 components need 6 chars");
    }

    #[test]
    fn a_malformed_blurhash_is_dropped() {
        let nine_by_nine = format!("|{}", "0".repeat(165));
        assert_eq!(nine_by_nine.len(), 166, "the right length for 9x9");
        let six_by_one = format!("5{}", "0".repeat(15));
        assert_eq!(six_by_one.len(), 16, "the right length for 6x1");
        let too_short = truncate_chars(GOOD_BLURHASH, 27);
        let too_long = format!("{GOOD_BLURHASH}0");
        let spaced = GOOD_BLURHASH.replacen('n', " ", 1);
        for (label, hash) in [
            ("9x9 components", nine_by_nine.as_str()),
            ("6 components on one axis", six_by_one.as_str()),
            ("a short length", too_short.as_str()),
            ("a long length", too_long.as_str()),
            ("a space in it", spaced.as_str()),
            ("a non-alphabet char", "LEHV6nWB2yk8pyo0adR*.7kCMdn\u{e9}"),
            ("an empty string", ""),
            ("fewer than six chars", "00000"),
        ] {
            assert_eq!(parsed_blurhash(hash), None, "{label} is not stored");
            assert!(!valid_blurhash(hash), "{label} is invalid");
        }
    }

    #[test]
    fn alt_text_is_trimmed_and_capped() {
        let mut document = minimal_note();
        document["attachment"] = json!([
            {"type": "Document", "url": "https://x.example/a.png", "name": "x".repeat(5000)},
            {"type": "Document", "url": "https://x.example/b.png", "name": "  padded \n"},
            {"type": "Document", "url": "https://x.example/c.png", "name": "\u{e9}".repeat(1501)},
        ]);
        let attachments = parse(&document).attachments;
        assert_eq!(
            attachments[0]
                .description
                .as_deref()
                .map(|d| d.chars().count()),
            Some(1500),
            "5000 characters are cut to 1500"
        );
        assert_eq!(
            attachments[1].description.as_deref(),
            Some("padded"),
            "surrounding whitespace is trimmed"
        );
        assert_eq!(
            attachments[2].description,
            Some("\u{e9}".repeat(1500)),
            "the cut is by characters, not bytes"
        );
    }

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
                blurhash: Some(GOOD_BLURHASH.to_owned()),
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
