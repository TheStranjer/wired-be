//! Translation between the admin API's wire form and the stored form,
//! and the merge and default rules over both.

use serde_json::{Map, Value};

/// Translates the admin API's wire form into the storage form: an array
/// of `{"tuple": [key, value]}` entries becomes the JSON object they
/// spell, and object keys lose a leading `:`.
///
/// A lone `{"tuple": …}` pair object becomes its one-entry object, and so
/// both this API's plain `"name"` and Pleroma's `":name"` subkey
/// spellings land on the same stored key. Other values pass through
/// shape-wise, translated recursively.
#[must_use]
pub fn to_storage(value: &Value) -> Value {
    match value {
        Value::Array(items) => tuples_to_object(items).map_or_else(
            || Value::Array(items.iter().map(to_storage).collect()),
            Value::Object,
        ),
        Value::Object(map) => match tuple_pair(map) {
            Some((key, inner)) => {
                let mut object = Map::new();
                let displaced = object.insert(normalize_key(key), to_storage(inner));
                drop(displaced);
                Value::Object(object)
            }
            None => Value::Object(
                map.iter()
                    .map(|(key, inner)| (normalize_key(key), to_storage(inner)))
                    .collect(),
            ),
        },
        scalar @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => {
            scalar.clone()
        }
    }
}

/// Translates the storage form into the admin API's wire form: every JSON
/// object (recursively) becomes its array of `{"tuple": [key, value]}`
/// entries — Pleroma's keyword-list encoding.
///
/// Arrays of plain values and scalars stay JSON as they are.
#[must_use]
pub fn to_wire(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Array(
            map.iter()
                .map(|(key, inner)| tuple_entry(Value::String(key.clone()), to_wire(inner)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(to_wire).collect()),
        scalar @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => {
            scalar.clone()
        }
    }
}

/// The `{"tuple": [key, value]}` entry of the wire form.
fn tuple_entry(key: Value, value: Value) -> Value {
    let mut entry = Map::new();
    let previous = entry.insert("tuple".to_owned(), Value::Array(vec![key, value]));
    drop(previous);
    Value::Object(entry)
}

/// The object an all-tuple array spells, `None` when any element is no
/// `{"tuple": [key, value]}` pair. The empty array is no keyword list
/// here either — a data list stays a list — and [`RuntimeConfig::set`]
/// is where the entry value's empty keyword form (`{}` on the storage
/// side, `[]` on the wire) gets reconciled.
fn tuples_to_object(items: &[Value]) -> Option<Map<String, Value>> {
    if items.is_empty() {
        return None;
    }
    let mut object = Map::new();
    for item in items {
        let Value::Object(map) = item else {
            return None;
        };
        let (key, value) = tuple_pair(map)?;
        let previous = object.insert(normalize_key(key), to_storage(value));
        drop(previous);
    }
    Some(object)
}

/// The `(key, value)` a `{"tuple": [key, value]}` object spells, `None`
/// for any other object. The key must be a string; a number-keyed or
/// nested-tuple key is data, not a pair.
pub(super) fn tuple_pair(map: &Map<String, Value>) -> Option<(&String, &Value)> {
    if map.len() != 1 {
        return None;
    }
    let Value::Array(items) = map.get("tuple")? else {
        return None;
    };
    let [Value::String(key), value] = items.as_slice() else {
        return None;
    };
    Some((key, value))
}

/// The storage key a wire key names: the name without its leading `:`.
pub(super) fn normalize_key(key: &str) -> String {
    key.strip_prefix(':').unwrap_or(key).to_owned()
}

/// `new` over `old`, the way Pleroma's `ConfigDB.merge` folds keyword
/// lists: two objects merge key by key, recursively.
///
/// Any other pair of values is the replacement Pleroma's last-clause-wins
/// `deep_merge` answers.
#[must_use]
pub fn merge_value(old: &Value, new: &Value) -> Value {
    let (Value::Object(existing), Value::Object(incoming)) = (old, new) else {
        return new.clone();
    };
    let mut merged = existing.clone();
    for (key, value) in incoming {
        let next = merged
            .get(key)
            .map_or_else(|| value.clone(), |previous| merge_value(previous, value));
        let displaced = merged.insert(key.clone(), next);
        drop(displaced);
    }
    Value::Object(merged)
}

/// The entry's effective value: the stored row over the registry
/// default, Pleroma's `ConfigDB.merge` of the boot config and the DB.
#[must_use]
pub fn effective_value(default: &Value, stored: Option<&Value>) -> Value {
    stored.map_or_else(|| default.clone(), |value| merge_value(default, value))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The wire form spells stored objects as tuple arrays — and plain
    /// JSON data (arrays of strings, scalars, pair lists) as itself.
    #[test]
    fn to_wire_encodes_keyword_objects_as_tuples() {
        assert_eq!(
            to_wire(&json!({"name": "Wired", "registrations_open": false})),
            json!([
                {"tuple": ["name", "Wired"]},
                {"tuple": ["registrations_open", false]},
            ]),
            "one entry per subkey, in stored order"
        );
        assert_eq!(
            to_wire(&json!({})),
            json!([]),
            "the empty keyword list is the empty array"
        );
        assert_eq!(
            to_wire(&json!({"reject": [["evil.example", "bots"]]})),
            json!([
                {"tuple": ["reject", [["evil.example", "bots"]]]}
            ]),
            "a pair LIST stays a list — only objects become tuples"
        );
        assert_eq!(to_wire(&json!("text")), json!("text"), "scalars pass");
    }

    /// [`to_storage`] decodes tuple arrays back — accepting both this
    /// API's plain subkeys and Pleroma's colon-prefixed spellings — and
    /// is the inverse of [`to_wire`].
    #[test]
    fn to_storage_decodes_tuples_and_normalizes_keys() {
        assert_eq!(
            to_storage(&json!([
                {"tuple": ["name", "Wired"]},
                {"tuple": ["limit", 5000]},
            ])),
            json!({"name": "Wired", "limit": 5000}),
            "the tuple array becomes its object"
        );
        assert_eq!(
            to_storage(&json!([{"tuple": [":name", "Wired"]}])),
            json!({"name": "Wired"}),
            "a colon-prefixed subkey stores without it"
        );
        assert_eq!(
            to_storage(&json!([{"tuple": ["timelines", [{"tuple": ["local", true]}]]}])),
            json!({"timelines": {"local": true}}),
            "nesting decodes recursively"
        );
        assert_eq!(
            to_storage(&json!(["plain", "strings"])),
            json!(["plain", "strings"]),
            "a data list is not keyword material"
        );
        assert_eq!(
            to_storage(&json!([])),
            json!([]),
            "the empty array stays the empty list it is"
        );
        let wire = json!([{"tuple": ["reject", [["a.example", "why"]]]}]);
        assert_eq!(
            to_wire(&to_storage(&wire)),
            wire,
            "wire → storage → wire round-trips"
        );
    }

    /// [`merge_value`] overlays objects key by key, recursively, and
    /// replaces anything that is not a keyword object — Pleroma's
    /// `deep_merge` outcome.
    #[test]
    fn merging_overlays_subkeys_and_replaces_scalars() {
        assert_eq!(
            merge_value(
                &json!({"name": "old", "email": "a@b.c", "nested": {"x": 1, "y": 2}}),
                &json!({"name": "new", "nested": {"y": 3}})
            ),
            json!({"name": "new", "email": "a@b.c", "nested": {"x": 1, "y": 3}}),
            "shared subkeys win, siblings stay, nesting merges deeply"
        );
        assert_eq!(
            merge_value(&json!({"a": 1}), &json!(["list"])),
            json!(["list"]),
            "a plain value replaces the keyword object"
        );
        assert_eq!(
            merge_value(&json!(7), &json!(9)),
            json!(9),
            "and replaces anything else"
        );
    }
}
