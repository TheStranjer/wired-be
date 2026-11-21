//! The document's words: the rendered `content` with its `source` and the
//! `Article`/`Page` header, the `published`/`updated` timestamps, the
//! `sensitive` flag and the shared reference reader.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{
    federation::json::value_or_id,
    util::{
        format::truncate_chars,
        sanitize::{escape_html, sanitize_html, strip_html},
        time::parse_time,
    },
};

use super::{
    CLOCK_SKEW, DEFAULT_HTML_CONTENT_TYPE, MAX_CONTENT_CHARS, TYPE_ARTICLE, TYPE_PAGE,
    types::{NoteText, NoteTimes},
};

/// The document's rendered text: `content`, or the first value of
/// `contentMap` when `content` is absent; `Article` and `Page` get the
/// title/link header Pleroma's clients render for them (the ticket's shape).
/// The result is sanitized and capped; the source text and its MIME type
/// come from `source` when it is an object stating them.
pub(super) fn parse_text(json: &Value, kind: &str, url: &str) -> NoteText {
    let body = document_body(json);
    let markup = if matches!(kind, TYPE_ARTICLE | TYPE_PAGE) {
        format!("{}{body}", article_header(json.get("title"), url))
    } else {
        body
    };
    let content = truncate_chars(&sanitize_html(&markup), MAX_CONTENT_CHARS);
    let source = json.get("source");
    let raw_text = source
        .and_then(|object| object.get("content"))
        .and_then(Value::as_str)
        .map_or_else(|| strip_html(&content), str::to_owned);
    let content_type = source
        .and_then(|object| object.get("mediaType"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_HTML_CONTENT_TYPE)
        .to_owned();
    NoteText {
        content,
        raw_text,
        content_type,
    }
}

/// The document's HTML body: the string under `content`, else the first
/// value of `contentMap`, else empty (a non-string `content` reads as
/// absent).
fn document_body(json: &Value) -> String {
    json.get("content")
        .and_then(Value::as_str)
        .or_else(|| {
            json.get("contentMap")
                .and_then(Value::as_object)
                .and_then(|entries| entries.values().next())
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_owned()
}

/// The header `Article` and `Page` documents render in front of their body:
/// the title as a bold first paragraph, then the post's url as a link. A
/// titleless article gets only the link paragraph. The result goes through
/// [`sanitize_html`] with the body, so the link gains the forced `rel` and
/// any hostile url loses its `href`.
fn article_header(title: Option<&Value>, url: &str) -> String {
    match title.and_then(Value::as_str) {
        Some(title) if !title.trim().is_empty() => format!(
            r#"<p><strong>{}</strong></p><p><a href="{url}">{url}</a></p>"#,
            escape_html(title)
        ),
        _ => format!(r#"<p><a href="{url}">{url}</a></p>"#),
    }
}

/// The timestamps: `published` clamped to `now` when it leads `now` by more
/// than [`CLOCK_SKEW`] (a remote clock cannot push a post into the future),
/// and `updated` kept as the edit time only when it postdates that.
pub(super) fn parse_times(json: &Value, now: DateTime<Utc>) -> NoteTimes {
    let ceiling = now.checked_add_signed(CLOCK_SKEW).unwrap_or(now);
    let inserted_at = timestamp_field(json, "published")
        .filter(|published| *published <= ceiling)
        .unwrap_or(now);
    let edited_at = timestamp_field(json, "updated").filter(|updated| *updated > inserted_at);
    NoteTimes {
        inserted_at,
        edited_at,
    }
}

/// The RFC 3339 timestamp under `field`, `None` when absent or
/// unparseable.
pub(super) fn timestamp_field(json: &Value, field: &str) -> Option<DateTime<Utc>> {
    json.get(field).and_then(Value::as_str).and_then(parse_time)
}

/// The `sensitive` flag with Pleroma's summary rule: the field's boolean
/// value when it carries one; otherwise a post with a non-empty `summary`
/// is sensitive, and everything else is not.
pub(super) fn parse_sensitive(json: &Value, has_spoiler: bool) -> bool {
    json.get("sensitive")
        .and_then(Value::as_bool)
        .unwrap_or(has_spoiler)
}

/// The reference carried by the first of `keys` that yields one: values are
/// read through [`value_or_id`], so a `null` (or any reference-less value)
/// under an earlier key falls through to a later one, like Pleroma's `||`
/// chains in `fix_object_defaults` and `fix_quote_url` do.
pub(super) fn field_reference(json: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| json.get(*key))
        .find_map(value_or_id)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::note_parser::test_support::*;

    #[test]
    fn the_mastodon_note_is_sanitized_and_stamped() {
        let parsed = parse(&fixture(MASTODON));
        assert!(
            parsed.status.content.contains(
                r#"<a href="https://example.org/post" rel="ugc nofollow noopener noreferrer">link</a>"#
            ),
            "the link keeps its href with the forced rel, got {}",
            parsed.status.content
        );
        assert!(
            !parsed.status.content.contains("steal"),
            "script bodies and event handlers are gone, got {}",
            parsed.status.content
        );
        assert!(!parsed.status.content.contains("target="), "target goes");
        assert_eq!(
            parsed.status.inserted_at,
            Some(parse_time("2024-02-01T10:00:00.000Z").unwrap()),
            "published is the creation time"
        );
        assert_eq!(
            parsed.status.edited_at,
            Some(parse_time("2024-02-01T10:05:00.000Z").unwrap()),
            "a later updated is the edit time"
        );
    }

    #[test]
    fn parses_an_article_into_a_rendered_post() {
        let parsed = parse(&fixture(ARTICLE));
        assert_eq!(
            parsed.status.content,
            format!(
                "<p><strong>On Parsing</strong></p><p><a href=\"{url}\" rel=\"ugc nofollow noopener noreferrer\">{url}</a></p><p>Body <em>text</em> with an <a href=\"https://example.org/ref\" rel=\"ugc nofollow noopener noreferrer\">external reference</a>.</p>",
                url = "https://remote.example/@bob/article/1"
            ),
            "bold title paragraph, url link, then the body"
        );
        assert_eq!(
            parsed.status.raw_text,
            "On Parsing\nhttps://remote.example/@bob/article/1\nBody text with an external reference.\n",
            "the rendered form as text"
        );
    }

    /// One timestamp case: the `published`/`updated` pair to try.
    fn note_with_times(published: Value, updated: Value) -> Value {
        let mut document = json!({
            "id": "https://remote.example/objects/times",
            "type": "Note",
            "attributedTo": AUTHOR_AP_ID,
        });
        if published != Value::Null {
            document["published"] = published;
        }
        if updated != Value::Null {
            document["updated"] = updated;
        }
        document
    }

    #[test]
    fn future_publish_times_clamp_to_now() {
        let skew = parse_time("2024-03-01T00:04:00Z").unwrap();
        let cases = [
            (
                "missing published is now",
                note_with_times(Value::Null, Value::Null),
                now(),
            ),
            (
                "within the skew survives",
                note_with_times(json!("2024-03-01T00:04:00Z"), Value::Null),
                skew,
            ),
            (
                "beyond the skew clamps",
                note_with_times(json!("2024-03-01T00:10:00Z"), Value::Null),
                now(),
            ),
            (
                "a broken stamp is now",
                note_with_times(json!("yesterday"), Value::Null),
                now(),
            ),
            (
                "the past is kept",
                note_with_times(json!("2024-02-29T23:00:00Z"), Value::Null),
                parse_time("2024-02-29T23:00:00Z").unwrap(),
            ),
        ];
        for (name, document, expected) in cases {
            assert_eq!(
                parse(&document).status.inserted_at,
                Some(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn only_a_later_updated_edits_the_post() {
        let published = "2024-02-28T12:00:00Z";
        let edited = parse(&note_with_times(
            json!(published),
            json!("2024-02-28T12:30:00Z"),
        ));
        assert_eq!(
            edited.status.edited_at,
            Some(parse_time("2024-02-28T12:30:00Z").unwrap()),
            "a later updated is an edit"
        );
        let stale = parse(&note_with_times(json!(published), json!(published)));
        assert_eq!(stale.status.edited_at, None, "an equal updated is not");
        let broken = parse(&note_with_times(json!(published), json!("soon")));
        assert_eq!(
            broken.status.edited_at, None,
            "an unparseable updated is dropped"
        );
    }

    #[test]
    fn content_and_spoiler_are_capped() {
        let mut document = minimal_note();
        document["content"] = json!("<p>a</p>".repeat(50_000));
        document["summary"] = json!("w".repeat(1200));
        let parsed = parse(&document);
        assert_eq!(
            parsed.status.content.chars().count(),
            200_000,
            "content caps at 200k chars"
        );
        assert_eq!(
            parsed.status.spoiler_text.chars().count(),
            1000,
            "spoiler caps at 1k chars"
        );
    }

    #[test]
    fn a_sourceless_note_strips_its_html() {
        let mut document = minimal_note();
        document["content"] = json!("<p>one</p><p>two</p>");
        let parsed = parse(&document);
        assert_eq!(
            parsed.status.raw_text, "one\ntwo\n",
            "strip_html of the content"
        );
        assert_eq!(parsed.status.content_type, "text/html", "the html default");
        remove(&mut document, "content");
        let empty = parse(&document);
        assert_eq!(
            empty.status.content, "",
            "no content and no contentMap is empty"
        );
        assert_eq!(empty.status.raw_text, "", "and so is its text");
    }

    /// One `sensitive` case: the field value to try and the outcome.
    fn note_with_sensitive(value: Option<Value>) -> Value {
        let mut document = minimal_note();
        document["summary"] = json!("cw");
        if let Some(field) = value {
            document["sensitive"] = field;
        } else {
            remove(&mut document, "sensitive");
        }
        document
    }

    #[test]
    fn sensitive_follows_the_field_then_the_summary() {
        let cases = [
            (
                "the field says true",
                note_with_sensitive(Some(json!(true))),
                true,
            ),
            (
                "the field says false",
                note_with_sensitive(Some(json!(false))),
                false,
            ),
            (
                "a string field is no field",
                note_with_sensitive(Some(json!("yes"))),
                true,
            ),
            ("no field, a summary", note_with_sensitive(None), true),
        ];
        for (name, document, expected) in cases {
            assert_eq!(parse(&document).status.sensitive, expected, "{name}");
        }
        let mut document = minimal_note();
        remove(&mut document, "summary");
        assert!(!parse(&document).status.sensitive, "no field, no summary");
    }

    #[test]
    fn references_read_their_first_usable_field() {
        let mut document = minimal_note();
        document["quoteUri"] = json!({"id": "https://x.example/q1"});
        document["context"] = Value::Null;
        document["conversation"] = json!("https://x.example/thread/9");
        let parsed = parse(&document);
        assert_eq!(
            parsed.quote_uri.as_deref(),
            Some("https://x.example/q1"),
            "quoteUri reads an object id"
        );
        assert_eq!(
            parsed.ap_context.as_deref(),
            Some("https://x.example/thread/9"),
            "a null context falls through to conversation"
        );
    }
}
