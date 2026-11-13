//! The `ActivityPub` activity documents of this instance's relational
//! facts.
//!
//! Served at `{BASE_URL}/activities/{uuid}`, these are the `Like`,
//! `EmojiReact`, `Follow`, `Block`, `Undo`, `Accept` and `Reject` behind
//! the tables of `docs/ARCHITECTURE.md` decision 6 — favourites,
//! reactions, follows and blocks — other than statuses.
//!
//! These are the counterparts of Pleroma's
//! `lib/pleroma/web/activity_pub/views/*_view.ex` renderers, and pure
//! groundwork in exactly the shape [`note`](crate::federation::note) sets
//! for statuses: every input rides in as a plain borrow or one data-only
//! struct ([`EmojiReactContext`], [`AnsweredActivity`]), every answer is a
//! [`serde_json::Value`], and nothing here asks the database — the row
//! reads that fill these inputs live in [`crate::models`]. The
//! `/activities/{uuid}` route and the outbound federation tickets are the
//! callers this waits for, and they are the reason each builder is named
//! after its activity type. `Create` and `Announce` already live in
//! [`note`](crate::federation::note) and are not duplicated or re-exported
//! here.
//!
//! [`activity_id`] is the one minting rule: `{BASE_URL}/activities/{id}`,
//! the shape [`note`](crate::federation::note) stamps into a `Create` and
//! the shape [`favourite`](crate::models::favourite::favourite),
//! [`react`](crate::models::emoji_reaction::react),
//! [`follow`](crate::models::follow::follow) and
//! [`block`](crate::models::block::block) already mint into their rows'
//! `ap_id` columns, so the stored id and the document id can never
//! disagree.
//!
//! The addressing is this instance's deliberate uniform convention, so the
//! outbound tickets have one shape to deliver against: every document
//! starts with the same JSON-LD `@context` the note builder writes, and
//! carries both `to` and `cc`, always present. [`like_json`] and
//! [`emoji_react_json`] answer the way Pleroma's `LikeView` does — `to`
//! names the `ActivityStreams` public collection, `cc` is empty — while
//! the two-actor activities [`follow_json`], [`block_json`],
//! [`undo_json`], [`accept_json`] and [`reject_json`] name the other
//! actor's id in `to` and leave `cc` empty.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{config::Config, federation::note::json_ld_context, util::time::mastodon_time};

/// The `ActivityStreams` public addressing collection — the sole `to`
/// entry of a `Like` or `EmojiReact`, as Pleroma's `LikeView` writes it.
const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The activity type of a favourite.
const TYPE_LIKE: &str = "Like";
/// The activity type of an emoji reaction.
const TYPE_EMOJI_REACT: &str = "EmojiReact";
/// The activity type of a follow request.
const TYPE_FOLLOW: &str = "Follow";
/// The activity type of a block.
const TYPE_BLOCK: &str = "Block";
/// The activity type of a withdrawn activity.
const TYPE_UNDO: &str = "Undo";
/// The activity type of a granted follow request.
const TYPE_ACCEPT: &str = "Accept";
/// The activity type of a refused follow request.
const TYPE_REJECT: &str = "Reject";
/// The tag type of a custom emoji, as the note builder's `emoji_tag`
/// writes it.
const TYPE_EMOJI: &str = "Emoji";
/// The type of an emoji tag's `icon`.
const TYPE_IMAGE: &str = "Image";

/// The `ActivityPub` id this instance mints for the activity named `id`.
///
/// `{BASE_URL}/activities/{id}` (decision 6), the shape
/// [`note`](crate::federation::note) stamps into a `Create` and the shape
/// [`favourite`](crate::models::favourite::favourite),
/// [`react`](crate::models::emoji_reaction::react),
/// [`follow`](crate::models::follow::follow) and
/// [`block`](crate::models::block::block) already stamp into the rows'
/// `ap_id` columns, so the stored id and the document id can never
/// disagree.
#[must_use]
pub fn activity_id(config: &Config, id: Uuid) -> String {
    format!("{}/activities/{id}", config.base_url)
}

/// Everything [`emoji_react_json`] renders one reaction's document from.
///
/// The two ends of the pair, plus the stored emoji and its image URL —
/// the [`EmojiReaction`](crate::models::emoji_reaction::EmojiReaction)
/// row's `emoji` and `url` columns — read in by the caller so the builder
/// itself stays pure, the way
/// [`NoteContext`](crate::federation::note::NoteContext) is for notes.
#[derive(Debug)]
pub struct EmojiReactContext<'a> {
    /// The instance whose origin and JSON-LD schema mint the document.
    pub config: &'a Config,
    /// The reaction row's `UUIDv7`, which also names the activity.
    pub id: Uuid,
    /// The reacting account's actor id.
    pub actor_ap_id: &'a str,
    /// The reacted-to status' object id.
    pub object_uri: &'a str,
    /// The emoji as stored: one unicode grapheme, or a custom emoji
    /// written `:shortcode:`.
    pub emoji: &'a str,
    /// The custom emoji's image URL — `Some` for a custom emoji only, and
    /// what makes [`emoji_react_json`] carry an `Emoji` tag; `None` (a
    /// unicode reaction) writes none.
    pub emoji_url: Option<&'a str>,
    /// The reaction's `inserted_at`, written as the activity's
    /// `published`.
    pub published: &'a DateTime<Utc>,
}

/// The inputs [`undo_json`], [`accept_json`] and [`reject_json`] share.
///
/// One actor of a pair answering the other's activity — a `Follow` with
/// an `Accept` or a `Reject`, or any sent activity with an `Undo` — and
/// carrying it inside as an embedded object rather than naming it by id.
#[derive(Debug)]
pub struct AnsweredActivity<'a> {
    /// The instance whose origin and JSON-LD schema mint the document.
    pub config: &'a Config,
    /// The `UUIDv7` naming this answer, which also names it (decision 6).
    pub id: Uuid,
    /// The acting actor's id: the followee who answers, or the actor who
    /// withdraws.
    pub actor_ap_id: &'a str,
    /// The other actor of the pair — the one `to` names.
    pub other_actor_ap_id: &'a str,
    /// The activity being answered or withdrawn, embedded as the object.
    pub answered: &'a Value,
    /// The answer row's creation moment, written as `published`.
    pub published: &'a DateTime<Utc>,
}

/// The skeleton every document in this module renders through: the public
/// builders fill one of these in and [`document_json`] turns it into the
/// uniform shape the module docs promise, so the key set and the
/// `@context`/`to`/`cc` convention hold by construction.
#[derive(Debug)]
struct ActivityDocument<'a> {
    /// The instance whose origin and JSON-LD schema mint the document.
    config: &'a Config,
    /// The `UUIDv7` naming the activity.
    id: Uuid,
    /// The activity type, one of this module's `TYPE_*` constants.
    type_name: &'static str,
    /// The acting actor's id.
    actor_ap_id: &'a str,
    /// The object: an id string, or an embedded activity document.
    object: Value,
    /// The primary addressing line; `cc` is always empty.
    to: Vec<Value>,
    /// The row's creation moment, written as `published`.
    published: &'a DateTime<Utc>,
    /// The type's own extra fields — `content` and `tag` — in render
    /// order after the shared ones.
    extra: Vec<(&'static str, Value)>,
}

/// `ctx` as the uniform activity document every builder here returns.
///
/// The JSON-LD `@context` first, then `id`, `type`, `actor`, `object`,
/// `published`, `to` and the always empty `cc`, plus
/// [`ActivityDocument`]'s extras.
fn document_json(ctx: ActivityDocument<'_>) -> Value {
    let mut fields = vec![
        ("@context", json_ld_context(ctx.config)),
        ("id", json!(activity_id(ctx.config, ctx.id))),
        ("type", json!(ctx.type_name)),
        ("actor", json!(ctx.actor_ap_id)),
        ("object", ctx.object),
        ("published", json!(mastodon_time(ctx.published))),
        ("to", Value::Array(ctx.to)),
        ("cc", json!([])),
    ];
    fields.extend(ctx.extra);
    Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

/// The reaction emoji as its `Emoji` tag, in the shape the note builder's
/// `emoji_tag` writes: the image URL as both `id` and `icon.url`, the
/// stored spelling (the `:shortcode:`) as the `name`.
fn react_emoji_tag(emoji: &str, url: &str) -> Value {
    json!({
        "type": TYPE_EMOJI,
        "id": url,
        "name": emoji,
        "icon": { "type": TYPE_IMAGE, "url": url },
    })
}

/// The `Like` behind the favourite row `id`: `actor_ap_id` favourited the
/// status whose object id is `object_uri`.
///
/// This instance's rendering of Pleroma's `LikeView`, addressed to the
/// public collection with an empty `cc`. The row's stored `ap_id`
/// (decision 6) is exactly [`activity_id`] of `config` and `id`, and
/// `published` is its `inserted_at`.
#[must_use]
pub fn like_json(
    config: &Config,
    id: Uuid,
    actor_ap_id: &str,
    object_uri: &str,
    published: &DateTime<Utc>,
) -> Value {
    document_json(ActivityDocument {
        config,
        id,
        type_name: TYPE_LIKE,
        actor_ap_id,
        object: json!(object_uri),
        to: vec![json!(AS_PUBLIC)],
        published,
        extra: Vec::new(),
    })
}

/// The `EmojiReact` behind the reaction row in `ctx`: `actor_ap_id`
/// reacted to the status at `object_uri` with the stored emoji.
///
/// Pleroma's answer to a `Like` that names its emoji in `content`, on the
/// public addressing [`like_json`] uses. A custom emoji (a `url` in
/// `ctx`) also carries the emoji as a `tag`, in the shape the note
/// builder writes; a unicode reaction carries none.
#[must_use]
pub fn emoji_react_json(ctx: &EmojiReactContext<'_>) -> Value {
    let mut extra = vec![("content", json!(ctx.emoji))];
    if let Some(url) = ctx.emoji_url {
        extra.push(("tag", Value::Array(vec![react_emoji_tag(ctx.emoji, url)])));
    }
    document_json(ActivityDocument {
        config: ctx.config,
        id: ctx.id,
        type_name: TYPE_EMOJI_REACT,
        actor_ap_id: ctx.actor_ap_id,
        object: json!(ctx.object_uri),
        to: vec![json!(AS_PUBLIC)],
        published: ctx.published,
        extra,
    })
}

/// The `Follow` behind the follow row `id`: `follower_ap_id` asks to
/// follow `target_ap_id`, who is the activity's object and the only
/// address `to` names — Pleroma's `FollowView`.
///
/// The row's stored `ap_id` (decision 6) is [`activity_id`] of `config`
/// and `id`, and `published` is the follow's creation moment.
#[must_use]
pub fn follow_json(
    config: &Config,
    id: Uuid,
    follower_ap_id: &str,
    target_ap_id: &str,
    published: &DateTime<Utc>,
) -> Value {
    document_json(ActivityDocument {
        config,
        id,
        type_name: TYPE_FOLLOW,
        actor_ap_id: follower_ap_id,
        object: json!(target_ap_id),
        to: vec![json!(target_ap_id)],
        published,
        extra: Vec::new(),
    })
}

/// The `Block` behind the block row `id`: `blocker_ap_id` blocks
/// `target_ap_id`.
///
/// Pleroma's `BlockView`: the blocked actor is the activity's object and
/// the only address `to` names, so the blocked peer learns of the block
/// and nobody else does.
#[must_use]
pub fn block_json(
    config: &Config,
    id: Uuid,
    blocker_ap_id: &str,
    target_ap_id: &str,
    published: &DateTime<Utc>,
) -> Value {
    document_json(ActivityDocument {
        config,
        id,
        type_name: TYPE_BLOCK,
        actor_ap_id: blocker_ap_id,
        object: json!(target_ap_id),
        to: vec![json!(target_ap_id)],
        published,
        extra: Vec::new(),
    })
}

/// The shared shape of the two-actor activities whose object is a full
/// embedded document: `type_name` answering `answer`'s actor on behalf of
/// the pair's other member.
fn answered_json(type_name: &'static str, answer: &AnsweredActivity<'_>) -> Value {
    document_json(ActivityDocument {
        config: answer.config,
        id: answer.id,
        type_name,
        actor_ap_id: answer.actor_ap_id,
        object: answer.answered.clone(),
        to: vec![json!(answer.other_actor_ap_id)],
        published: answer.published,
        extra: Vec::new(),
    })
}

/// The `Undo` that withdraws `answer`'s embedded activity.
///
/// `actor_ap_id` takes back what they once sent to `other_actor_ap_id`:
/// unfavouriting, unreacting, unfollowing, unblocking, or withdrawing a
/// follow request that is still pending. The withdrawn activity rides
/// inside as an object, not a bare id, so the peer matches it without a
/// fetch.
#[must_use]
pub fn undo_json(answer: &AnsweredActivity<'_>) -> Value {
    answered_json(TYPE_UNDO, answer)
}

/// The `Accept` that grants `answer`'s embedded `Follow`.
///
/// `actor_ap_id` — the followee — admits `other_actor_ap_id`, the
/// follower, who is the only address `to` names and must receive the
/// grant to complete the handshake.
#[must_use]
pub fn accept_json(answer: &AnsweredActivity<'_>) -> Value {
    answered_json(TYPE_ACCEPT, answer)
}

/// The `Reject` that refuses `answer`'s embedded `Follow`.
///
/// `actor_ap_id` — the followee — turns down `other_actor_ap_id`, the
/// follower, who is the only address `to` names and must receive the
/// refusal to close the handshake.
#[must_use]
pub fn reject_json(answer: &AnsweredActivity<'_>) -> Value {
    answered_json(TYPE_REJECT, answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::time::parse_time;

    /// The instance origin of [`Config::for_tests`].
    const BASE: &str = "https://test.example";
    /// The schema URL every document's `@context` names.
    const SCHEMA_URL: &str = "https://test.example/schemas/litepub-0.1.jsonld";
    /// The activity fixture's id, spelled out for equality.
    const ACTIVITY_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// A second id: the `Follow` that `Accept`, `Reject` and `Undo`
    /// embed, so wrapper and wrapped never share an address.
    const FOLLOW_ID: &str = "01934569-2222-7000-8000-abcdef012345";
    /// The acting account's actor id, on this instance.
    const ALICE_AP: &str = "https://test.example/users/alice";
    /// The other account's actor id, on a peer instance.
    const BOB_AP: &str = "https://remote.example/users/bob";
    /// The liked/reacted-to status' object id.
    const STATUS_URI: &str = "https://test.example/objects/01934568-1111-7000-8000-abcdef012345";
    /// The custom emoji's image URL, as the reaction row stores it.
    const EMOJI_URL: &str = "https://test.example/emoji/railroad_track.png";
    /// Every fixture's creation moment, in the wire format.
    const PUBLISHED: &str = "2024-01-31T12:34:56.789Z";

    /// The keys every plain activity document carries; the two-actor
    /// builders emit exactly these, and [`EmojiReactContext`]'s two add
    /// `content` (custom reactions also `tag`).
    const ACTIVITY_KEYS: [&str; 8] = [
        "@context",
        "actor",
        "cc",
        "id",
        "object",
        "published",
        "to",
        "type",
    ];

    /// Every field of a rendered document, sorted — the exact key-set
    /// assertion the note tests pair with goldens.
    fn json_keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("a document is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    /// The test instance's config.
    fn config() -> Config {
        Config::for_tests()
    }

    /// The activity fixture's uuid.
    fn activity_uuid() -> Uuid {
        Uuid::parse_str(ACTIVITY_ID).unwrap()
    }

    /// The embedded `Follow`'s uuid.
    fn follow_uuid() -> Uuid {
        Uuid::parse_str(FOLLOW_ID).unwrap()
    }

    /// The fixtures' shared creation moment.
    fn published() -> DateTime<Utc> {
        parse_time(PUBLISHED).unwrap()
    }

    /// The `@context` every document of the test instance carries.
    fn expected_context() -> Value {
        json!([
            "https://www.w3.org/ns/activitystreams",
            SCHEMA_URL,
            { "@language": "und" },
        ])
    }

    /// The `Follow` of `BOB_AP` asking to follow `ALICE_AP`, the document
    /// `Accept`, `Reject` and `Undo` embed.
    fn follow_of_bob() -> Value {
        follow_json(&config(), follow_uuid(), BOB_AP, ALICE_AP, &published())
    }

    #[test]
    fn activity_id_builds_the_documented_url() {
        assert_eq!(
            activity_id(&config(), activity_uuid()),
            format!("{BASE}/activities/{ACTIVITY_ID}"),
            "the shape decision 6 mints and the ap_id columns store"
        );
    }

    #[test]
    fn a_favourite_renders_pleromas_like_view() {
        let document = like_json(
            &config(),
            activity_uuid(),
            ALICE_AP,
            STATUS_URI,
            &published(),
        );
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Like",
                "actor": ALICE_AP,
                "object": STATUS_URI,
                "published": PUBLISHED,
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                "cc": [],
            }),
            "the whole Like, field for field"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn a_unicode_reaction_renders_content_without_a_tag() {
        let config = config();
        let published = published();
        let ctx = EmojiReactContext {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            emoji: "🎉",
            emoji_url: None,
            published: &published,
        };
        let document = emoji_react_json(&ctx);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "EmojiReact",
                "actor": ALICE_AP,
                "object": STATUS_URI,
                "content": "🎉",
                "published": PUBLISHED,
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                "cc": [],
            }),
            "a unicode reaction is a Like that says which emoji"
        );
        let mut keys = ACTIVITY_KEYS.to_vec();
        keys.push("content");
        keys.sort_unstable();
        assert_eq!(json_keys(&document), keys, "content, and no tag");
    }

    #[test]
    fn a_custom_reaction_carries_the_note_shapes_emoji_tag() {
        let config = config();
        let published = published();
        let ctx = EmojiReactContext {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            emoji: ":railroad_track:",
            emoji_url: Some(EMOJI_URL),
            published: &published,
        };
        let document = emoji_react_json(&ctx);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "EmojiReact",
                "actor": ALICE_AP,
                "object": STATUS_URI,
                "content": ":railroad_track:",
                "published": PUBLISHED,
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                "cc": [],
                "tag": [{
                    "type": "Emoji",
                    "id": EMOJI_URL,
                    "name": ":railroad_track:",
                    "icon": { "type": "Image", "url": EMOJI_URL },
                }],
            }),
            "the shortcode as content and the image as a tag"
        );
        let mut keys = ACTIVITY_KEYS.to_vec();
        keys.extend(["content", "tag"]);
        keys.sort_unstable();
        assert_eq!(json_keys(&document), keys, "content and tag, both");
    }

    #[test]
    fn a_follow_addresses_the_followed_actor_alone() {
        let document = follow_json(&config(), activity_uuid(), BOB_AP, ALICE_AP, &published());
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Follow",
                "actor": BOB_AP,
                "object": ALICE_AP,
                "published": PUBLISHED,
                "to": [ALICE_AP],
                "cc": [],
            }),
            "the whole Follow: object and to are the followed actor"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn a_block_addresses_the_blocked_actor_alone() {
        let document = block_json(&config(), activity_uuid(), ALICE_AP, BOB_AP, &published());
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Block",
                "actor": ALICE_AP,
                "object": BOB_AP,
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [],
            }),
            "the whole Block: the blocked peer is the only addressee"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn an_undo_embeds_the_activity_it_withdraws() {
        let followed = follow_of_bob();
        let config = config();
        let published = published();
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: BOB_AP,
            other_actor_ap_id: ALICE_AP,
            answered: &followed,
            published: &published,
        };
        let document = undo_json(&answer);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Undo",
                "actor": BOB_AP,
                "object": followed,
                "published": PUBLISHED,
                "to": [ALICE_AP],
                "cc": [],
            }),
            "the withdrawn Follow rides inside as an object"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn an_accept_answers_the_followers_request() {
        let followed = follow_of_bob();
        let config = config();
        let published = published();
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            other_actor_ap_id: BOB_AP,
            answered: &followed,
            published: &published,
        };
        let document = accept_json(&answer);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Accept",
                "actor": ALICE_AP,
                "object": followed,
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [],
            }),
            "the followee grants, addressing the follower"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn a_reject_refuses_the_followers_request() {
        let followed = follow_of_bob();
        let config = config();
        let published = published();
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            other_actor_ap_id: BOB_AP,
            answered: &followed,
            published: &published,
        };
        let document = reject_json(&answer);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Reject",
                "actor": ALICE_AP,
                "object": followed,
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [],
            }),
            "the followee refuses, and only the follower learns"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn every_document_starts_with_the_shared_note_context() {
        let config = config();
        let published = published();
        let followed = follow_of_bob();
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            other_actor_ap_id: BOB_AP,
            answered: &followed,
            published: &published,
        };
        let react_ctx = EmojiReactContext {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            emoji: "🎉",
            emoji_url: None,
            published: &published,
        };
        let documents = [
            (
                "Like",
                like_json(&config, activity_uuid(), ALICE_AP, STATUS_URI, &published),
            ),
            ("EmojiReact", emoji_react_json(&react_ctx)),
            (
                "Follow",
                follow_json(&config, activity_uuid(), BOB_AP, ALICE_AP, &published),
            ),
            (
                "Block",
                block_json(&config, activity_uuid(), ALICE_AP, BOB_AP, &published),
            ),
            ("Undo", undo_json(&answer)),
            ("Accept", accept_json(&answer)),
            ("Reject", reject_json(&answer)),
        ];
        for (name, document) in documents {
            assert_eq!(
                document["@context"],
                json_ld_context(&config),
                "the {name} document shares the note module's context"
            );
            let first_key = document
                .as_object()
                .expect("a document is a JSON object")
                .keys()
                .next()
                .expect("and carries at least the context");
            assert_eq!(first_key, "@context", "the {name} document starts with it");
            assert!(
                document.get("to").is_some() && document.get("cc").is_some(),
                "the {name} document carries both addressing keys"
            );
        }
    }
}
