//! The one parser for a `Content-Type` value's media type.
//!
//! Every gate that reads the header (the inline media-proxy check, the inbox
//! type check, the `WebFinger` client, the upload extension lookup, the
//! attachment kind) goes through [`essence`], so they all agree on where the
//! media type ends and how it is normalized.

/// The top-level types a media attachment can have, in the order
/// [`top_level`] reports them.
const MEDIA_TOP_LEVELS: [&str; 3] = ["image", "video", "audio"];

/// The media type of a `Content-Type` value: everything before the first
/// `;`, trimmed and lowercased. Empty when there is nothing before it.
#[must_use]
pub fn essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// The top-level type of a media `Content-Type`: `"image"`, `"video"` or
/// `"audio"` when its [`essence`] starts with that type and a `/`; [`None`]
/// for anything else.
#[must_use]
pub fn top_level(content_type: &str) -> Option<&'static str> {
    let media_type = essence(content_type);
    MEDIA_TOP_LEVELS.into_iter().find(|top| {
        media_type
            .strip_prefix(top)
            .is_some_and(|rest| rest.starts_with('/'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn essence_drops_parameters_and_lowercases() {
        assert_eq!(essence("Image/PNG; charset=x"), "image/png");
    }

    #[test]
    fn essence_of_an_empty_string_is_empty() {
        assert_eq!(essence(""), "");
    }

    #[test]
    fn essence_without_parameters_passes_through_trimmed() {
        assert_eq!(essence("  text/HTML  "), "text/html");
    }

    #[test]
    fn essence_of_only_parameters_is_empty() {
        assert_eq!(essence("; charset=utf-8"), "");
    }

    #[test]
    fn top_level_names_the_media_prefixes() {
        assert_eq!(top_level("image/png"), Some("image"));
        assert_eq!(top_level("Video/MP4; codecs=avc1"), Some("video"));
        assert_eq!(top_level(" audio/ogg "), Some("audio"));
    }

    #[test]
    fn top_level_is_none_for_other_types() {
        assert_eq!(top_level("application/json"), None);
        assert_eq!(top_level(""), None);
        assert_eq!(top_level("image"), None);
        assert_eq!(top_level("imagery/png"), None);
    }
}
