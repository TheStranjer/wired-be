//! The `:frontend_configurations` entry: one arbitrary settings object per
//! frontend, keyed by frontend name.
//!
//! The reference is Pleroma's `ConfigDB.merge`: each subkey's value is a map,
//! so a write replaces that subkey's whole object and leaves every other
//! stored subkey alone. The settings objects are opaque to the server, so
//! unlike the other keys they are stored and rendered as plain JSON — no
//! tuple decoding below the subkey level.

use serde_json::{Map, Value, json};

use super::wire::{normalize_key, tuple_pair};
use crate::services::config_registry::KEY_FRONTEND_CONFIGURATIONS;

/// The refusal for a value that is not a list of `[:frontend, {…}]` tuples.
const SHAPE_ERROR: &str = ":frontend_configurations takes a non-empty list of \
    {\"tuple\": [\":frontend\", {...}]} entries, one settings object per frontend";

/// Decodes the wire-form `value` of an upsert into subkey → settings object.
///
/// Subkey names lose a leading `:`; a name given twice keeps its last
/// object, as a keyword list does.
///
/// # Errors
///
/// The message the admin API answers 400 with, when `value` is not a
/// non-empty array of `{"tuple": [name, object]}` entries.
pub fn decode_frontend_configurations(value: &Value) -> Result<Map<String, Value>, String> {
    let Value::Array(items) = value else {
        return Err(SHAPE_ERROR.to_owned());
    };
    if items.is_empty() {
        return Err(SHAPE_ERROR.to_owned());
    }
    let mut decoded = Map::new();
    for item in items {
        let Value::Object(entry) = item else {
            return Err(SHAPE_ERROR.to_owned());
        };
        let (name, settings) = tuple_pair(entry).ok_or_else(|| SHAPE_ERROR.to_owned())?;
        let name = normalize_key(name);
        if name.is_empty() {
            return Err(SHAPE_ERROR.to_owned());
        }
        if !settings.is_object() {
            return Err(format!(
                "{KEY_FRONTEND_CONFIGURATIONS} {name} must be a settings object"
            ));
        }
        let previous = decoded.insert(name, settings.clone());
        drop(previous);
    }
    Ok(decoded)
}

/// `incoming` over the stored row: every named subkey is replaced whole,
/// every other stored subkey stays.
#[must_use]
pub(super) fn replace_subkeys(old: Option<&Value>, incoming: Map<String, Value>) -> Value {
    let mut merged = old.and_then(Value::as_object).cloned().unwrap_or_default();
    merged.extend(incoming);
    Value::Object(merged)
}

/// The wire form of a stored row of `key`: tuples whose values stay the
/// plain JSON settings objects for `:frontend_configurations`, the generic
/// recursive [`to_wire`](super::to_wire) for every other key.
#[must_use]
pub fn wire_of(key: &str, value: &Value) -> Value {
    if key != KEY_FRONTEND_CONFIGURATIONS {
        return super::wire::to_wire(value);
    }
    let Value::Object(map) = value else {
        return value.clone();
    };
    Value::Array(
        map.iter()
            .map(|(name, settings)| json!({"tuple": [name, settings]}))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed list decodes, colons stripped, objects verbatim.
    #[test]
    fn decoding_keeps_the_settings_objects_verbatim() {
        let decoded = decode_frontend_configurations(&json!([
            {"tuple": [":soapbox_fe", {"brandColor": "#123456", ":odd": [{"tuple": ["a", 1]}]}]},
            {"tuple": ["pleroma_fe", {}]},
        ]))
        .expect("a keyword list of objects decodes");
        assert_eq!(
            Value::Object(decoded),
            json!({
                "soapbox_fe": {"brandColor": "#123456", ":odd": [{"tuple": ["a", 1]}]},
                "pleroma_fe": {},
            }),
            "names lose the colon, the objects are untouched"
        );
    }

    /// Every other shape is refused.
    #[test]
    fn decoding_refuses_other_shapes() {
        for bad in [
            json!({"soapbox_fe": {"a": 1}}),
            json!("soapbox_fe"),
            json!([]),
            json!(["soapbox_fe"]),
            json!([{"tuple": [":soapbox_fe"]}]),
            json!([{"tuple": [":soapbox_fe", {}, 1]}]),
            json!([{"tuple": [7, {}]}]),
            json!([{"tuple": [":", {}]}]),
            json!([{"tuple": [":soapbox_fe", "dark"]}]),
            json!([{"tuple": [":soapbox_fe", [1]]}]),
            json!([{"tuple": [":soapbox_fe", null]}]),
            json!([{"tuple": [":soapbox_fe", {}]}, {"other": 1}]),
        ] {
            assert!(
                decode_frontend_configurations(&bad).is_err(),
                "{bad} is not a frontend keyword list"
            );
        }
    }

    /// A later duplicate wins.
    #[test]
    fn a_repeated_subkey_keeps_the_last_object() {
        let decoded = decode_frontend_configurations(&json!([
            {"tuple": [":soapbox_fe", {"a": 1}]},
            {"tuple": [":soapbox_fe", {"b": 2}]},
        ]))
        .expect("decodes");
        assert_eq!(
            Value::Object(decoded),
            json!({"soapbox_fe": {"b": 2}}),
            "the second object replaces the first"
        );
    }

    /// Replacing swaps a subkey's whole object and keeps the siblings.
    #[test]
    fn replacing_swaps_whole_subkeys() {
        let old = json!({"soapbox_fe": {"a": 1, "nested": {"x": 1}}, "pleroma_fe": {"k": 1}});
        let incoming = decode_frontend_configurations(&json!([
            {"tuple": [":soapbox_fe", {"nested": {"y": 2}}]},
        ]))
        .expect("decodes");
        assert_eq!(
            replace_subkeys(Some(&old), incoming.clone()),
            json!({"soapbox_fe": {"nested": {"y": 2}}, "pleroma_fe": {"k": 1}}),
            "no key of the old soapbox_fe object survives"
        );
        assert_eq!(
            replace_subkeys(None, incoming),
            json!({"soapbox_fe": {"nested": {"y": 2}}}),
            "with nothing stored the incoming subkeys are the row"
        );
    }

    /// The key renders tuples around plain objects; other keys keep the
    /// generic recursive form.
    #[test]
    fn wire_of_keeps_settings_objects_plain() {
        assert_eq!(
            wire_of(
                KEY_FRONTEND_CONFIGURATIONS,
                &json!({"soapbox_fe": {"brandColor": "#123456"}})
            ),
            json!([{"tuple": ["soapbox_fe", {"brandColor": "#123456"}]}]),
            "tuple outside, plain object inside"
        );
        assert_eq!(
            wire_of(":instance", &json!({"name": "Wired"})),
            json!([{"tuple": ["name", "Wired"]}]),
            "other keys render as before"
        );
    }
}
