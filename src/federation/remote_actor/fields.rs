//! The optional-field readers of an actor document: URLs, media, profile
//! fields, aliases and timestamps, each dropping what is malformed.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::{ActorError, PROPERTY_VALUE_TYPE};
use crate::{
    federation::json::{as_array, first_string, value_or_id},
    models::user::Flag,
    util::{time::parse_time, url::is_http_url},
};

/// The required URL of a field (`inbox`): a string or reference object
/// spelling an absolute `http(s)` URL, via [`value_or_id`].
pub(super) fn required_http_url(json: &Value, field: &str) -> Result<String, ActorError> {
    optional_http_url(json, field)
        .ok_or_else(|| ActorError::Invalid(format!("`{field}` is missing or not an http(s) URL")))
}

/// An optional URL field (`outbox`, `followers`, `following`, `featured`,
/// `movedTo`): [`value_or_id`] reads strings, `{"id": ...}` collection
/// objects and one-or-many arrays; a value that carries no `http(s)` URL is
/// dropped rather than rejected.
pub(super) fn optional_http_url(json: &Value, field: &str) -> Option<String> {
    json.get(field)
        .and_then(value_or_id)
        .filter(|url| is_http_url(url))
}

/// A text field (`name`, `summary`): the string under `field`, empty for
/// any other shape.
pub(super) fn string_field(json: &Value, field: &str) -> String {
    json.get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A boolean flag field (`manuallyApprovesFollowers`, `discoverable`):
/// `true`/`false` as sent, and `false` for anything else, Pleroma's
/// `data["key"] || false`.
pub(super) fn bool_field(json: &Value, field: &str) -> Flag {
    Flag::new(json.get(field).and_then(Value::as_bool).unwrap_or_default())
}

/// The `icon`/`image` URL of `field`: the value may be a bare URL string,
/// an `Image` object whose `url` is a string, a `Link` object or an array
/// of either (all three spellings are on the wire), or an array of any of
/// those; the first entry yielding an `http(s)` URL wins, anything else is
/// dropped.
pub(super) fn parse_media_url(json: &Value, field: &str) -> Option<String> {
    let value = json.get(field)?;
    as_array(value).iter().find_map(|element| {
        let url = element
            .as_object()
            .and_then(|object| object.get("url"))
            .map_or_else(|| first_string(element), first_string);
        url.filter(|url| is_http_url(url))
    })
}

/// The `attachment` list as the stored `fields` value: a JSON array of the
/// `{name, value}` objects of every `PropertyValue` entry, in order
/// (Pleroma's `Map.take(fields, ["name", "value"])`). Entries of any other
/// shape are skipped.
pub(super) fn parse_fields(json: &Value) -> Value {
    let properties = json
        .get("attachment")
        .map_or_default(as_array)
        .iter()
        .filter_map(property_value)
        .collect();
    Value::Array(properties)
}

/// One `attachment` entry as a stored `{name, value}` field, `None` unless
/// it is a `PropertyValue` object carrying both as strings.
fn property_value(entry: &Value) -> Option<Value> {
    if entry.get("type").and_then(Value::as_str) != Some(PROPERTY_VALUE_TYPE) {
        return None;
    }
    let name = entry.get("name").and_then(Value::as_str)?;
    let value = entry.get("value").and_then(Value::as_str)?;
    Some(json!({ "name": name, "value": value }))
}

/// The `alsoKnownAs` aliases as strings: a single string or an array,
/// `[]` when absent or of another shape (Pleroma's
/// `normalize_also_known_as`); non-string entries are skipped.
pub(super) fn parse_also_known_as(json: &Value) -> Vec<String> {
    json.get("alsoKnownAs")
        .map_or_default(as_array)
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// `capabilities.acceptsChatMessages` (Pleroma's `litepub:capabilities`
/// object): `true` only when the nested flag literally says so.
pub(super) fn parse_accepts_chat_messages(json: &Value) -> Flag {
    bool_at(json, "capabilities", "acceptsChatMessages")
}

/// The boolean under `key` inside the object under `parent`, `false` when
/// either layer is absent or the value is not a boolean.
fn bool_at(json: &Value, parent: &str, key: &str) -> Flag {
    Flag::new(
        json.get(parent)
            .and_then(|object| object.get(key))
            .and_then(Value::as_bool)
            .unwrap_or_default(),
    )
}

/// The actor's `published` creation time as an RFC 3339 timestamp
/// ([`parse_time`] handles the `Z` and `+hh:mm` spellings); `None` when
/// absent or unparseable.
pub(super) fn parse_published(json: &Value) -> Option<DateTime<Utc>> {
    json.get("published")
        .and_then(Value::as_str)
        .and_then(parse_time)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::federation::remote_actor::{
        parse_actor,
        test_support::{MASTODON, fixture},
    };

    #[test]
    fn media_fields_take_a_string_an_object_or_an_array() {
        let mut document = fixture(MASTODON);
        document["icon"] = json!("https://cdn.example/a.png");
        assert_eq!(
            parse_actor(&document).unwrap().avatar_url.as_deref(),
            Some("https://cdn.example/a.png"),
            "a bare string icon"
        );
        document["icon"] = json!([Value::Null, {"url": "https://cdn.example/b.png"}]);
        assert_eq!(
            parse_actor(&document).unwrap().avatar_url.as_deref(),
            Some("https://cdn.example/b.png"),
            "an array icon whose first entry is filler"
        );
        document["image"] =
            json!({"type": "Image", "url": [{"href": "https://cdn.example/c.png"}]});
        assert_eq!(
            parse_actor(&document).unwrap().banner_url.as_deref(),
            Some("https://cdn.example/c.png"),
            "an object image with a url array of Links"
        );
    }

    #[test]
    fn malformed_optional_fields_are_dropped_not_rejected() {
        let mut document = fixture(MASTODON);
        document["outbox"] = json!(7);
        document["followers"] = json!("not a url");
        document["url"] = json!({"nothing": true});
        document["icon"] = json!({"url": []});
        document["endpoints"] = json!("sharedInbox");
        document["discoverable"] = json!("yes");
        document["alsoKnownAs"] = json!(42);
        document["movedTo"] = json!(["who"]);
        document["published"] = json!("yesterday");
        let parsed = parse_actor(&document).unwrap();
        assert_eq!(parsed.outbox, None, "a number outbox");
        assert_eq!(parsed.followers_url, None, "a relative followers");
        assert_eq!(parsed.profile_url, None, "a url with no reference");
        assert_eq!(parsed.avatar_url, None, "an icon with no usable url");
        assert_eq!(parsed.shared_inbox, None, "endpoints not an object");
        assert!(!parsed.discoverable.get(), "a string is not the flag");
        assert!(parsed.also_known_as.is_empty(), "a number alias list");
        assert_eq!(parsed.moved_to, None, "a non-url move target");
        assert_eq!(parsed.published, None, "an unparseable timestamp");
    }
}
