//! Id and key extraction from rendered JSON entities.

use serde_json::Value;

/// The string `id` of every entry of the JSON array `array`, in order.
///
/// # Panics
///
/// If `array` is not a JSON array, or an entry has no string `id`.
#[must_use]
pub fn ids_of(array: &Value) -> Vec<String> {
    let Some(entries) = array.as_array() else {
        panic!("expected a JSON array of entities, got {array}");
    };
    entries.iter().map(id_of).collect()
}

/// The string `id` of the JSON entity `entity`.
///
/// # Panics
///
/// If `entity` has no string `id`.
#[must_use]
pub fn id_of(entity: &Value) -> String {
    let Some(id) = entity["id"].as_str() else {
        panic!("expected an entity with a string `id`, got {entity}");
    };
    id.to_owned()
}

/// The keys of the JSON object `value`, sorted, for exact key-set
/// assertions.
///
/// # Panics
///
/// If `value` is not a JSON object.
#[must_use]
pub fn sorted_keys(value: &Value) -> Vec<&str> {
    let Some(object) = value.as_object() else {
        panic!("expected a JSON object, got {value}");
    };
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// An object's keys come back sorted.
    #[test]
    fn sorted_keys_sorts_the_object_keys() {
        assert_eq!(
            sorted_keys(&json!({"b": 1, "c": 2, "a": 3})),
            vec!["a", "b", "c"]
        );
        assert_eq!(sorted_keys(&json!({})), Vec::<&str>::new());
    }

    /// A non-object panics.
    #[test]
    #[should_panic(expected = "JSON object")]
    fn sorted_keys_panics_on_a_non_object() {
        drop(sorted_keys(&json!([1])));
    }

    /// An entity's `id` text is returned.
    #[test]
    fn id_of_reads_the_string_id() {
        assert_eq!(id_of(&json!({"id": "7", "other": "x"})), "7");
    }

    /// An entity without a string `id` panics.
    #[test]
    #[should_panic(expected = "string `id`")]
    fn id_of_panics_without_a_string_id() {
        drop(id_of(&json!({"id": 7})));
    }

    /// Ids come back in array order.
    #[test]
    fn ids_of_keeps_array_order() {
        let listing = json!([{"id": "b"}, {"id": "a"}]);
        assert_eq!(ids_of(&listing), vec!["b", "a"]);
        assert_eq!(ids_of(&json!([])).len(), 0);
    }

    /// A non-array body panics.
    #[test]
    #[should_panic(expected = "JSON array")]
    fn ids_of_panics_on_a_non_array() {
        drop(ids_of(&json!({"id": "1"})));
    }

    /// An entry without an id panics.
    #[test]
    #[should_panic(expected = "string `id`")]
    fn ids_of_panics_on_an_entry_without_an_id() {
        drop(ids_of(&json!([{"id": "1"}, {"name": "x"}])));
    }
}
