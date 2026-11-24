//! The delivery assertion toolkit the scenarios share: what one captured
//! delivery is expected to look like ([`Expected`], [`assert_delivered`]),
//! the addressing rows of the visibility matrix ([`NoteCase`],
//! [`note_cases`], [`public_case`]), the lookups and counts that find one
//! activity in the mock's capture ([`delivered_for`], [`count_delivered`],
//! [`count_at`], [`count_of`]), the entity readers ([`assert_flags`],
//! [`uri_of`], [`create_id_of`]) and the lifecycle asserts the
//! public-status scenario composes. Everything shared is `pub(super)`.

use serde_json::{Value, json};
use wired_be::{config::TEST_BASE_URL, models::user::User};

use super::support::{ACTIVITY_JSON, AS_PUBLIC, followers_of};
use crate::{common, remote};

// --- asserting the peer's capture ---------------------------------------------------------

/// What one captured delivery is expected to look like: where it arrived,
/// what it carries and whose key signed it.
#[derive(Debug)]
pub(super) struct Expected<'a> {
    /// The request path the mock recorded.
    pub(super) path: &'a str,
    /// The `type` of the delivered activity.
    pub(super) kind: &'a str,
    /// The local actor the document names and whose published key must
    /// authenticate the bytes.
    pub(super) actor: &'a User,
    /// The pinned `to` line, when the scenario fixes it.
    pub(super) to: Option<Value>,
    /// The pinned `cc` line, when the scenario fixes it.
    pub(super) cc: Option<Value>,
}

/// One row of the visibility matrix: where the note's kind arrives and the
/// two addressing lines it must carry, for every lifecycle activity.
#[derive(Debug)]
pub(super) struct NoteCase {
    /// The inbox path the mock records for this visibility.
    pub(super) path: String,
    /// The expected `to` line.
    pub(super) to: Value,
    /// The expected `cc` line.
    pub(super) cc: Value,
}

/// The four delivered visibilities' cases in the order
/// [`post_all_visibilities`] posts them: `public` talks to the world with
/// the followers in `cc`, `unlisted` inverts the pair, `private` reaches
/// followers alone, and `direct` the mentioned remote's own inbox.
pub(super) fn note_cases(bob_actor: &str, followers: &str, bob_personal: &str) -> [NoteCase; 4] {
    [
        NoteCase {
            path: remote::INBOX.to_owned(),
            to: json!([AS_PUBLIC]),
            cc: json!([followers]),
        },
        NoteCase {
            path: remote::INBOX.to_owned(),
            to: json!([followers]),
            cc: json!([AS_PUBLIC]),
        },
        NoteCase {
            path: remote::INBOX.to_owned(),
            to: json!([followers]),
            cc: json!([]),
        },
        NoteCase {
            path: bob_personal.to_owned(),
            to: json!([bob_actor]),
            cc: json!([]),
        },
    ]
}

/// `assert_delivered` shaped from a [`NoteCase`]: the same `kind` and
/// addressing the case pins.
pub(super) fn expected_case<'a>(
    case: &'a NoteCase,
    kind: &'a str,
    actor: &'a User,
) -> Expected<'a> {
    Expected {
        path: &case.path,
        kind,
        actor,
        to: Some(case.to.clone()),
        cc: Some(case.cc.clone()),
    }
}

/// Asserts `delivery` is this instance's own signed `POST` of the activity
/// [`Expected`] describes: one that arrived at the mock's `path` with the
/// four auth headers plus the media-type pair, whose `Digest` describes the
/// bytes as received, whose `Signature` names the actor's `#main-key` and
/// verifies against the key that actor publishes, and whose document carries
/// the pinned addressing with the blind-recipient lines never on the wire.
pub(super) fn assert_delivered(
    delivery: &remote::Delivery,
    expected: &Expected<'_>,
    mock_host: &str,
) {
    let body = delivery.activity();
    assert_eq!(delivery.path(), expected.path, "the arrival path");
    assert_eq!(body["type"], json!(expected.kind), "the activity type");
    assert_eq!(body["actor"], json!(expected.actor.ap_id), "the maker");
    let id = body["id"].as_str().expect("the activity carries its id");
    assert!(
        id.starts_with(&format!("{TEST_BASE_URL}/")),
        "an id on this instance's own origin: {id}"
    );
    assert_eq!(
        delivery.key_id().as_deref(),
        Some(format!("{}#main-key", expected.actor.ap_id)).as_deref(),
        "the signer is the actor the activity names"
    );
    assert!(
        delivery.digest_matches(),
        "the Digest describes the bytes that arrived"
    );
    let pem = expected
        .actor
        .public_key
        .as_deref()
        .expect("the fixture actor publishes its key");
    assert!(
        delivery.signature_verifies(pem),
        "and the Signature verifies against that key"
    );
    assert_eq!(
        delivery.header("content-type"),
        Some(ACTIVITY_JSON),
        "the body is labelled an activity"
    );
    assert_eq!(
        delivery.header("accept"),
        Some(ACTIVITY_JSON),
        "and the answer asked for in the same media type"
    );
    assert_eq!(delivery.header("host"), Some(mock_host), "the Host");
    let date = delivery.header("date").expect("a Date header is sent");
    assert!(date.ends_with(" GMT"), "a signed RFC1123 Date: {date}");
    assert!(
        delivery
            .header("digest")
            .is_some_and(|value| value.starts_with("SHA-256=")),
        "a SHA-256 Digest"
    );
    assert!(body.get("bto").is_none(), "no blind `to` on the wire");
    assert!(body.get("bcc").is_none(), "and no blind `cc`");
    if let Some(to) = &expected.to {
        assert_eq!(&body["to"], to, "the delivered `to` line");
    }
    if let Some(cc) = &expected.cc {
        assert_eq!(&body["cc"], cc, "the delivered `cc` line");
    }
}

/// Asserts the entity carries each `(key, value)` pair of `expected`
/// verbatim: one named fact per line.
pub(super) fn assert_flags(entity: &Value, expected: &[(&str, Value)]) {
    for (key, value) in expected {
        assert_eq!(
            entity.get(*key).cloned().unwrap_or(Value::Null),
            *value,
            "the entity's `{key}` reads as expected"
        );
    }
}

/// The single recorded delivery whose activity is a `kind` naming `uri` as
/// its object (embedded or by reference): the one copy of one document a
/// scenario asserts its addressing against.
///
/// # Panics
///
/// If `uri` was delivered by that `kind` more than once, or not at all.
pub(super) fn delivered_for<'a>(
    posts: &'a [remote::Delivery],
    kind: &str,
    uri: &str,
) -> &'a remote::Delivery {
    let found: Vec<&remote::Delivery> = posts
        .iter()
        .filter(|post| {
            post.activity_type() == Some(kind) && object_reference(post.activity()) == Some(uri)
        })
        .collect();
    assert_eq!(
        found.len(),
        1,
        "exactly one {kind} of `{uri}` was delivered"
    );
    found[0]
}

/// How many recorded deliveries carry `path` and a `kind` activity naming
/// `uri`: the count an addressing scenario compares against its table.
pub(super) fn count_delivered(
    posts: &[remote::Delivery],
    path: &str,
    kind: &str,
    uri: &str,
) -> usize {
    posts
        .iter()
        .filter(|post| {
            post.path() == path
                && post.activity_type() == Some(kind)
                && object_reference(post.activity()) == Some(uri)
        })
        .count()
}

/// How many recorded deliveries arrived at `path`: the positive silence an
/// addressing scenario reads off the capture.
pub(super) fn count_at(posts: &[remote::Delivery], path: &str) -> usize {
    posts.iter().filter(|post| post.path() == path).count()
}

/// How many recorded deliveries carry `kind`.
pub(super) fn count_of(posts: &[remote::Delivery], kind: &str) -> usize {
    posts
        .iter()
        .filter(|post| post.activity_type() == Some(kind))
        .count()
}

/// The `object` of a delivered activity as a uri: the embedded document's
/// own `id`, or the plain string an `Announce` / `Like` names it with.
fn object_reference(activity: &Value) -> Option<&str> {
    activity.get("object").and_then(|object| {
        object
            .as_str()
            .or_else(|| object.get("id").and_then(Value::as_str))
    })
}

/// The `uri` of an entity — for a boost, the id its `Announce` federates
/// under.
pub(super) fn uri_of(entity: &Value) -> String {
    entity["uri"]
        .as_str()
        .expect("the entity uri is a string")
        .to_owned()
}

/// The deterministic `Create` id of a posted status entity (decision 6).
pub(super) fn create_id_of(entity: &Value) -> String {
    format!("{TEST_BASE_URL}/activities/{}", common::id_of(entity))
}

/// Asserts the `Create` of the lifecycle test: decision 6's deterministic
/// id, the author's note embedded, public addressing on both lines.
pub(super) fn assert_lifecycle_create(
    posts: &[remote::Delivery],
    alice: &User,
    host: &str,
    uri: &str,
    create_id: &str,
) {
    let create = delivered_for(posts, "Create", uri);
    assert_delivered(
        create,
        &expected_case(&public_case(alice), "Create", alice),
        host,
    );
    assert_flags(create.activity(), &[("id", json!(create_id))]);
    assert!(
        create.activity()["published"].is_string(),
        "the Create stamps its publication"
    );
    assert_flags(
        &create.activity()["object"],
        &[
            ("id", json!(uri)),
            ("type", json!("Note")),
            ("attributedTo", json!(alice.ap_id)),
            ("to", json!([AS_PUBLIC])),
        ],
    );
}

/// Asserts the `Update` of the lifecycle test: a fresh activity id of this
/// instance's own — never the Create restated — the same reserved uri, the
/// rewritten content and the edit stamp.
pub(super) fn assert_lifecycle_update(
    posts: &[remote::Delivery],
    alice: &User,
    host: &str,
    uri: &str,
    create_id: &str,
) {
    let update = delivered_for(posts, "Update", uri);
    assert_delivered(
        update,
        &expected_case(&public_case(alice), "Update", alice),
        host,
    );
    let update_id = update.activity()["id"]
        .as_str()
        .expect("the Update carries its id");
    assert_ne!(
        update_id, create_id,
        "an edit is its own activity, not the Create restated"
    );
    assert!(
        update_id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
        "under a fresh id of this instance's own: {update_id}"
    );
    assert_flags(
        &update.activity()["object"],
        &[
            ("id", json!(uri)),
            ("content", json!("<p>second draft</p>")),
        ],
    );
    assert!(
        update.activity()["object"]["updated"].is_string(),
        "and the re-drafted note carries its edit stamp"
    );
}

/// Asserts the `Delete` of the lifecycle test: the deterministic
/// `delete-{status id}` id and the `Tombstone` the object route now serves.
pub(super) fn assert_lifecycle_delete(
    posts: &[remote::Delivery],
    alice: &User,
    host: &str,
    uri: &str,
    status_id: &str,
) {
    let delete = delivered_for(posts, "Delete", uri);
    assert_delivered(
        delete,
        &expected_case(&public_case(alice), "Delete", alice),
        host,
    );
    assert_flags(
        delete.activity(),
        &[(
            "id",
            json!(format!("{TEST_BASE_URL}/activities/delete-{status_id}")),
        )],
    );
    assert_flags(
        &delete.activity()["object"],
        &[
            ("id", json!(uri)),
            ("type", json!("Tombstone")),
            ("formerType", json!("Note")),
        ],
    );
}

/// The public note's case: `to` the world, `cc` the followers collection,
/// the mock's shared inbox the arrival path.
fn public_case(alice: &User) -> NoteCase {
    NoteCase {
        path: remote::INBOX.to_owned(),
        to: json!([AS_PUBLIC]),
        cc: json!([followers_of(alice)]),
    }
}
