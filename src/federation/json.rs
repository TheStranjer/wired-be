//! Shared shapes of `ActivityPub` JSON-LD.
//!
//! The wire protocol is loose about cardinality: a field documented as a
//! single object often arrives as a one-element array (`publicKey` in
//! Pleroma's actors, `icon` in some feed readers), a collection reference
//! may be a bare `id` string or the whole collection object, and a `Link`
//! carries its target under `href` rather than `id`. The parsers in this
//! module's neighbours ([`super::remote_actor`] and the activity readers
//! that follow it, up to [`super::dispatch`]'s `ActivityView`) funnel every
//! such field through the same normalizers instead of hand-rolling a
//! `match` per key: [`as_array`] for "one or many", [`value_or_id`] for
//! "string or referencing object", [`first_string`] for "whatever string
//! comes first", [`first_type`] for the `type` field that may itself be an
//! array, [`references`] for an addressing property holding many
//! references, and [`http_host`] for the host of an id that containment
//! checks compare.

use serde_json::{Map, Value};
use url::Url;

/// Object keys that name a reference on a link or `Image` object, in the
/// order [`value_or_id`] and [`first_string`] try them: `id` (the
/// `ActivityStreams` identifier), `href` (the `Link` target) and `url` (the
/// `Image`/`Document` target).
const REFERENCE_KEYS: [&str; 3] = ["id", "href", "url"];

/// Every JSON value as an array view: the elements of an array, a
/// one-element slice of anything else.
///
/// This is the "may be a single object or an array of them" rule of
/// `ActivityPub` made uniform: callers iterate the result and never branch
/// on the input's shape, and a scalar (including `null`) reads as the
/// one-element array holding it, not as the empty one.
#[must_use]
pub fn as_array(value: &Value) -> &[Value] {
    value
        .as_array()
        .map_or_else(|| std::slice::from_ref(value), |values| values.as_slice())
}

/// The reference a value carries, as a string.
///
/// A string is its own reference; an object's is the first reference key
/// that holds a string (`id` first, so a collection object stands for
/// its own id, and `href` covers Mastodon's `Link` shape); an array's is its
/// first element's, matching the one-or-many rule [`as_array`] normalizes.
/// Numbers, booleans and `null`, and objects or arrays that carry no such
/// string, reference nothing.
#[must_use]
pub fn value_or_id(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => reference_key_in(map),
        Value::Array(values) => values.first().and_then(value_or_id),
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

/// The first string reachable anywhere inside a value.
///
/// A string is itself; an array is scanned front to back for the first
/// element that yields one; an object yields the first string reachable
/// under its reference keys in order (`id`, `href`, `url`;
/// recursively, so `{"url": [{"href": ...}]}` gives the `href`).
/// Everything else yields nothing. Unlike
/// [`value_or_id`] an array here is searched rather than truncated to its
/// head element's reference, which is what `icon`/`image` and `url` fields
/// with filler entries need.
#[must_use]
pub fn first_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => reference_key_in(map),
        Value::Array(values) => values.iter().find_map(first_string),
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

/// The first string of a `type` field, which may be a plain string or an
/// array of types (JSON-LD allows both): `["Create", "Service"]` reads as
/// `Create`, anything with no string anywhere reads as nothing.
#[must_use]
pub fn first_type(value: &Value) -> Option<String> {
    as_array(value)
        .iter()
        .find_map(Value::as_str)
        .map(str::to_owned)
}

/// Every reference a value carries, as strings: an array element by
/// element through [`value_or_id`], anything else alone.
///
/// A lone value becomes the one-element list of its own reference, or the
/// empty list when it names nothing. This is the normalization of the
/// addressing properties — `to`, `cc`, `bto`, `bcc` — which arrive as a
/// string, an object, or a mixed array of both.
#[must_use]
pub fn references(value: &Value) -> Vec<String> {
    as_array(value).iter().filter_map(value_or_id).collect()
}

/// The host of an absolute `http(s)` URL, `None` for anything else.
///
/// URL parsing lowercases the host, so hosts compare case-insensitively.
/// `ActivityPub` ids are URLs, and containment ([`super::dispatch`])
/// compares the host of one id against the host of another.
#[must_use]
pub fn http_host(text: &str) -> Option<String> {
    let parsed = Url::parse(text).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed.host_str().map(str::to_owned)
}

/// The first [`REFERENCE_KEYS`] entry of `map` that holds a string, resolved
/// recursively so a `{"url": [...]}` object reaches into its array.
fn reference_key_in(map: &Map<String, Value>) -> Option<String> {
    REFERENCE_KEYS
        .iter()
        .find_map(|key| map.get(*key).and_then(first_string))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn as_array_views_arrays_and_wraps_scalars() {
        assert_eq!(
            as_array(&json!([{"id": "a"}, "b"])),
            &[json!({"id": "a"}), json!("b")],
            "an array is itself"
        );
        assert_eq!(
            as_array(&json!({"id": "a"})),
            &[json!({"id": "a"})],
            "an object is the one-element view"
        );
        assert_eq!(as_array(&json!("x")), &[json!("x")], "a string too");
        assert_eq!(
            as_array(&Value::Null),
            &[Value::Null],
            "even null reads as one element, not none"
        );
        assert!(
            as_array(&json!([])).is_empty(),
            "an empty array stays empty"
        );
    }

    #[test]
    fn value_or_id_reads_strings_ids_and_hrefs() {
        assert_eq!(
            value_or_id(&json!("https://x.example/a")),
            Some("https://x.example/a".to_owned()),
            "a string is its own reference"
        );
        assert_eq!(
            value_or_id(&json!({"id": "https://x.example/a", "type": "OrderedCollection"})),
            Some("https://x.example/a".to_owned()),
            "an object references its id"
        );
        assert_eq!(
            value_or_id(&json!({"type": "Link", "href": "https://x.example/a"})),
            Some("https://x.example/a".to_owned()),
            "a Link references its href"
        );
        assert_eq!(
            value_or_id(&json!([{"id": "https://x.example/a"}, "https://x.example/b"])),
            Some("https://x.example/a".to_owned()),
            "an array reads as its first element (one-or-many)"
        );
    }

    #[test]
    fn value_or_id_prefers_id_over_href_and_needs_a_string() {
        assert_eq!(
            value_or_id(&json!({"id": "https://x.example/a", "href": "https://x.example/b"})),
            Some("https://x.example/a".to_owned()),
            "id wins"
        );
        assert_eq!(
            value_or_id(&json!({"id": 7, "href": "https://x.example/b"})),
            Some("https://x.example/b".to_owned()),
            "a non-string id falls through to href"
        );
        assert_eq!(value_or_id(&json!(7)), None, "numbers reference nothing");
        assert_eq!(value_or_id(&Value::Null), None, "null references nothing");
        assert_eq!(value_or_id(&json!({})), None, "an object with no keys");
        assert_eq!(value_or_id(&json!([])), None, "the empty array");
        assert_eq!(
            value_or_id(&json!([7, "https://x.example/b"])),
            None,
            "only the first element counts, and it references nothing"
        );
    }

    #[test]
    fn first_string_searches_arrays_and_descends_objects() {
        assert_eq!(
            first_string(&json!("s")),
            Some("s".to_owned()),
            "a string is itself"
        );
        assert_eq!(
            first_string(&json!([Value::Null, "s", "t"])),
            Some("s".to_owned()),
            "the first element with a string wins, filler skipped"
        );
        assert_eq!(
            first_string(&json!({"type": "Link", "href": "https://x.example/a"})),
            Some("https://x.example/a".to_owned()),
            "a Link yields its href"
        );
        assert_eq!(
            first_string(&json!({"url": [{"type": "Link", "href": "https://x.example/a"}]})),
            Some("https://x.example/a".to_owned()),
            "an Image object descends url -> array -> href"
        );
        assert_eq!(
            first_string(&json!([{"nope": 1}, {"href": "https://x.example/a"}])),
            Some("https://x.example/a".to_owned()),
            "the scan reaches inside object elements too"
        );
        assert_eq!(first_string(&json!([1, true])), None, "no string anywhere");
        assert_eq!(
            first_string(&json!({"id": 3})),
            None,
            "references must be strings"
        );
    }

    #[test]
    fn first_type_reads_strings_and_type_arrays() {
        assert_eq!(
            first_type(&json!("Follow")),
            Some("Follow".to_owned()),
            "a plain string type"
        );
        assert_eq!(
            first_type(&json!(["Create", "Service"])),
            Some("Create".to_owned()),
            "the first of an array type"
        );
        assert_eq!(
            first_type(&json!([])),
            None,
            "the empty array names nothing"
        );
        assert_eq!(
            first_type(&json!([3, "Like"])),
            Some("Like".to_owned()),
            "the first string of a mixed array wins"
        );
        assert_eq!(first_type(&json!(7)), None, "a number is no type");
        assert_eq!(
            first_type(&json!({"a": "b"})),
            None,
            "an object carries no string itself"
        );
    }

    #[test]
    fn references_reads_addressing_arrays_and_lone_values() {
        assert_eq!(
            references(&json!(["https://x.example/a", {"id": "https://x.example/b"}])),
            vec![
                "https://x.example/a".to_owned(),
                "https://x.example/b".to_owned()
            ],
            "a mixed addressing array keeps both references"
        );
        assert_eq!(
            references(&json!("https://x.example/a")),
            vec!["https://x.example/a".to_owned()],
            "a lone string is the one-element list"
        );
        assert_eq!(
            references(&json!([null, 3, "https://x.example/b"])),
            vec!["https://x.example/b".to_owned()],
            "elements that name nothing drop out"
        );
        assert!(
            references(&Value::Null).is_empty(),
            "null references nothing"
        );
        assert!(references(&json!([])).is_empty(), "the empty array");
    }

    #[test]
    fn http_host_reads_absolute_http_urls_only() {
        assert_eq!(
            http_host("https://Peer.Example/users/alice"),
            Some("peer.example".to_owned()),
            "the lowercased host of an https id"
        );
        assert_eq!(
            http_host("http://x.example/a"),
            Some("x.example".to_owned()),
            "http too"
        );
        assert_eq!(http_host("urn:uuid:1234"), None, "not an http url");
        assert_eq!(http_host("not a url"), None, "not a url at all");
        assert_eq!(
            http_host("ftp://x.example/a"),
            None,
            "the wrong scheme is no host"
        );
    }
}
