//! The PHP/Rails-style key parser behind form bodies and query strings:
//! `ids[]=1&ids[]=2` becomes an array, `a[b]=c` a nested object.

use std::mem;

use serde_json::{Map, Value};
use url::form_urlencoded;

/// Parses `application/x-www-form-urlencoded` text (also used for the query
/// string) into a JSON value, giving PHP/Rails-style keys their structure:
/// `a[b]=c` nests an object, `ids[]=1&ids[]=2` appends an array, repeated
/// plain keys become an array, and bracketed indexes stay object keys
/// (`fields_attributes[0][name]` becomes `{"fields_attributes":{"0":
/// {"name":…}}}`; [`index_map_to_vec`] turns such a map back into a list).
/// Values are the percent-decoded strings (`+` a space); containers win
/// over conflicting scalars, and the last write wins.
pub(super) fn parse_form(text: &str) -> Value {
    parse_pairs(form_urlencoded::parse(text.as_bytes()))
}

/// Structures already-decoded key/value pairs — the pairs of a urlencoded
/// body, a query string, or the text fields of a multipart body — into one
/// parameters value by the same rules as `parse_form`.
#[must_use]
pub fn parse_pairs<P, K, V>(pairs: P) -> Value
where
    P: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut root = Value::Object(Map::new());
    for (key, value) in pairs {
        let path = split_key(key.as_ref());
        // A key starting with `[` (or empty) addresses no name; drop it.
        if matches!(path.first(), Some(first) if first.is_empty()) {
            continue;
        }
        assign(&mut root, &path, Value::String(value.as_ref().to_owned()));
    }
    root
}

/// Turns a parsed key like `fields_attributes[0][name]` into its segments
/// (`fields_attributes`, `0`, `name`); `key[]` becomes (`key`, `""`), the
/// empty segment marking an array append.
fn split_key(key: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut rest = key;
    while let Some((head, tail)) = rest.split_once('[') {
        // The text before `[` is a segment; when it follows a previous `]`
        // it still carries that bracket.
        segments.push(head.strip_suffix(']').unwrap_or(head));
        rest = tail;
    }
    segments.push(rest.strip_suffix(']').unwrap_or(rest));
    segments
}

/// Stores `value` in the JSON tree at `node` under the bracketed path
/// `path`, creating the objects and arrays the path asks for. The empty
/// segment appends to `node` itself as an array; a named segment descends
/// into `node` as an object; the end of the path merges the value in (see
/// [`merge_leaf`]).
fn assign(node: &mut Value, path: &[&str], value: Value) {
    let Some((&segment, rest)) = path.split_first() else {
        merge_leaf(node, value);
        return;
    };
    if segment.is_empty() {
        ensure_array(node);
        let Some(array) = node.as_array_mut() else {
            return;
        };
        if rest.is_empty() {
            array.push(value);
        } else {
            array.push(Value::Object(Map::new()));
            if let Some(element) = array.last_mut() {
                assign(element, rest, value);
            }
        }
    } else {
        ensure_object(node);
        let Some(map) = node.as_object_mut() else {
            return;
        };
        let slot = map.entry(segment.to_owned()).or_insert(Value::Null);
        assign(slot, rest, value);
    }
}

/// Makes `node` an array: kept as is, created when empty, an existing lone
/// value wrapped, an object replaced (containers win).
fn ensure_array(node: &mut Value) {
    match node {
        Value::Array(_) => {}
        Value::Null | Value::Object(_) => *node = Value::Array(Vec::new()),
        Value::Bool(_) | Value::String(_) | Value::Number(_) => {
            let lone = mem::replace(node, Value::Null);
            *node = Value::Array(vec![lone]);
        }
    }
}

/// Makes `node` an object, replacing anything else (containers win).
fn ensure_object(node: &mut Value) {
    if !node.is_object() {
        *node = Value::Object(Map::new());
    }
}

/// Combines an already-stored value with a new one for the same key: an
/// array grows, an empty slot stays the value, a scalar becomes a
/// two-element array (the repeated plain keys `a=1&a=2`), and an object
/// slot is replaced (last write wins).
fn merge_leaf(existing: &mut Value, value: Value) {
    match existing {
        Value::Array(array) => array.push(value),
        Value::Null | Value::Object(_) => *existing = value,
        Value::Bool(_) | Value::String(_) | Value::Number(_) => {
            let previous = mem::replace(existing, Value::Null);
            *existing = Value::Array(vec![previous, value]);
        }
    }
}

/// Turns a PHP-style index map into the list it encodes.
///
/// The values of an object, ordered by their keys as numbers when every
/// key is numeric — like the `fields_attributes` of a form body; a JSON
/// array unchanged; any other value as a one-element list. Object keys
/// that are not all numeric keep the plain key order [`Value`] objects are
/// stored in.
#[must_use]
pub fn index_map_to_vec(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            let numeric = entries.iter().all(|(key, _)| key.parse::<usize>().is_ok());
            if numeric {
                entries.sort_by_key(|(key, _)| key.parse::<usize>().unwrap_or_default());
            }
            entries.into_iter().map(|(_, item)| item).collect()
        }
        scalar @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => {
            vec![scalar]
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn keys_split_at_brackets() {
        assert_eq!(split_key("name"), ["name"], "a plain key");
        assert_eq!(split_key("a[b]"), ["a", "b"], "one level");
        assert_eq!(split_key("ids[]"), ["ids", ""], "an append marker");
        assert_eq!(
            split_key("fields_attributes[0][name]"),
            ["fields_attributes", "0", "name"],
            "index then field"
        );
    }

    #[test]
    fn array_keys_become_json_arrays() {
        assert_eq!(parse_form("ids[]=1&ids[]=2"), json!({"ids": ["1", "2"]}));
        assert_eq!(
            parse_form("tags%5B%5D=a+b&tags[]=c"),
            json!({"tags": ["a b", "c"]}),
            "the encoded and plain marker are the same key"
        );
        assert_eq!(
            parse_form("a[][b]=1&a[][b]=2"),
            json!({"a": [{"b": "1"}, {"b": "2"}]}),
            "an append in the middle opens a new element"
        );
        assert_eq!(
            parse_form("a[b][]=x&a[b][]=y"),
            json!({"a": {"b": ["x", "y"]}}),
            "appends nest inside objects"
        );
    }

    #[test]
    fn nested_keys_become_json_objects() {
        assert_eq!(parse_form("a[b]=c"), json!({"a": {"b": "c"}}));
        assert_eq!(
            parse_form("a[b][c]=d&a[e]=f"),
            json!({"a": {"b": {"c": "d"}, "e": "f"}}),
            "levels coexist"
        );
        assert_eq!(
            parse_form(
                "fields_attributes[0][name]=x&fields_attributes[0][value]=y&fields_attributes[1][name]=z"
            ),
            json!({"fields_attributes": {
                "0": {"name": "x", "value": "y"},
                "1": {"name": "z"},
            }}),
            "numeric keys stay object keys, as required"
        );
    }

    #[test]
    fn repeated_plain_keys_become_arrays_and_decoding_applies() {
        assert_eq!(parse_form("a=1&a=2"), json!({"a": ["1", "2"]}));
        assert_eq!(parse_form("a=1&a=2&a=3"), json!({"a": ["1", "2", "3"]}));
        assert_eq!(
            parse_form("q=hello+world&a[b%28x%29]=1"),
            json!({"q": "hello world", "a": {"b(x)": "1"}}),
            "+ and %XX"
        );
        assert_eq!(
            parse_form("a=1&a[]=2"),
            json!({"a": ["1", "2"]}),
            "a lone value joins the array"
        );
    }

    #[test]
    fn containers_win_and_the_last_write_wins() {
        assert_eq!(
            parse_form("a=1&a[b]=c"),
            json!({"a": {"b": "c"}}),
            "object over scalar"
        );
        assert_eq!(
            parse_form("a[b]=c&a=1"),
            json!({"a": "1"}),
            "scalar over object"
        );
        assert_eq!(parse_form(""), json!({}), "empty text is no parameters");
        assert_eq!(
            parse_form("=1&[a]=2"),
            json!({}),
            "keys naming nothing are dropped"
        );
    }

    #[test]
    fn index_maps_become_lists() {
        let map = parse_form("f[10][v]=ten&f[2][v]=two&f[1][v]=one");
        let fields = map.get("f").expect("parsed map").clone();
        assert_eq!(
            index_map_to_vec(fields),
            vec![
                json!({"v": "one"}),
                json!({"v": "two"}),
                json!({"v": "ten"})
            ],
            "numeric keys order as numbers"
        );
        assert_eq!(
            index_map_to_vec(json!({"b": 2, "a": 1})),
            vec![json!(1), json!(2)],
            "non-numeric keys keep the object's key order"
        );
        assert_eq!(
            index_map_to_vec(json!([1, 2])),
            vec![json!(1), json!(2)],
            "arrays pass through"
        );
        assert_eq!(
            index_map_to_vec(json!("x")),
            vec![json!("x")],
            "a lone value becomes one item"
        );
        assert_eq!(
            index_map_to_vec(json!({})),
            Vec::<Value>::new(),
            "an empty map is empty"
        );
    }
}
