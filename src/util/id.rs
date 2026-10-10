//! Identifiers: `UUIDv7` generation and the parsing of client-supplied ids.
//!
//! Per `docs/ARCHITECTURE.md` decision 4 every primary key is a `UUIDv7` made in
//! Rust and serialized to JSON as lowercase hyphenated text. That text sorts in
//! creation order, which is what makes `max_id`/`min_id`/`since_id` pagination a
//! plain comparison on the id column.

use uuid::Uuid;

/// Length of UUID text in hyphenated form: `8-4-4-4-12`.
const HYPHENATED_LEN: usize = 36;
/// Length of UUID text in simple form: 32 hex digits without separators.
const SIMPLE_LEN: usize = 32;

/// A fresh `UUIDv7`, ready to be used as a primary key.
#[must_use]
pub fn new_id() -> Uuid {
    Uuid::now_v7()
}

/// Reads a [`Uuid`] back from its hyphenated or simple text, `None` for
/// anything else.
///
/// Only the two plain text forms clients actually receive are accepted: the
/// braced (`{...}`) and `urn:uuid:` spellings are rejected, as is any other
/// input. Hex digits are case-insensitive, and the version is not checked, so
/// ids that arrived from a remote instance parse too. Handlers answer `None`
/// with a 404 instead of trusting an id from a path or query parameter.
#[must_use]
pub fn parse_id(text: &str) -> Option<Uuid> {
    // The length gate is what rules out the braced and URN forms; `parse_str`
    // then checks that every character is a hex digit in the right place.
    if !matches!(text.len(), SIMPLE_LEN | HYPHENATED_LEN) {
        return None;
    }
    Uuid::parse_str(text).ok()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, thread, time::Duration};

    use super::*;
    use uuid::Version;

    /// How many ids the batch tests generate.
    const BATCH: usize = 100;

    #[test]
    fn new_ids_are_version_seven() {
        for _ in 0..BATCH {
            let id = new_id();
            assert_eq!(
                id.get_version(),
                Some(Version::SortRand),
                "ids are UUIDv7 (uuid::Version::SortRand)"
            );
        }
    }

    #[test]
    fn new_ids_are_unique() {
        let ids: HashSet<Uuid> = (0..BATCH).map(|_| new_id()).collect();
        assert_eq!(ids.len(), BATCH, "{BATCH} ids made in a row differ");
    }

    #[test]
    fn ids_sort_in_creation_order_as_text() {
        let mut ids = Vec::with_capacity(BATCH);
        for _ in 0..BATCH {
            ids.push(new_id().to_string());
            // One millisecond apart, so the timestamp prefix decides.
            thread::sleep(Duration::from_millis(1));
        }
        for (before, after) in ids.iter().zip(ids.iter().skip(1)) {
            assert!(
                before < after,
                "text sorts in creation order, so {before} cannot come before {after}"
            );
        }
    }

    #[test]
    fn parses_hyphenated_and_simple_text() {
        let id = new_id();
        assert_eq!(
            parse_id(&id.hyphenated().to_string()),
            Some(id),
            "hyphenated text parses"
        );
        assert_eq!(
            parse_id(&id.simple().to_string()),
            Some(id),
            "simple text parses"
        );
        assert_eq!(
            parse_id(&id.hyphenated().to_string().to_uppercase()),
            Some(id),
            "hex digits are case-insensitive"
        );
    }

    #[test]
    fn rejects_other_uuid_spellings() {
        let id = new_id();
        assert_eq!(
            parse_id(&id.braced().to_string()),
            None,
            "the braced form is not client text"
        );
        assert_eq!(
            parse_id(&id.urn().to_string()),
            None,
            "the urn form is not client text"
        );
    }

    #[test]
    fn rejects_anything_that_is_not_an_id() {
        for text in [
            "",
            " ",
            "not-an-id",
            "1",
            // One digit short, one too many, one with the hyphens misplaced and
            // one with a non-hex digit: all 31 to 37 characters of near-miss.
            "6790f6b8-9c3e-5a3d-9b1f-6790f6b89c3",
            "6790f6b8-9c3e-5a3d-9b1f-6790f6b89c3ee",
            "6790f6b89-c3e-5a3d-9b1f-6790f6b89c3e",
            "6790f6b8-9c3e-5a3d-9b1f-6790f6b89c3g",
            "2024-01-31T12:34:56.789Z",
        ] {
            assert_eq!(parse_id(text), None, "{text} is not an id");
        }
    }
}
