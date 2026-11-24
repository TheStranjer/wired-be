//! Small collection helpers shared by the batched read paths.

use std::{collections::HashMap, hash::Hash};

/// Groups `(key, value)` pairs by key, keeping the order the pairs arrive in
/// within each key.
///
/// The fold every batched per-status read ends with: rows come back from one
/// query (already ordered as the caller wants them shown) and the map hands
/// each key its values in that same order.
#[must_use]
pub fn group_pairs<K, V, I>(pairs: I) -> HashMap<K, Vec<V>>
where
    K: Eq + Hash,
    I: IntoIterator<Item = (K, V)>,
{
    let mut grouped: HashMap<K, Vec<V>> = HashMap::new();
    for (key, value) in pairs {
        grouped.entry(key).or_default().push(value);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_insertion_order_within_a_key() {
        let grouped = group_pairs([(1, "c"), (2, "x"), (1, "a"), (1, "b"), (2, "y")]);
        assert_eq!(grouped[&1], vec!["c", "a", "b"]);
        assert_eq!(grouped[&2], vec!["x", "y"]);
    }

    #[test]
    fn deduplicates_keys() {
        let grouped = group_pairs([("a", 1), ("b", 2), ("a", 3)]);
        assert_eq!(grouped.len(), 2);
    }

    #[test]
    fn empty_input_gives_an_empty_map() {
        let grouped = group_pairs(Vec::<(u8, u8)>::new());
        assert!(grouped.is_empty());
    }
}
