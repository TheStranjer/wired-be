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

/// The activity id for `id` on the instance at `base_url` (decision 6).
///
/// The shape is `{base_url}/activities/{id}`. Every place that writes or reads
/// a local activity id builds it here, so the shape lives in one spot.
#[must_use]
pub fn activity_ap_id(base_url: &str, id: Uuid) -> String {
    format!("{base_url}/activities/{id}")
}

/// Path segment between the base url and the id of a local object id.
pub const OBJECTS_PATH: &str = "/objects/";

/// The object id for `id` on the instance at `base_url` (decision 6).
///
/// The shape is `{base_url}/objects/{id}`. Every place that mints a local
/// object id builds it here, and [`local_object_uuid`] reads it back.
#[must_use]
pub fn object_ap_id(base_url: &str, id: Uuid) -> String {
    format!("{base_url}{OBJECTS_PATH}{id}")
}

/// The context collection id this instance mints for a conversation.
///
/// The shape is `{base_url}/contexts/{conversation_id}`: what a local status
/// stamps as its `context` when its thread carries no context of its own,
/// and the address `GET /contexts/{id}` serves.
#[must_use]
pub fn context_ap_id(base_url: &str, conversation_id: Uuid) -> String {
    format!("{base_url}/contexts/{conversation_id}")
}

/// The id inside `ap_id` when it is an object id of the instance at
/// `base_url`, `None` for a remote host, another path or a tail that
/// [`parse_id`] does not read.
#[must_use]
pub fn local_object_uuid(base_url: &str, ap_id: &str) -> Option<Uuid> {
    let tail = ap_id.strip_prefix(base_url)?.strip_prefix(OBJECTS_PATH)?;
    parse_id(tail)
}

/// A freshly minted local activity id: [`activity_ap_id`] over a fresh
/// [`new_id`].
#[must_use]
pub fn new_activity_ap_id(base_url: &str) -> String {
    activity_ap_id(base_url, new_id())
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

/// The minted local activity id a test row carries, reduced to its id part.
///
/// It must be present, sit under `{TEST_BASE_URL}/activities/` and end in an
/// id [`parse_id`] reads.
#[cfg(test)]
pub(crate) fn assert_local_activity_id(ap_id: Option<&str>) -> String {
    let prefix = format!("{}/activities/", crate::config::TEST_BASE_URL);
    let ap_id = ap_id.expect("a local actor's row carries a minted activity id");
    let tail = ap_id
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("the activity id is this instance's, got {ap_id}"));
    assert!(
        parse_id(tail).is_some(),
        "the tail of the activity id is an id: {tail}"
    );
    tail.to_owned()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, thread, time::Duration};

    use super::*;
    use uuid::Version;

    /// How many ids the batch tests generate.
    const BATCH: usize = 100;

    #[test]
    fn context_ap_id_joins_base_contexts_path_and_conversation() {
        let id = new_id();
        assert_eq!(
            context_ap_id("https://h", id),
            format!("https://h/contexts/{id}"),
            "the conversation sits under the instance's contexts path"
        );
    }

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
    fn activity_ap_id_joins_base_prefix_and_id() {
        let id = new_id();
        assert_eq!(
            activity_ap_id("https://h", id),
            format!("https://h/activities/{id}"),
            "the id sits under the instance's activities path"
        );
    }

    #[test]
    fn object_ap_id_joins_base_prefix_and_id() {
        let id = new_id();
        assert_eq!(
            object_ap_id("https://h", id),
            format!("https://h/objects/{id}"),
            "the id sits under the instance's objects path"
        );
    }

    #[test]
    fn local_object_uuid_round_trips_object_ap_id() {
        let base = "https://h";
        let id = new_id();
        assert_eq!(
            local_object_uuid(base, &object_ap_id(base, id)),
            Some(id),
            "a minted object id reads back as its uuid"
        );
    }

    #[test]
    fn local_object_uuid_rejects_other_ids() {
        let base = "https://h";
        let id = new_id();
        for ap_id in [
            format!("https://remote.example/objects/{id}"),
            format!("{base}/notice/{id}"),
            format!("{base}/activities/{id}"),
            format!("{base}/objects/chat-1"),
            format!("{base}/objects/"),
            format!("{base}/objects/{id}/replies"),
            format!("{base}x/objects/{id}"),
            id.to_string(),
        ] {
            assert_eq!(
                local_object_uuid(base, &ap_id),
                None,
                "{ap_id} is not a local object id"
            );
        }
    }

    #[test]
    fn new_activity_ap_ids_differ_and_parse_back() {
        let prefix = "https://h/activities/";
        let first = new_activity_ap_id("https://h");
        let second = new_activity_ap_id("https://h");
        assert_ne!(first, second, "every minted activity is its own object");
        for minted in [&first, &second] {
            let tail = minted
                .strip_prefix(prefix)
                .unwrap_or_else(|| panic!("{minted} is under {prefix}"));
            let id = parse_id(tail).expect("the tail is an id");
            assert_eq!(
                id.get_version(),
                Some(Version::SortRand),
                "the tail is a UUIDv7"
            );
        }
    }

    #[test]
    fn assert_local_activity_id_returns_the_tail() {
        let id = new_id();
        let ap_id = activity_ap_id(crate::config::TEST_BASE_URL, id);
        assert_eq!(
            assert_local_activity_id(Some(&ap_id)),
            id.to_string(),
            "the helper reduces the id to its uuid text"
        );
    }

    #[test]
    #[should_panic(expected = "carries a minted activity id")]
    fn assert_local_activity_id_rejects_a_missing_id() {
        drop(assert_local_activity_id(None));
    }

    #[test]
    #[should_panic(expected = "this instance's")]
    fn assert_local_activity_id_rejects_a_foreign_id() {
        drop(assert_local_activity_id(Some(
            "https://elsewhere.example/activities/x",
        )));
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
