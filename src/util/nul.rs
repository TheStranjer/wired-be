//! Removal of U+0000, which `PostgreSQL` cannot store in `text` or `jsonb`.
//!
//! A NUL that reaches a query makes the database answer `unsupported
//! Unicode escape sequence` or `invalid byte sequence`, which surfaces as a
//! 500. Every text a client or a peer hands us is cleaned at the door with
//! these helpers instead: [`strip_nul`] for one string, [`strip_nul_json`]
//! for a whole parsed document, keys included.

use std::borrow::Cow;

use serde_json::Value;

/// The character the database refuses.
const NUL: char = '\0';

/// `text` without its U+0000 characters; borrowed when it has none.
#[must_use]
pub fn strip_nul(text: &str) -> Cow<'_, str> {
    if text.contains(NUL) {
        Cow::Owned(text.replace(NUL, ""))
    } else {
        Cow::Borrowed(text)
    }
}

/// Removes U+0000 from every string and object key in `value`, recursively.
///
/// Numbers, booleans and `null` are untouched. Two keys that become equal
/// after the strip keep the later one's value.
pub fn strip_nul_json(value: &mut Value) {
    match value {
        Value::String(text) => {
            text.retain(|ch| ch != NUL);
        }
        Value::Array(items) => {
            for item in items {
                strip_nul_json(item);
            }
        }
        Value::Object(map) => {
            *map = std::mem::take(map)
                .into_iter()
                .map(|(mut key, mut item)| {
                    key.retain(|ch| ch != NUL);
                    strip_nul_json(&mut item);
                    (key, item)
                })
                .collect();
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn strip_nul_removes_every_nul_and_borrows_clean_text() {
        assert_eq!(strip_nul("a\0b\0"), "ab");
        assert!(matches!(strip_nul("clean"), Cow::Borrowed("clean")));
        assert_eq!(strip_nul("\0"), "");
    }

    #[test]
    fn strip_nul_json_cleans_strings_keys_and_nesting() {
        let mut value = json!({
            "co\u{0}ntent": "nul \u{0} here",
            "list": ["a\u{0}", 1, null, true, {"x\u{0}": ["\u{0}y"]}],
            "n": 2.5,
        });
        strip_nul_json(&mut value);
        assert_eq!(
            value,
            json!({
                "content": "nul  here",
                "list": ["a", 1, null, true, {"x": ["y"]}],
                "n": 2.5,
            })
        );
    }
}
