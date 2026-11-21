//! Fixtures shared by the tests of the remote-actor submodules.

use serde_json::Value;

/// The Mastodon-style fixture: every field in its common single-value
/// spelling, plus a hostile `summary` for the sanitizer.
pub(super) const MASTODON: &str = include_str!("../../../tests/fixtures/mastodon_actor.json");
/// The Pleroma-style fixture: the array and object spellings —
/// `publicKey` and `icon` as arrays, `url` as `Link` objects, `outbox`
/// as a collection object, `image` as a bare string.
pub(super) const PLEROMA: &str = include_str!("../../../tests/fixtures/pleroma_actor.json");

/// Parses fixture text into JSON; broken fixture files fail the test.
pub(super) fn fixture(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

/// Removes `field` from the document's top-level object.
pub(super) fn remove(document: &mut Value, field: &str) {
    let _gone = document.as_object_mut().unwrap().remove(field);
}
