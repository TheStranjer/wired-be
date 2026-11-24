//! The documents that carry, re-draft, retire or repeat a note: the
//! activity wrappers, the withdrawal and the shared JSON-LD context.
//!
//! [`create_activity_json`], [`update_activity_json`], the
//! [`Delete`](delete_activity_json) over a [`NoteDeletion`]'s
//! [`tombstone_json`], and the [`Announce`](announce_activity_json) a
//! reblog row federates as, over the shared `json_ld_context` header and
//! the [`litepub_context_json`] schema it names.

use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    config::Config,
    models::{
        status::{Status, Visibility},
        user::{User, followers},
    },
    util::time::mastodon_time,
};

use super::{
    AS_PUBLIC, AS_VOCABULARY, LANGUAGE_UNDETERMINED, LITEPUB_SCHEMA_PATH, TYPE_ANNOUNCE,
    TYPE_CREATE, TYPE_DELETE, TYPE_NOTE, TYPE_TOMBSTONE, TYPE_UPDATE,
};

/// The `@context` of every document this module and the
/// [`activities`](crate::federation::activities) beside it build: the
/// `ActivityStreams` vocabulary, this instance's [`litepub_context_json`]
/// schema, and the `und` language map Pleroma's `make_json_ld_header`
/// writes for a document that declares no language. The collection
/// documents in [`crate::federation::collections`] carry the same header.
pub(crate) fn json_ld_context(config: &Config) -> Value {
    json_ld_context_for(config, None)
}

/// The `@context` of a note written in `language`: [`json_ld_context`] with
/// the language map naming that BCP 47 tag, as Pleroma's
/// `make_json_ld_header` does for an object that has one. A `None` or empty
/// tag falls back to the `und` map.
pub(crate) fn json_ld_context_for(config: &Config, language: Option<&str>) -> Value {
    let language = language
        .filter(|tag| !tag.is_empty())
        .unwrap_or(LANGUAGE_UNDETERMINED);
    json!([
        AS_VOCABULARY,
        format!("{}{LITEPUB_SCHEMA_PATH}", config.base_url),
        { "@language": language },
    ])
}

/// The `@context` an activity wrapping `note` carries: the note's own, so
/// the two documents never disagree on the language, falling back to the
/// plain [`json_ld_context`] for an object that has none.
fn wrapped_context(note: &Value, config: &Config) -> Value {
    note.get("@context")
        .cloned()
        .unwrap_or_else(|| json_ld_context(config))
}

/// The JSON-LD context document served at [`LITEPUB_SCHEMA_PATH`].
///
/// The prefixes and term definitions this instance's own documents use: the
/// counterpart of Pleroma's `priv/static/schemas/litepub-0.1.jsonld`, cut
/// down to what [`note_json`](super::note_json) and the activity builders
/// actually write.
#[must_use]
pub fn litepub_context_json() -> Value {
    json!({
        "@context": {
            "as": "https://www.w3.org/ns/activitystreams#",
            "fedibird": "http://fedibird.com/ns#",
            "litepub": "http://litepub.social/ns#",
            "ostatus": "http://ostatus.org#",
            "toot": "http://joinmastodon.org/ns#",
            "Hashtag": "as:Hashtag",
            "Emoji": "toot:Emoji",
            "EmojiReact": "litepub:EmojiReact",
            "capabilities": "litepub:capabilities",
            "conversation": { "@id": "ostatus:conversation", "@type": "@id" },
            "directMessage": "litepub:directMessage",
            "quoteUri": "fedibird:quoteUri",
            "quoteUrl": "as:quoteUrl",
            "sensitive": "as:sensitive",
            "votersCount": "toot:votersCount",
        }
    })
}

/// The `Create` activity carrying `note` for `status`, whose `UUIDv7` also
/// names the activity: `{BASE_URL}/activities/{id}`, the address a later
/// ticket will resolve against the status row (decision 6).
///
/// The addressing mirrors the object's — Pleroma's `make_create_data`
/// copies `to`/`cc` from the object, and the actor is the object's
/// `attributedTo` — so the two documents can never disagree.
#[must_use]
pub fn create_activity_json(note: &Value, status: &Status, config: &Config) -> Value {
    json!({
        "@context": wrapped_context(note, config),
        "id": format!("{}/activities/{}", config.base_url, status.id),
        "type": TYPE_CREATE,
        "actor": note.get("actor").cloned().unwrap_or(Value::Null),
        "to": note.get("to").cloned().unwrap_or_else(|| json!([])),
        "cc": note.get("cc").cloned().unwrap_or_else(|| json!([])),
        "object": note,
        "published": mastodon_time(&status.inserted_at),
    })
}

/// The `Update` activity re-drafting an edited status: the new note as its
/// object, under the addressing the note itself carries.
///
/// Like [`create_activity_json`] this is a copy of the object's own `actor`,
/// `to` and `cc` (Pleroma's `Builder.update` follows the object's recipients
/// for a status), so a re-draft travels to exactly the audience the post has
/// and the two documents can never disagree about who they reached.
///
/// The activity id is a FRESH one, not the `Create`'s: an edit is its own
/// activity, keyed on its own id (decision 6, [`activity_id`]'s shape),
/// while what it revises is named from inside — the object's reserved
/// `{BASE_URL}/objects/{uuid}` uri is the address a peer replaces. And the
/// activity carries no `published` of its own for the same reason the
/// withdrawal below carries none: the moment of the edit is the object's
/// `updated` fact, which is what a reader of the re-draft reads.
///
/// [`activity_id`]: crate::federation::activities::activity_id
#[must_use]
pub fn update_activity_json(note: &Value, id: Uuid, config: &Config) -> Value {
    json!({
        "@context": wrapped_context(note, config),
        "id": format!("{}/activities/{id}", config.base_url),
        "type": TYPE_UPDATE,
        "actor": note.get("actor").cloned().unwrap_or(Value::Null),
        "to": note.get("to").cloned().unwrap_or_else(|| json!([])),
        "cc": note.get("cc").cloned().unwrap_or_else(|| json!([])),
        "object": note,
    })
}

/// Everything [`delete_activity_json`] renders one status withdrawal from:
/// the row as the delete left it, the author announcing it, and the two
/// addressing lines the status travelled under.
///
/// The addressing rides in as the caller's already-rendered lines because it
/// is not this document's to compute: the row behind it is a tombstone by the
/// time a withdrawal is built, so the lines come out of the note builder's
/// own addressing rule run over the status as it stood *before* the delete —
/// which is what makes the withdrawal reach exactly the inboxes that hold the
/// post.
#[derive(Debug)]
pub struct NoteDeletion<'a> {
    /// The instance whose origin mints the activity id.
    pub config: &'a Config,
    /// The deleted status: the reserved `uri` the `Tombstone` names, and
    /// the id the deterministic activity id is built from.
    pub status: &'a Status,
    /// The author's actor id — the `Delete`'s actor, the account whose key
    /// the delivery signs with.
    pub actor_ap_id: &'a str,
    /// The primary addressing line, as the note builder's rule rendered it.
    pub to: &'a [Value],
    /// The copy line, as the note builder's rule rendered it.
    pub cc: &'a [Value],
}

/// The `Delete` activity withdrawing a status: the row's `Tombstone` as its
/// object, under the deterministic `{BASE_URL}/activities/delete-{status id}`
/// id.
///
/// Mastodon accepts the bare uri too, and Pleroma's `Builder.delete` writes
/// that plain reference, but the `Tombstone` is what this instance serves at
/// that uri from the delete onwards, so a peer's copy and its fetch agree.
///
/// The id is derived rather than minted because the row's id is unique and
/// never reused: every (re)run of one delete announces the same activity,
/// and the delivery queue's per-activity, per-inbox dedup swallows a retry.
/// The activity carries no `published` of its own — the moment the post went
/// is the object's `deleted` fact, which is what a reader of the tombstone
/// is after.
#[must_use]
pub fn delete_activity_json(deletion: &NoteDeletion<'_>) -> Value {
    json!({
        "@context": json_ld_context(deletion.config),
        "id": format!(
            "{}/activities/delete-{}",
            deletion.config.base_url, deletion.status.id
        ),
        "type": TYPE_DELETE,
        "actor": deletion.actor_ap_id,
        "object": tombstone_json(deletion.status),
        "to": deletion.to,
        "cc": deletion.cc,
    })
}

/// The `Announce` a reblog row federates as.
///
/// Its own stored `uri` is already the `{BASE_URL}/activities/{id}` address
/// (decision 6), so this is that row spoken as an activity of `author`
/// boosting `original_uri` by `original_author_ap_id`.
///
/// The addressing is Pleroma's `make_announce_data`: a public or unlisted
/// boost goes to the booster's followers and the boosted author with the
/// public collection in `cc`, a private or direct one only to the
/// followers — and a `local` reblog is never announced at all (`None`).
#[must_use]
pub fn announce_activity_json(
    reblog: &Status,
    author: &User,
    original_uri: &str,
    original_author_ap_id: &str,
    config: &Config,
) -> Option<Value> {
    let followers_addr = json!(followers(&author.ap_id));
    let (to, cc) = match reblog.visibility {
        Visibility::Public | Visibility::Unlisted => (
            json!([followers_addr, original_author_ap_id]),
            json!([AS_PUBLIC]),
        ),
        Visibility::Private | Visibility::Direct => (json!([followers_addr]), json!([])),
        Visibility::Local => return None,
    };
    Some(json!({
        "@context": json_ld_context(config),
        "id": &reblog.uri,
        "type": TYPE_ANNOUNCE,
        "actor": &author.ap_id,
        "object": original_uri,
        "to": to,
        "cc": cc,
        "published": mastodon_time(&reblog.inserted_at),
    }))
}

/// The `Tombstone` a soft-deleted status is served as.
///
/// It carries the reserved object id, the type the post had lived as, and
/// the moment it went. A row that never recorded a deletion reports its
/// last write instead — callers that mean to tombstone a live status still
/// get a `deleted` they can sign.
#[must_use]
pub fn tombstone_json(status: &Status) -> Value {
    let deleted = status.deleted_at.unwrap_or(status.updated_at);
    json!({
        "id": &status.uri,
        "type": TYPE_TOMBSTONE,
        "formerType": TYPE_NOTE,
        "deleted": mastodon_time(&deleted),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        federation::note::{note_json, test_support::*},
        util::time::parse_time,
    };

    #[test]
    fn the_note_context_is_the_documented_schema_chain() {
        let document = render(Visibility::Public);
        let context = document["@context"].as_array().expect("an array context");
        assert_eq!(context.len(), 3, "vocabulary, schema, language map");
        assert_eq!(context[0], json!(AS_VOCABULARY), "the AS vocabulary");
        assert_eq!(
            context[1],
            json!(format!("{BASE}{LITEPUB_SCHEMA_PATH}")),
            "this instance's schema document"
        );
        assert_eq!(context[2], json!({"@language": "und"}), "the language map");
    }

    #[test]
    fn the_litepub_document_defines_the_terms_in_use() {
        let document = litepub_context_json();
        let context = document["@context"].as_object().expect("a term map");
        assert_eq!(
            context["conversation"],
            json!({"@id": "ostatus:conversation", "@type": "@id"}),
            "the ostatus conversation reference"
        );
        assert_eq!(context["EmojiReact"], "litepub:EmojiReact", "litepub terms");
        assert_eq!(context["capabilities"], "litepub:capabilities", "…");
        assert_eq!(context["directMessage"], "litepub:directMessage", "…");
        assert_eq!(context["quoteUrl"], "as:quoteUrl", "the quote pair");
        assert_eq!(context["quoteUri"], "fedibird:quoteUri", "…");
        assert_eq!(context["votersCount"], "toot:votersCount", "the poll count");
        assert_eq!(context["sensitive"], "as:sensitive", "the cw flag");
        assert_eq!(context["Hashtag"], "as:Hashtag", "the tag type");
        assert_eq!(context["Emoji"], "toot:Emoji", "the emoji type");
    }

    #[test]
    fn create_wraps_the_note_with_its_own_addressing() {
        let ctx = note_context(Visibility::Public);
        let note = note_json(&ctx).expect("the fixture renders");
        let status = base_status(Visibility::Public);
        let config = Config::for_tests();
        let activity = create_activity_json(&note, &status, &config);
        assert_eq!(
            json_keys(&activity),
            [
                "@context",
                "actor",
                "cc",
                "id",
                "object",
                "published",
                "to",
                "type"
            ],
            "the create key set"
        );
        assert_eq!(
            activity["id"],
            format!("{BASE}/activities/{STATUS_ID}"),
            "the activity id derives from the status id"
        );
        assert_eq!(activity["type"], "Create", "the activity type");
        assert_eq!(activity["actor"], AUTHOR_AP, "the object's author acts");
        assert_eq!(
            activity["to"], note["to"],
            "the addressing mirrors the object"
        );
        assert_eq!(activity["cc"], note["cc"], "…both lines");
        assert_eq!(activity["object"], note, "the note rides inside verbatim");
        assert_eq!(activity["published"], PUBLISHED, "the status's own time");
    }

    #[test]
    fn a_public_reblog_announces_pleroma_style() {
        let reblog = base_status(Visibility::Public);
        let config = Config::for_tests();
        let activity = announce_activity_json(&reblog, &author(), PARENT_URI, BOB_AP, &config)
            .expect("a public announce renders");
        assert_eq!(
            json_keys(&activity),
            [
                "@context",
                "actor",
                "cc",
                "id",
                "object",
                "published",
                "to",
                "type"
            ],
            "the announce key set"
        );
        assert_eq!(
            activity["id"], reblog.uri,
            "the reblog row's stored uri is the activity id"
        );
        assert_eq!(activity["type"], "Announce", "the activity type");
        assert_eq!(activity["actor"], AUTHOR_AP, "the booster acts");
        assert_eq!(activity["object"], PARENT_URI, "the boosted object");
        assert_eq!(
            activity["to"],
            json!([format!("{AUTHOR_AP}/followers"), BOB_AP]),
            "followers and the boosted author are addressed"
        );
        assert_eq!(activity["cc"], json!([AS_PUBLIC]), "the world is cc-ed");
    }

    #[test]
    fn a_private_reblog_announces_to_followers_only() {
        let reblog = base_status(Visibility::Private);
        let config = Config::for_tests();
        let activity = announce_activity_json(&reblog, &author(), PARENT_URI, BOB_AP, &config)
            .expect("a private announce renders");
        assert_eq!(
            activity["to"],
            json!([format!("{AUTHOR_AP}/followers")]),
            "only the followers"
        );
        assert_eq!(activity["cc"], json!([]), "cc stays empty");
    }

    #[test]
    fn a_local_reblog_is_never_announced() {
        let reblog = base_status(Visibility::Local);
        let config = Config::for_tests();
        assert!(
            announce_activity_json(&reblog, &author(), PARENT_URI, BOB_AP, &config).is_none(),
            "a local boost has no activity"
        );
    }

    #[test]
    fn a_deleted_status_serves_a_tombstone() {
        let mut deleted = base_status(Visibility::Public);
        deleted.deleted_at = Some(parse_time(DELETED).unwrap());
        assert_eq!(
            tombstone_json(&deleted),
            json!({
                "id": format!("{BASE}/objects/{STATUS_ID}"),
                "type": "Tombstone",
                "formerType": "Note",
                "deleted": DELETED,
            }),
            "the documented tombstone stub"
        );

        let live = base_status(Visibility::Public);
        assert_eq!(
            tombstone_json(&live)["deleted"],
            PUBLISHED,
            "a row that recorded no deletion falls back to its last write"
        );
    }
}
