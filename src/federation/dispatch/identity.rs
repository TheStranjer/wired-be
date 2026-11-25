//! The identity an incoming activity is deduplicated under: its id, and for
//! an `Update` also a digest of the embedded object.

use serde_json::Value;
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::util::token::to_hex;

/// The key a delivery is deduplicated under, `None` for a document with no
/// string `id`.
///
/// The `id` is canonicalized first (lowercased, trailing host dot and empty
/// query/fragment removed), so spellings of one id share a key.
///
/// Plain activities dedupe on their `id`. An `Update` that embeds its
/// object dedupes on `id` plus a SHA-256 of the object's canonical JSON:
/// Mastodon builds Update ids from a whole-second timestamp, so two edits
/// in one second share an id but not an object, and the second must not be
/// swallowed as a redelivery.
#[must_use]
pub fn dedupe_key(activity: &Value) -> Option<String> {
    let id = canonical_id(activity.get("id").and_then(Value::as_str)?);
    let is_update = activity.get("type").is_some_and(|kind| match kind {
        Value::String(name) => name == "Update",
        Value::Array(names) => names.iter().any(|name| name.as_str() == Some("Update")),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => false,
    });
    let embedded = activity.get("object").filter(|object| object.is_object());
    Some(match embedded {
        Some(object) if is_update => {
            let mut canonical = String::new();
            write_canonical(object, &mut canonical);
            format!(
                "{id}#sha256:{}",
                to_hex(&Sha256::digest(canonical.as_bytes()))
            )
        }
        _ => id,
    })
}

/// The canonical spelling of an activity id: lowercase, no trailing dot on
/// the host, no empty query or fragment.
///
/// A hostile peer can re-deliver one activity under many spellings of its id;
/// the ledger must see them as one. An id that is not a url is only
/// lowercased.
fn canonical_id(id: &str) -> String {
    let Ok(mut parsed) = Url::parse(id) else {
        return id.to_lowercase();
    };
    if let Some(host) = parsed.host_str() {
        let trimmed = host.trim_end_matches('.');
        if !trimmed.is_empty() && trimmed.len() != host.len() {
            let trimmed = trimmed.to_owned();
            if parsed.set_host(Some(&trimmed)).is_err() {
                return id.to_lowercase();
            }
        }
    }
    if parsed.query() == Some("") {
        parsed.set_query(None);
    }
    if parsed.fragment() == Some("") {
        parsed.set_fragment(None);
    }
    parsed.as_str().to_lowercase()
}

/// Appends `value` as JSON with every object's keys in sorted order, so two
/// spellings of one document digest alike.
fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            out.push('{');
            for (index, (key, member)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(member, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            out.push_str(&value.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// An `Update` of a note with `content`, under the shared id.
    fn update(content: &str) -> Value {
        json!({
            "id": "https://peer.example/s/1#updates/1795571820",
            "type": "Update",
            "actor": "https://peer.example/users/alice",
            "object": {"id": "https://peer.example/s/1", "content": content},
        })
    }

    /// Same id, different object: different keys; same object: same key.
    #[test]
    fn an_update_is_keyed_by_id_and_object_digest() {
        let first = dedupe_key(&update("one"));
        let second = dedupe_key(&update("two"));

        assert_ne!(first, second, "different objects, different keys");
        assert_eq!(first, dedupe_key(&update("one")), "a redelivery matches");
        assert!(
            first.as_deref().is_some_and(
                |key| key.starts_with("https://peer.example/s/1#updates/1795571820#sha256:")
            ),
            "the key starts with the activity id: {first:?}"
        );
    }

    /// Member order does not change the digest.
    #[test]
    fn key_order_does_not_change_the_digest() {
        let mut reordered = update("one");
        reordered["object"] = json!({"content": "one", "id": "https://peer.example/s/1"});

        assert_eq!(dedupe_key(&update("one")), dedupe_key(&reordered));
    }

    /// Every other shape dedupes on the bare id.
    #[test]
    fn other_activities_and_bare_updates_use_the_plain_id() {
        let follow =
            json!({"id": "https://peer.example/a/1", "type": "Follow", "object": {"x": 1}});
        let bare = json!({"id": "https://peer.example/a/2", "type": "Update", "object": "https://peer.example/s/1"});

        assert_eq!(
            dedupe_key(&follow).as_deref(),
            Some("https://peer.example/a/1")
        );
        assert_eq!(
            dedupe_key(&bare).as_deref(),
            Some("https://peer.example/a/2")
        );
        assert_eq!(
            dedupe_key(&json!({"type": "Update"})),
            None,
            "no id, no key"
        );
    }

    /// Case, trailing-dot and empty query/fragment variants share a key.
    #[test]
    fn spellings_of_one_id_share_a_key() {
        let key = |id: &str| dedupe_key(&json!({"id": id, "type": "Follow"}));
        let base = key("https://peer.example/activities/replay-1");

        for variant in [
            "https://peer.example/activities/REPLAY-1",
            "HTTPS://Peer.Example/activities/replay-1",
            "https://peer.example./activities/replay-1",
            "https://peer.example/activities/replay-1?",
            "https://peer.example/activities/replay-1#",
            "https://peer.example:443/activities/replay-1",
        ] {
            assert_eq!(key(variant), base, "{variant} is the same activity");
        }
        assert_ne!(
            key("https://peer.example/activities/replay-1?x=1"),
            base,
            "a real query is a different id"
        );
        assert_ne!(
            key("https://peer.example/activities/replay-1#a"),
            base,
            "a real fragment is a different id"
        );
        assert_eq!(key("not a url/ID").as_deref(), Some("not a url/id"));
    }

    /// An array `type` containing `Update` counts as an `Update`.
    #[test]
    fn an_array_type_naming_update_is_an_update() {
        let mut doc = update("one");
        doc["type"] = json!(["Update", "Extra"]);

        assert_ne!(dedupe_key(&doc).as_deref(), doc["id"].as_str());
    }
}
