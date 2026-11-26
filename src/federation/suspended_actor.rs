//! The actor document of a deactivated or suspended local account.
//!
//! Mastodon's `ActivityPub::ActorSerializer` answers 200 for a suspended
//! account, with `"suspended": true` and the profile stripped. The route
//! does the same (a 410 would make a peer treat the account as deleted, which
//! re-enabling could not undo), and the `Update` queued on every activation
//! change carries this same document. [`for_activity`] is the single place
//! that turns the full document into the minimal one.

use serde_json::{Value, json};

/// The members the minimal document keeps: identity, endpoints and key.
///
/// A peer needs them to deliver to the account and to verify its signatures.
/// Everything else is profile, which a suspension hides.
const KEPT: [&str; 11] = [
    "@context",
    "id",
    "type",
    "preferredUsername",
    "url",
    "inbox",
    "outbox",
    "followers",
    "following",
    "endpoints",
    "publicKey",
];

/// `document` as the account's own actor document: unchanged while it is
/// `active`, otherwise stripped to the kept members plus `"suspended": true`.
#[must_use]
pub fn for_activity(mut document: Value, active: bool) -> Value {
    if active {
        return document;
    }
    if let Some(fields) = document.as_object_mut() {
        fields.retain(|name, _value| KEPT.contains(&name.as_str()));
        drop(fields.insert("suspended".to_owned(), json!(true)));
    }
    document
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full document, profile and all.
    fn full() -> Value {
        json!({
            "@context": ["https://www.w3.org/ns/activitystreams"],
            "id": "https://test.example/users/alice",
            "type": "Person",
            "preferredUsername": "alice",
            "name": "Alice",
            "summary": "<p>hello</p>",
            "url": "https://test.example/users/alice",
            "inbox": "https://test.example/users/alice/inbox",
            "outbox": "https://test.example/users/alice/outbox",
            "followers": "https://test.example/users/alice/followers",
            "following": "https://test.example/users/alice/following",
            "featured": "https://test.example/users/alice/collections/featured",
            "endpoints": {"sharedInbox": "https://test.example/inbox"},
            "publicKey": {"id": "https://test.example/users/alice#main-key"},
            "icon": {"type": "Image", "url": "https://test.example/a.png"},
            "attachment": [{"type": "PropertyValue", "name": "a", "value": "b"}],
            "tag": [],
        })
    }

    #[test]
    fn an_active_account_keeps_its_whole_document() {
        assert_eq!(for_activity(full(), true), full(), "untouched");
    }

    #[test]
    fn an_inactive_account_keeps_identity_and_keys_and_says_suspended() {
        let minimal = for_activity(full(), false);

        assert_eq!(minimal["suspended"], true, "the flag");
        for kept in KEPT {
            assert!(minimal.get(kept).is_some(), "{kept} stays");
        }
        for stripped in ["name", "summary", "icon", "attachment", "tag", "featured"] {
            assert!(minimal.get(stripped).is_none(), "{stripped} is stripped");
        }
        assert_eq!(
            minimal.as_object().map(serde_json::Map::len),
            Some(KEPT.len() + 1),
            "nothing but the kept members and the flag"
        );
    }

    #[test]
    fn a_non_object_document_is_returned_as_it_is() {
        assert_eq!(for_activity(json!("x"), false), json!("x"), "no members");
    }
}
