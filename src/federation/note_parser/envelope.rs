//! The document envelope: the checks every received object must pass
//! before its fields are read — size, accepted `type`, usable `id` and the
//! containment rules against the announcing author — and the id/url pair
//! the rest of the parse works from.

use serde_json::Value;
use url::Url;

use crate::{
    federation::json::{as_array, first_string, value_or_id},
    models::user::User,
};

use super::{MAX_NOTE_BYTES, OBJECT_TYPES, ParseError};

/// Refuses documents whose serialization is larger than [`MAX_NOTE_BYTES`].
pub(super) fn check_size(json: &Value) -> Result<(), ParseError> {
    let bytes = serde_json::to_vec(json)
        .map_err(|err| ParseError::Invalid(format!("the document cannot be serialized: {err}")))?;
    if bytes.len() > MAX_NOTE_BYTES {
        return Err(ParseError::TooLarge);
    }
    Ok(())
}

/// The document's `type`, validated against [`OBJECT_TYPES`]; anything else
/// — including a missing or non-string type — is
/// [`ParseError::UnsupportedType`].
pub(super) fn object_kind(json: &Value) -> Result<&'static str, ParseError> {
    let seen = json.get("type").and_then(Value::as_str);
    OBJECT_TYPES
        .into_iter()
        .find(|kind| Some(*kind) == seen)
        .ok_or_else(|| ParseError::UnsupportedType(seen.unwrap_or("<missing>").to_owned()))
}

/// The object's `id` and its host: a plain string (objects name themselves,
/// like actors) spelling an absolute `http(s)` url. The id is kept verbatim,
/// the host as URL parsing lowercases it.
pub(super) fn object_id(json: &Value) -> Result<(String, String), ParseError> {
    let raw = json
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ParseError::Invalid("`id` is missing or not a string".to_owned()))?;
    let host = http_host(raw)
        .ok_or_else(|| ParseError::Invalid("`id` is not an absolute http(s) URL".to_owned()))?;
    Ok((raw.to_owned(), host))
}

/// The authorship rules of the ticket: the first `id` (or bare string)
/// reachable in `attributedTo` must equal `author.ap_id`, and the host it
/// names must equal the object id's host. Pleroma applies the same pair in
/// `validate_fields_match` and `validate_host_match`.
pub(super) fn check_containment(
    json: &Value,
    author: &User,
    id_host: &str,
) -> Result<(), ParseError> {
    let attributed = json
        .get("attributedTo")
        .map_or_default(as_array)
        .iter()
        .find_map(value_or_id)
        .ok_or_else(|| {
            ParseError::Containment("`attributedTo` is missing or names nothing".to_owned())
        })?;
    if attributed != author.ap_id {
        return Err(ParseError::Containment(format!(
            "`attributedTo` {attributed} is not the author {}",
            author.ap_id
        )));
    }
    let actor_host = http_host(&attributed).ok_or_else(|| {
        ParseError::Containment("`attributedTo` is not an http(s) URL".to_owned())
    })?;
    if actor_host != id_host {
        return Err(ParseError::Containment(format!(
            "the object host {id_host} differs from the actor host {actor_host}"
        )));
    }
    Ok(())
}

/// The host of an absolute `http(s)` URL, `None` for anything else; URL
/// parsing lowercases the host, so hosts compare case-insensitively.
fn http_host(text: &str) -> Option<String> {
    let parsed = Url::parse(text).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed.host_str().map(str::to_owned)
}

/// True when `text` is an absolute `http(s)` URL with a host.
pub(super) fn is_http_url(text: &str) -> bool {
    http_host(text).is_some()
}

/// The human-facing url: the first string or `Link` `href` of `url` when it
/// is an `http(s)` URL, the object id otherwise (Pleroma's `fix_url`).
pub(super) fn resolve_url(json: &Value, id: &str) -> String {
    json.get("url")
        .and_then(first_string)
        .filter(|candidate| is_http_url(candidate))
        .unwrap_or_else(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::note_parser::{TYPE_QUESTION, parse_note, test_support::*};

    /// One rejection case: what to call it, how to break a copy of the
    /// Mastodon fixture, and the expected failure.
    type Rejection = (&'static str, fn(&mut Value), fn(&ParseError) -> bool);

    /// Documents built from the Mastodon fixture by breaking exactly one
    /// rule each; none of them may parse.
    fn rejections() -> Vec<Rejection> {
        vec![
            (
                "attributedTo missing",
                |doc| remove(doc, "attributedTo"),
                |err| matches!(err, ParseError::Containment(_)),
            ),
            (
                "attributedTo names someone else",
                |doc| doc["attributedTo"] = json!("https://remote.example/users/carol"),
                |err| matches!(err, ParseError::Containment(_)),
            ),
            (
                "attributedTo is an empty array",
                |doc| doc["attributedTo"] = json!([]),
                |err| matches!(err, ParseError::Containment(_)),
            ),
            (
                "attributedTo is a number",
                |doc| doc["attributedTo"] = json!(7),
                |err| matches!(err, ParseError::Containment(_)),
            ),
            (
                "the object lives on another host",
                |doc| doc["id"] = json!("https://evil.example/users/bob/statuses/1"),
                |err| matches!(err, ParseError::Containment(_)),
            ),
            (
                "no id",
                |doc| remove(doc, "id"),
                |err| matches!(err, ParseError::Invalid(_)),
            ),
            (
                "array id",
                |doc| doc["id"] = json!(["https://remote.example/objects/1"]),
                |err| matches!(err, ParseError::Invalid(_)),
            ),
            (
                "ftp id",
                |doc| doc["id"] = json!("ftp://remote.example/objects/1"),
                |err| matches!(err, ParseError::Invalid(_)),
            ),
            (
                "relative id",
                |doc| doc["id"] = json!("/objects/1"),
                |err| matches!(err, ParseError::Invalid(_)),
            ),
            (
                "a Tombstone",
                |doc| doc["type"] = json!("Tombstone"),
                |err| matches!(err, ParseError::UnsupportedType(_)),
            ),
            (
                "a lowercase type",
                |doc| doc["type"] = json!("note"),
                |err| matches!(err, ParseError::UnsupportedType(_)),
            ),
            (
                "an array type",
                |doc| doc["type"] = json!(["Note"]),
                |err| matches!(err, ParseError::UnsupportedType(_)),
            ),
            (
                "no type",
                |doc| remove(doc, "type"),
                |err| matches!(err, ParseError::UnsupportedType(_)),
            ),
        ]
    }

    #[test]
    fn documents_that_are_not_own_posts_are_rejected() {
        for (name, break_it, expected) in rejections() {
            let mut document = fixture(MASTODON);
            break_it(&mut document);
            let error = parse_note(&document, &author(), now()).unwrap_err();
            assert!(
                expected(&error),
                "{name}: rejected with the wrong error: {error}"
            );
        }
    }

    #[test]
    fn every_accepted_object_type_parses() {
        for kind in OBJECT_TYPES {
            let mut document = minimal_note();
            document["type"] = json!(kind);
            if kind == TYPE_QUESTION {
                document["oneOf"] = json!([{"name": "a"}, {"name": "b"}]);
            }
            let parsed = parse(&document);
            assert!(
                parsed.status.content.contains("<p>hello</p>"),
                "{kind} keeps its body as content, got {}",
                parsed.status.content
            );
        }
    }

    #[test]
    fn non_object_documents_are_unsupported() {
        for document in [
            json!("https://x.example/o"),
            json!([]),
            json!(null),
            json!(1),
        ] {
            let error = parse_note(&document, &author(), now()).unwrap_err();
            assert!(
                matches!(error, ParseError::UnsupportedType(_)),
                "a document that is not an object fails UnsupportedType, got {error}"
            );
        }
    }

    #[test]
    fn oversized_documents_are_rejected() {
        let mut document = minimal_note();
        document["content"] = json!("x".repeat(2_500_000));
        let error = parse_note(&document, &author(), now()).unwrap_err();
        assert_eq!(error, ParseError::TooLarge, "2.5 MB goes over the cap");
        document["content"] = json!("x".repeat(2_000_000));
        let kept = parse(&document);
        assert_eq!(
            kept.status.content.chars().count(),
            200_000,
            "just under the cap parses, and only the content caps"
        );
    }
}
