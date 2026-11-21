//! The document helpers the pipeline shares: reading an activity's `type`
//! and its origin host, finding the embedded note a content rule speaks
//! about, and the addressing and `attachment` rewrites the domain-list
//! policy and its neighbours perform on it.

use serde_json::{Map, Value};

use crate::federation::{
    json,
    note_parser::{OBJECT_TYPES, PUBLIC_RECIPIENTS},
};

use super::{ADDRESS_FIELDS, FOLLOWERS_SUFFIX, PUBLIC_ADDRESS, WRAPPING_TYPES};

/// The first string of a document's `type`, which may itself be an array.
pub(in crate::federation::mrf) fn activity_type(document: &Value) -> Option<String> {
    document.get("type").and_then(json::first_type)
}

/// The host of an activity's `actor` — the origin every list of this policy
/// compares against. `None` for a document that names no actor, or whose
/// actor is not an `http(s)` URL.
pub(in crate::federation::mrf) fn activity_host(activity: &Value) -> Option<String> {
    reference_host(activity.get("actor"))
}

/// The host an actor document's own `id` names, `None` when the document
/// carries no parsable one.
pub(super) fn actor_document_host(actor: &Value) -> Option<String> {
    reference_host(actor.get("id"))
}

/// The host a reference value points at, in any of the shapes
/// [`json::value_or_id`] reads.
fn reference_host(value: Option<&Value>) -> Option<String> {
    value
        .and_then(json::value_or_id)
        .and_then(|reference| json::http_host(&reference))
}

/// Whether the document is a `Create`/`Update` whose `object` is embedded
/// and of a note-family type: the shape the content rules speak about.
pub(in crate::federation::mrf) fn is_note_activity(activity: &Value) -> bool {
    let wraps = activity_type(activity).is_some_and(|kind| WRAPPING_TYPES.contains(&kind.as_str()));
    let note = activity
        .get("object")
        .and_then(Value::as_object)
        .and_then(|object| object.get("type"))
        .and_then(json::first_type)
        .is_some_and(|kind| OBJECT_TYPES.contains(&kind.as_str()));
    wraps && note
}

/// The embedded note object of such an activity, mutably; nothing else.
pub(in crate::federation::mrf) fn note_object_mut(
    activity: &mut Value,
) -> Option<&mut Map<String, Value>> {
    if !is_note_activity(activity) {
        return None;
    }
    activity.get_mut("object").and_then(Value::as_object_mut)
}

/// Whether a note carries attachments: a `null` and an empty array are
/// none, anything else — one entry or many — is at least one.
pub(in crate::federation::mrf) fn has_attachments(object: &Map<String, Value>) -> bool {
    object
        .get("attachment")
        .is_some_and(|value| !value.is_null() && !json::as_array(value).is_empty())
}

/// The author's followers collection: an address the documents already
/// carry when one names a `/followers` URL on the actor's own host, and the
/// `{actor}/followers` convention otherwise — the spelling Pleroma,
/// Mastodon and this instance's own actors use, and the one
/// [`parse_note`](crate::federation::note_parser::parse_note) compares the
/// author's stored `followers_url` against.
pub(in crate::federation::mrf) fn followers_collection(activity: &Value) -> String {
    let actor = activity
        .get("actor")
        .and_then(json::value_or_id)
        .unwrap_or_default();
    let host = json::http_host(&actor);
    addressed_followers(activity, host.as_deref())
        .unwrap_or_else(|| format!("{actor}{FOLLOWERS_SUFFIX}"))
}

/// The followers-collection address of `host` the activity or its embedded
/// object already addresses.
fn addressed_followers(activity: &Value, host: Option<&str>) -> Option<String> {
    [activity, activity.get("object")?]
        .into_iter()
        .flat_map(|document| {
            ADDRESS_FIELDS
                .iter()
                .flat_map(move |field| document.get(*field).map_or_else(Vec::new, json::references))
        })
        .find(|address| is_followers_of(address, host))
}

/// Whether `address` is a `/followers` collection URL on `host`.
fn is_followers_of(address: &str, host: Option<&str>) -> bool {
    address.ends_with(FOLLOWERS_SUFFIX)
        && host.is_some_and(|wanted| json::http_host(address).is_some_and(|found| found == wanted))
}

/// Applies one document-level rewrite to the embedded object and then to
/// the activity itself: the two places a peer spells a note's addressing.
pub(in crate::federation::mrf) fn rewrite_each_document<F>(activity: &mut Value, mut rewrite: F)
where
    F: FnMut(&mut Map<String, Value>),
{
    if let Some(object) = activity.get_mut("object").and_then(Value::as_object_mut) {
        rewrite(object);
    }
    if let Some(root) = activity.as_object_mut() {
        rewrite(root);
    }
}

/// Moves the public address out of a document's `to` and into its `cc`: the
/// note becomes unlisted. A document whose `to` names nothing public is
/// left alone — there is no timeline to remove it from.
pub(in crate::federation::mrf) fn unlist_document(document: &mut Map<String, Value>) {
    if !addresses_public(document, "to") {
        return;
    }
    remove_public(document, "to");
    add_address(document, "cc", PUBLIC_ADDRESS);
}

/// Takes the public address out of a document's `to` and `cc` and puts the
/// author's followers collection in `to`: the note becomes followers-only.
/// A note nobody public was addressed (a direct one) keeps its recipients:
/// widening a private post to the whole follower set is not what limiting
/// it means.
pub(in crate::federation::mrf) fn limit_document(
    document: &mut Map<String, Value>,
    followers: &str,
) {
    let public = addresses_public(document, "to") || addresses_public(document, "cc");
    if !public {
        return;
    }
    remove_public(document, "to");
    remove_public(document, "cc");
    add_address(document, "to", followers);
}

/// Whether `field` of `document` names the public address.
pub(in crate::federation::mrf) fn addresses_public(
    document: &Map<String, Value>,
    field: &str,
) -> bool {
    document
        .get(field)
        .is_some_and(|value| json::as_array(value).iter().any(is_public_address))
}

/// Drops every spelling of the public address from `field`. A field given
/// as a single value is first normalized into the one-element array it
/// stands for, so the bare `"to": "…#Public"` spelling goes empty rather
/// than staying behind.
pub(in crate::federation::mrf) fn remove_public(document: &mut Map<String, Value>, field: &str) {
    let Some(value) = document.get_mut(field) else {
        return;
    };
    if !value.is_array() {
        *value = Value::Array(json::as_array(value).to_vec());
    }
    if let Value::Array(entries) = value {
        entries.retain(|entry| !is_public_address(entry));
    }
}

/// Drops one exact reference from `field`, normalizing a single-valued
/// field into the one-element array it stands for, as
/// [`remove_public`] does for the public address.
pub(in crate::federation::mrf) fn remove_reference(
    document: &mut Map<String, Value>,
    field: &str,
    address: &str,
) {
    let Some(value) = document.get_mut(field) else {
        return;
    };
    if !value.is_array() {
        *value = Value::Array(json::as_array(value).to_vec());
    }
    if let Value::Array(entries) = value {
        entries.retain(|entry| json::value_or_id(entry).as_deref() != Some(address));
    }
}

/// Adds `address` to one document's `field` unless it already names it,
/// creating the field as an array when the document carries none.
pub(in crate::federation::mrf) fn add_address(
    document: &mut Map<String, Value>,
    field: &str,
    address: &str,
) {
    let mut entries = document
        .get(field)
        .map_or_else(Vec::new, |value| json::as_array(value).to_vec());
    if entries
        .iter()
        .any(|entry| json::value_or_id(entry).as_deref() == Some(address))
    {
        return;
    }
    entries.push(Value::String(address.to_owned()));
    let _previous = document.insert(field.to_owned(), Value::Array(entries));
}

/// Whether an addressing entry is the public address in any of the
/// [`PUBLIC_RECIPIENTS`] spellings, reference-object form included.
fn is_public_address(entry: &Value) -> bool {
    json::value_or_id(entry).is_some_and(|address| PUBLIC_RECIPIENTS.contains(&address.as_str()))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        federation::{mrf::simple::test_support::*, note_parser},
        models::{
            status::Visibility,
            user::{self, Flag, NewUser},
        },
    };

    /// [`List::FederatedTimelineRemoval`](crate::federation::mrf::simple::List::FederatedTimelineRemoval):
    /// the public address moves from `to` to `cc`, on the activity and on
    /// the note it embeds, and nothing else about the document changes.
    #[test]
    fn federated_timeline_removal_unlists_a_note() {
        let lists = json!({"federated_timeline_removal": ["peer.example"]});
        let expected = json!({
            "id": format!("https://{PEER}/activities/1"),
            "type": "Create",
            "actor": ALICE,
            "to": [],
            "cc": [FOLLOWERS, BOB, PUBLIC],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [],
                "cc": [FOLLOWERS, BOB, PUBLIC],
                "content": "<p>hello</p>",
            },
        });

        assert_eq!(
            accepted(&lists, create()),
            expected,
            "the note leaves the public timeline"
        );
        assert_eq!(
            accepted(&lists, follow()),
            follow(),
            "a `Follow` from the same host carries no note and is left alone"
        );
    }

    /// [`List::FollowersOnly`](crate::federation::mrf::simple::List::FollowersOnly): the public address goes and the author's
    /// followers collection takes its place in `to`; a note nobody public
    /// was addressed keeps the recipients it had.
    #[test]
    fn followers_only_limits_a_public_note_to_its_followers() {
        let lists = json!({"followers_only": ["peer.example"]});
        let expected = json!({
            "id": format!("https://{PEER}/activities/1"),
            "type": "Create",
            "actor": ALICE,
            "to": [FOLLOWERS],
            "cc": [FOLLOWERS, BOB],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [FOLLOWERS],
                "cc": [FOLLOWERS, BOB],
                "content": "<p>hello</p>",
            },
        });

        assert_eq!(
            accepted(&lists, create()),
            expected,
            "public gone, followers in `to`"
        );

        let direct = json!({
            "id": format!("https://{PEER}/activities/5"),
            "type": "Create",
            "actor": ALICE,
            "to": [BOB],
            "object": {"id": NOTE, "type": "Note", "attributedTo": ALICE, "to": [BOB]},
        });
        assert_eq!(
            accepted(&lists, direct.clone()),
            direct,
            "a direct note is not widened into a followers post"
        );
    }

    /// A followers-only note whose author is the stored mirror parses as
    /// `private`, and an unlisted one as `unlisted`: the rewrites are what
    /// this instance reads back, not only what it sends on.
    #[tokio::test]
    async fn the_addressing_rewrites_read_back_as_their_visibility() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let author = stored_author(db.pool()).await;

        let limited = accepted(&json!({"followers_only": [PEER]}), create());
        let parsed = note_parser::parse_note(object_of(&limited), &author, Utc::now())
            .expect("the rewritten note parses");
        assert_eq!(
            parsed.status.visibility,
            Visibility::Private,
            "followers-only is what the parser sees"
        );

        let unlisted = accepted(&json!({"federated_timeline_removal": [PEER]}), create());
        let relaxed = note_parser::parse_note(object_of(&unlisted), &author, Utc::now())
            .expect("the rewritten note parses");
        assert_eq!(
            relaxed.status.visibility,
            Visibility::Unlisted,
            "and the timeline removal reads as unlisted"
        );
    }

    /// [`List::MediaRemoval`](crate::federation::mrf::simple::List::MediaRemoval) drops a note's attachments and nothing else.
    #[test]
    fn media_removal_strips_a_notes_attachments() {
        let lists = json!({"media_removal": ["peer.example"]});
        let expected = json!({
            "id": format!("https://{PEER}/activities/1"),
            "type": "Create",
            "actor": ALICE,
            "to": [PUBLIC],
            "cc": [FOLLOWERS, BOB],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [PUBLIC],
                "cc": [FOLLOWERS, BOB],
                "content": "<p>hello</p>",
            },
        });

        assert_eq!(
            accepted(&lists, create_with_media()),
            expected,
            "the note stands without its attachment"
        );
        assert_eq!(
            accepted(&lists, create()),
            create(),
            "a note without attachments is already what it should be"
        );
    }

    /// [`List::MediaNsfw`](crate::federation::mrf::simple::List::MediaNsfw) marks a note that has attachments sensitive —
    /// one it already was, and nothing a note without media.
    #[test]
    fn media_nsfw_marks_notes_that_carry_media() {
        let lists = json!({"media_nsfw": ["peer.example"]});
        let expected = {
            let mut marked = create_with_media();
            marked["object"]["sensitive"] = json!(true);
            marked
        };

        assert_eq!(
            accepted(&lists, create_with_media()),
            expected,
            "the attachment is flagged as the policy asks"
        );
        assert_eq!(
            accepted(&lists, create()),
            create(),
            "a note with no media gets no flag"
        );
        let both = json!({"media_nsfw": [PEER], "media_removal": [PEER]});
        let stripped = accepted(&both, create_with_media());
        assert_eq!(
            stripped["object"]["sensitive"],
            json!(true),
            "with both lists on, the note loses its media and gains the flag"
        );
        assert!(
            stripped["object"].get("attachment").is_none(),
            "the removal still happens"
        );
    }

    /// The note rules speak about an `Update` as they do about a `Create`,
    /// and the followers collection they write is the one the document
    /// already addresses — even in its reference-object spelling, whose
    /// string form is then not duplicated.
    #[test]
    fn an_update_is_rewritten_like_a_create_and_the_documents_own_followers_url_wins() {
        let lists = json!({"followers_only": [PEER], "media_removal": [PEER]});
        let update = json!({
            "id": format!("https://{PEER}/activities/6"),
            "type": "Update",
            "actor": {"id": ALICE, "type": "Person"},
            "to": [{"type": "Collection", "id": FOLLOWERS}, PUBLIC],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [PUBLIC],
                "cc": [{"type": "OrderedCollection", "id": FOLLOWERS}],
                "attachment": [{"type": "Document", "url": format!("https://{PEER}/media/2.png")}],
            },
        });

        let rewritten = accepted(&lists, update);

        assert_eq!(
            rewritten["to"],
            json!([{"type": "Collection", "id": FOLLOWERS}]),
            "the activity keeps the collection it addressed and loses the public one"
        );
        assert_eq!(
            rewritten["object"]["to"],
            json!([FOLLOWERS]),
            "the note gains the very same followers URL"
        );
        assert_eq!(
            rewritten["object"]["cc"],
            json!([{"type": "OrderedCollection", "id": FOLLOWERS}]),
            "its reference-object entries are otherwise untouched"
        );
        assert!(
            rewritten["object"].get("attachment").is_none(),
            "and the media rule fired on the `Update` too"
        );
    }

    /// The stored mirror of the fixture author, with the followers
    /// collection the policy's fallback spells the same way.
    async fn stored_author(pool: &PgPool) -> user::User {
        user::insert(
            pool,
            &NewUser {
                nickname: "alice@peer.example".to_owned(),
                ap_id: ALICE.to_owned(),
                local: Flag::new(false),
                domain: Some(PEER.to_owned()),
                followers_url: Some(FOLLOWERS.to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the author fixture inserts")
    }

    /// The embedded note of a `Create` fixture.
    fn object_of(activity: &Value) -> &Value {
        activity.get("object").expect("the fixture embeds its note")
    }
}
