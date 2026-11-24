//! The `ActivityStreams` vocabulary constants every federation module shares.
//!
//! They are the public addressing collection, its accepted spellings, the
//! core vocabulary IRI, and the two predicates that test an addressing
//! entry for membership of the public collection.
//!
//! Defining them once means a change to the accepted spellings (a new
//! reference-object form, say) is made here and reaches the note parser,
//! the publisher and the MRF policies together.

use serde_json::Value;

use super::json;
use crate::util::media_type::essence;

/// The `ActivityStreams` public addressing collection, the one recipient
/// that means "everyone": the `to` of a public post, the `cc` of an
/// unlisted one or of a publicly-addressed interaction.
pub const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// The three spellings of the public addressing recipient.
///
/// They are the full `ActivityStreams` iri, the prefix form Pleroma emits,
/// and the bare word older software sends (Pleroma's
/// `fix_addressing_public` normalizes all three to the iri).
pub const PUBLIC_RECIPIENTS: [&str; 3] = [AS_PUBLIC, "as:Public", "Public"];

/// The `ActivityStreams` core vocabulary, first entry of every `@context`.
pub const AS_VOCABULARY: &str = "https://www.w3.org/ns/activitystreams";

/// The `ActivityPub` media type Mastodon and Pleroma serve documents as.
pub const ACTIVITY_JSON: &str = "application/activity+json";

/// The JSON-LD media type, the second spelling of an `ActivityPub`
/// document. Whether its `profile` parameter is checked is up to the caller.
pub const LD_JSON: &str = "application/ld+json";

/// The two media types that name an `ActivityPub` document, lowercase and
/// without parameters.
pub const AP_MEDIA_TYPES: [&str; 2] = [ACTIVITY_JSON, LD_JSON];

/// Whether a `Content-Type`-style value names an `ActivityPub` media type:
/// its [`essence`] (parameters dropped, trimmed, lowercased) is one of the
/// [`AP_MEDIA_TYPES`].
#[must_use]
pub fn is_ap_media_type(content_type: &str) -> bool {
    AP_MEDIA_TYPES.contains(&essence(content_type).as_str())
}

/// Whether `address` is the public addressing recipient in any of the
/// [`PUBLIC_RECIPIENTS`] spellings.
#[must_use]
pub fn is_public_address(address: &str) -> bool {
    PUBLIC_RECIPIENTS.contains(&address)
}

/// Whether an addressing entry is the public address in any of the
/// [`PUBLIC_RECIPIENTS`] spellings, reference-object form (`{"id": …}`)
/// included.
#[must_use]
pub fn value_is_public(entry: &Value) -> bool {
    json::value_or_id(entry).is_some_and(|address| is_public_address(&address))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn each_spelling_of_the_public_address_is_public() {
        for spelling in PUBLIC_RECIPIENTS {
            assert!(is_public_address(spelling), "`{spelling}` is public");
        }
        assert!(is_public_address(AS_PUBLIC), "the full iri is public");
        assert!(is_public_address("as:Public"), "the prefix form is public");
        assert!(is_public_address("Public"), "the bare word is public");
    }

    #[test]
    fn the_first_spelling_is_the_public_iri() {
        assert_eq!(PUBLIC_RECIPIENTS[0], AS_PUBLIC, "the iri leads the list");
        assert_eq!(
            AS_PUBLIC,
            format!("{AS_VOCABULARY}#Public"),
            "the public collection lives in the vocabulary"
        );
    }

    #[test]
    fn other_addresses_are_not_public() {
        assert!(!is_public_address("public"), "the match is case-sensitive");
        assert!(
            !is_public_address("https://peer.example/users/bob/followers"),
            "a followers collection is not public"
        );
        assert!(!is_public_address(""), "the empty string is not public");
    }

    #[test]
    fn both_activitypub_types_are_ap_media_types() {
        for candidate in [
            "application/activity+json",
            "application/activity+json; charset=utf-8",
            "APPLICATION/Activity+JSON",
            " application/activity+json ; charset=utf-8 ",
            "application/ld+json",
            r#"application/ld+json; profile="https://www.w3.org/ns/activitystreams""#,
            "Application/LD+Json",
        ] {
            assert!(is_ap_media_type(candidate), "{candidate} names AP");
        }
    }

    #[test]
    fn other_media_types_are_not_ap_media_types() {
        for candidate in [
            "application/json",
            "text/html",
            "application/jrd+json",
            "activity+json",
            "",
        ] {
            assert!(!is_ap_media_type(candidate), "{candidate} does not name AP");
        }
    }

    #[test]
    fn the_list_holds_exactly_the_two_constants() {
        assert_eq!(
            AP_MEDIA_TYPES,
            [ACTIVITY_JSON, LD_JSON],
            "the pair, in order"
        );
    }

    #[test]
    fn a_value_is_public_by_string_or_reference_object() {
        assert!(value_is_public(&json!(AS_PUBLIC)), "a plain string");
        assert!(value_is_public(&json!("as:Public")), "a shorthand string");
        assert!(
            value_is_public(&json!({"id": AS_PUBLIC})),
            "a reference object"
        );
    }

    #[test]
    fn a_value_that_names_something_else_is_not_public() {
        assert!(!value_is_public(&json!({"id": "x"})), "another object id");
        assert!(!value_is_public(&json!("x")), "another string");
        assert!(!value_is_public(&json!(7)), "a number references nothing");
        assert!(!value_is_public(&Value::Null), "null references nothing");
        assert!(!value_is_public(&json!({})), "an empty object");
    }
}
