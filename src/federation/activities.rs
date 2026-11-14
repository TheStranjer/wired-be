//! The `ActivityPub` activity documents of this instance's relational
//! facts.
//!
//! Served at `{BASE_URL}/activities/{uuid}`, these are the `Like`,
//! `EmojiReact`, `Announce`, `Follow`, `Block`, `Undo`, `Accept` and
//! `Reject` behind the tables of `docs/ARCHITECTURE.md` decision 6 —
//! favourites, reactions, boosts, follows and blocks — other than notes.
//!
//! These are the counterparts of Pleroma's
//! `lib/pleroma/web/activity_pub/views/*_view.ex` renderers, and pure
//! groundwork in exactly the shape [`note`](crate::federation::note) sets
//! for statuses: every input rides in as a plain borrow or one data-only
//! struct ([`EmojiReactContext`], [`AnsweredActivity`]), every answer is a
//! [`serde_json::Value`], and nothing here asks the database — the row
//! reads that fill these inputs live in [`crate::models`]. The
//! `/activities/{uuid}` route and the outbound federation tickets are the
//! callers, and they are the reason each builder is named after its activity
//! type. `Create` lives in [`note`](crate::federation::note) and is not
//! duplicated here; a reblog row's `Announce` has a delivery rendering here
//! ([`announce_json`], addressed by the outbound hook) and its own served
//! rendering in [`note`](crate::federation::note), which the object and
//! outbox routes keep.
//!
//! [`activity_id`] is the one minting rule: `{BASE_URL}/activities/{id}`,
//! the shape [`note`](crate::federation::note) stamps into a `Create` and
//! the shape [`favourite`](crate::models::favourite::favourite),
//! [`react`](crate::models::emoji_reaction::react),
//! [`follow`](crate::models::follow::follow) and
//! [`block`](crate::models::block::block) already mint into their rows'
//! `ap_id` columns. The outbound hooks hand each builder the row's stored
//! id string — the Like's id is the favourite row's `ap_id`, the Announce's
//! the reblog row's `uri` — so a peer's later lookup of what was delivered
//! finds exactly what it holds.
//!
//! The addressing is this instance's deliberate uniform convention, so the
//! outbound tickets have one shape to deliver against: every document
//! starts with the same JSON-LD `@context` the note builder writes, and
//! carries both `to` and `cc`, always present. The activity builders that
//! speak of one object — [`like_json`], [`emoji_react_json`] and
//! [`announce_json`] — render the `to` and `cc` their caller computed from
//! the interacted-with status (the outbound hooks in
//! [`crate::services::federation_out`] derive them; the C2S serving route
//! keeps this module's public-address convention), while the two-actor
//! activities [`follow_json`] and [`block_json`] name the other actor's id
//! in `to` and leave `cc` empty. [`undo_json`], [`accept_json`] and
//! [`reject_json`] render the addressing their [`AnsweredActivity`]
//! carries: the two follow answers name the follower, and the withdrawal
//! of an interaction repeats the addressing of the activity it embeds —
//! exactly who heard the fact hears its taking-back.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{config::Config, federation::note::json_ld_context, util::time::mastodon_time};

/// The `ActivityStreams` public addressing collection: the `cc` entry of a
/// publicly-addressed interaction, and the `to` the C2S route keeps on the
/// relation documents it serves.
pub const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The activity type of a favourite.
const TYPE_LIKE: &str = "Like";
/// The activity type of a boost.
const TYPE_ANNOUNCE: &str = "Announce";
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
/// `ap_id` columns; the outbound hooks deliver the row's stored string
/// itself, so the document carries the id the table keeps.
#[must_use]
pub fn activity_id(config: &Config, id: Uuid) -> String {
    format!("{}/activities/{id}", config.base_url)
}

/// The inputs [`like_json`] and [`announce_json`] render from.
///
/// One object-addressed activity: the acting account, the object the
/// activity names, the id the row stores as the activity's own, the
/// addressing the caller computed from the acted-on status, and the row's
/// moment — read in as plain borrows so the builder stays pure, the way
/// [`NoteContext`](crate::federation::note::NoteContext) is for notes.
#[derive(Debug)]
pub struct ObjectActivityContext<'a> {
    /// The instance whose origin and JSON-LD schema frame the document.
    pub config: &'a Config,
    /// The document's `id`: the row's stored `ap_id` (decision 6), which
    /// for a reblog row is its `uri` — the id peers know the activity by.
    pub id: &'a str,
    /// The acting account's actor id.
    pub actor_ap_id: &'a str,
    /// The object the activity names: the liked/boosted status' `uri`.
    pub object_uri: &'a str,
    /// The primary addressing line, in order.
    pub to: &'a [&'a str],
    /// The copy line, in order.
    pub cc: &'a [&'a str],
    /// The row's creation moment, written as `published`.
    pub published: &'a DateTime<Utc>,
}

/// Everything [`emoji_react_json`] renders one reaction's document from.
///
/// The two ends of the pair, the addressing [`like_json`] is given, plus
/// the stored emoji and its image URL — the
/// [`EmojiReaction`](crate::models::emoji_reaction::EmojiReaction) row's
/// `emoji` and `url` columns — read in by the caller so the builder itself
/// stays pure, the way [`NoteContext`](crate::federation::note::NoteContext)
/// is for notes.
#[derive(Debug)]
pub struct EmojiReactContext<'a> {
    /// The instance whose origin and JSON-LD schema frame the document.
    pub config: &'a Config,
    /// The document's `id`: the reaction row's stored `ap_id` (decision 6).
    pub id: &'a str,
    /// The reacting account's actor id.
    pub actor_ap_id: &'a str,
    /// The reacted-to status' object id.
    pub object_uri: &'a str,
    /// The primary addressing line, in order.
    pub to: &'a [&'a str],
    /// The copy line, in order.
    pub cc: &'a [&'a str],
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
/// One actor answering an activity — a `Follow` with an `Accept` or a
/// `Reject`, or any sent activity with an `Undo` — carrying it inside as an
/// embedded object rather than naming it by id, and repeating the
/// addressing the answer must travel under: the two follow answers name
/// the other actor of the pair, and the withdrawal of a delivered activity
/// repeats that activity's own `to` and `cc`, so exactly the inboxes that
/// heard the fact hear its taking-back.
#[derive(Debug)]
pub struct AnsweredActivity<'a> {
    /// The instance whose origin and JSON-LD schema mint the document.
    pub config: &'a Config,
    /// The `UUIDv7` naming this answer, which also names it (decision 6).
    pub id: Uuid,
    /// The acting actor's id: the followee who answers, or the actor who
    /// withdraws.
    pub actor_ap_id: &'a str,
    /// The answer's primary addressing line, in order.
    pub to: &'a [&'a str],
    /// The answer's copy line, in order.
    pub cc: &'a [&'a str],
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
    /// The document's own id, spelled out: every builder either mints it
    /// with [`activity_id`] or takes the caller's stored string.
    id: String,
    /// The activity type, one of this module's `TYPE_*` constants.
    type_name: &'static str,
    /// The acting actor's id.
    actor_ap_id: &'a str,
    /// The object: an id string, or an embedded activity document.
    object: Value,
    /// The primary addressing line.
    to: Vec<Value>,
    /// The copy line.
    cc: Vec<Value>,
    /// The row's creation moment, written as `published`.
    published: &'a DateTime<Utc>,
    /// The type's own extra fields — `content` and `tag` — in render
    /// order after the shared ones.
    extra: Vec<(&'static str, Value)>,
}

/// `ctx` as the uniform activity document every builder here returns.
///
/// The JSON-LD `@context` first, then `id`, `type`, `actor`, `object`,
/// `published`, `to` and `cc`, plus [`ActivityDocument`]'s extras.
fn document_json(ctx: ActivityDocument<'_>) -> Value {
    let mut fields = vec![
        ("@context", json_ld_context(ctx.config)),
        ("id", json!(ctx.id)),
        ("type", json!(ctx.type_name)),
        ("actor", json!(ctx.actor_ap_id)),
        ("object", ctx.object),
        ("published", json!(mastodon_time(ctx.published))),
        ("to", Value::Array(ctx.to)),
        ("cc", Value::Array(ctx.cc)),
    ];
    fields.extend(ctx.extra);
    Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

/// One addressing line as document entries.
fn address_values(line: &[&str]) -> Vec<Value> {
    line.iter().map(|address| json!(*address)).collect()
}

/// The shared shape of the object-addressed activities: `type_name` naming
/// `ctx`'s object id under the caller's addressing and stored id.
fn object_json(type_name: &'static str, ctx: &ObjectActivityContext<'_>) -> Value {
    document_json(ActivityDocument {
        config: ctx.config,
        id: ctx.id.to_owned(),
        type_name,
        actor_ap_id: ctx.actor_ap_id,
        object: json!(ctx.object_uri),
        to: address_values(ctx.to),
        cc: address_values(ctx.cc),
        published: ctx.published,
        extra: Vec::new(),
    })
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

/// The `Like` in `ctx`: `actor_ap_id` favourited the status at
/// `object_uri`.
///
/// It travels under the caller's addressing: the outbound hook sends the
/// author in `to` and the actor's followers (plus the public address, for
/// a public status) in `cc`; the C2S route keeps this instance's
/// public-address convention. `published` is the favourite row's
/// `inserted_at`, `id` its stored `ap_id` (decision 6) — the id a matching
/// `Undo` embeds.
#[must_use]
pub fn like_json(ctx: &ObjectActivityContext<'_>) -> Value {
    object_json(TYPE_LIKE, ctx)
}

/// The `Announce` in `ctx`: `actor_ap_id` boosted the status whose object
/// id is `object_uri`, under the addressing the caller computed from the
/// boost's own visibility.
///
/// Decision 6: a reblog row *is* the activity, so `ctx.id` is the row's
/// `uri` — the id peers know the boost by, which their `Undo` of a
/// delivered one and a later fetch of it both name back. `published` is
/// the row's `inserted_at`.
#[must_use]
pub fn announce_json(ctx: &ObjectActivityContext<'_>) -> Value {
    object_json(TYPE_ANNOUNCE, ctx)
}

/// The `EmojiReact` behind the reaction row in `ctx`: `actor_ap_id`
/// reacted to the status at `object_uri` with the stored emoji, under the
/// addressing [`like_json`] is given.
///
/// Pleroma's answer to a `Like` that names its emoji in `content`. A
/// custom emoji (a `url` in `ctx`) also carries the emoji as a `tag`, in
/// the shape the note builder writes; a unicode reaction carries none.
#[must_use]
pub fn emoji_react_json(ctx: &EmojiReactContext<'_>) -> Value {
    let mut extra = vec![("content", json!(ctx.emoji))];
    if let Some(url) = ctx.emoji_url {
        extra.push(("tag", Value::Array(vec![react_emoji_tag(ctx.emoji, url)])));
    }
    document_json(ActivityDocument {
        config: ctx.config,
        id: ctx.id.to_owned(),
        type_name: TYPE_EMOJI_REACT,
        actor_ap_id: ctx.actor_ap_id,
        object: json!(ctx.object_uri),
        to: address_values(ctx.to),
        cc: address_values(ctx.cc),
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
        id: activity_id(config, id),
        type_name: TYPE_FOLLOW,
        actor_ap_id: follower_ap_id,
        object: json!(target_ap_id),
        to: vec![json!(target_ap_id)],
        cc: Vec::new(),
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
        id: activity_id(config, id),
        type_name: TYPE_BLOCK,
        actor_ap_id: blocker_ap_id,
        object: json!(target_ap_id),
        to: vec![json!(target_ap_id)],
        cc: Vec::new(),
        published,
        extra: Vec::new(),
    })
}

/// The shared shape of the activities whose object is a full embedded
/// document: `type_name` answering under `answer`'s own addressing.
fn answered_json(type_name: &'static str, answer: &AnsweredActivity<'_>) -> Value {
    document_json(ActivityDocument {
        config: answer.config,
        id: activity_id(answer.config, answer.id),
        type_name,
        actor_ap_id: answer.actor_ap_id,
        object: answer.answered.clone(),
        to: address_values(answer.to),
        cc: address_values(answer.cc),
        published: answer.published,
        extra: Vec::new(),
    })
}

/// The `Undo` that withdraws `answer`'s embedded activity.
///
/// `actor_ap_id` takes back what they once sent: unfavouriting,
/// unreacting, unboosting, unfollowing, unblocking, or withdrawing a
/// follow request that is still pending. The withdrawn activity rides
/// inside as an object, not a bare id, so the peer matches it without a
/// fetch, and the answer travels under the addressing `answer` carries —
/// for a withdrawn interaction, exactly the `to` and `cc` the activity
/// itself went out under.
#[must_use]
pub fn undo_json(answer: &AnsweredActivity<'_>) -> Value {
    answered_json(TYPE_UNDO, answer)
}

/// The `Accept` that grants `answer`'s embedded `Follow`.
///
/// `actor_ap_id` — the followee — admits the follower, who is the only
/// address `to` names and must receive the grant to complete the
/// handshake.
#[must_use]
pub fn accept_json(answer: &AnsweredActivity<'_>) -> Value {
    answered_json(TYPE_ACCEPT, answer)
}

/// The `Reject` that refuses `answer`'s embedded `Follow`.
///
/// `actor_ap_id` — the followee — turns down the follower, who is the
/// only address `to` names and must receive the refusal to close the
/// handshake.
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
    /// The acting account's followers collection, the interaction `cc`
    /// entry the outbound hooks compute.
    const FOLLOWERS: &str = "https://test.example/users/alice/followers";
    /// The liked/reacted-to/boosted status' object id.
    const STATUS_URI: &str = "https://test.example/objects/01934568-1111-7000-8000-abcdef012345";
    /// The custom emoji's image URL, as the reaction row stores it.
    const EMOJI_URL: &str = "https://test.example/emoji/railroad_track.png";
    /// Every fixture's creation moment, in the wire format.
    const PUBLISHED: &str = "2024-01-31T12:34:56.789Z";

    /// The public `to` line the C2S route keeps on the relation documents
    /// it serves.
    const PUBLIC_TO: [&str; 1] = [AS_PUBLIC];
    /// The empty copy line.
    const NO_CC: [&str; 0] = [];
    /// The delivered interaction's `to`: the acted-on status' author.
    const AUTHOR_TO: [&str; 1] = [BOB_AP];
    /// A public interaction's `cc`: the actor's followers, and the world.
    const PUBLIC_CC: [&str; 2] = [FOLLOWERS, AS_PUBLIC];
    /// A reaction's `cc`: the actor's followers, and nobody else.
    const FOLLOWERS_CC: [&str; 1] = [FOLLOWERS];

    /// The keys every activity document carries; the two-actor builders
    /// and the object-addressed ones all emit exactly these, and
    /// [`EmojiReactContext`]'s builder adds `content` (custom reactions
    /// also `tag`).
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

    /// The stored id every fixture document carries: [`activity_id`] of
    /// the fixture uuid, the shape the relation rows' `ap_id` columns and
    /// a reblog row's `uri` hold (decision 6).
    fn stored_id() -> String {
        activity_id(&config(), activity_uuid())
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
    fn a_favourite_renders_the_like_its_maker_addressed() {
        let config = config();
        let published = published();
        let id = stored_id();
        let document = like_json(&ObjectActivityContext {
            config: &config,
            id: &id,
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            to: &AUTHOR_TO,
            cc: &PUBLIC_CC,
            published: &published,
        });
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": id,
                "type": "Like",
                "actor": ALICE_AP,
                "object": STATUS_URI,
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [FOLLOWERS, AS_PUBLIC],
            }),
            "the whole Like, field for field, under the caller's addressing"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn a_boost_renders_the_announce_under_its_stored_uri() {
        let config = config();
        let published = published();
        let id = stored_id();
        let document = announce_json(&ObjectActivityContext {
            config: &config,
            id: &id,
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            to: &PUBLIC_TO,
            cc: &PUBLIC_CC,
            published: &published,
        });
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": id,
                "type": "Announce",
                "actor": ALICE_AP,
                "object": STATUS_URI,
                "published": PUBLISHED,
                "to": [AS_PUBLIC],
                "cc": [FOLLOWERS, AS_PUBLIC],
            }),
            "the whole Announce: the id is the reblog row's own uri string"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn a_unicode_reaction_renders_content_without_a_tag() {
        let config = config();
        let published = published();
        let id = stored_id();
        let ctx = EmojiReactContext {
            config: &config,
            id: &id,
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            to: &AUTHOR_TO,
            cc: &FOLLOWERS_CC,
            emoji: "🎉",
            emoji_url: None,
            published: &published,
        };
        let document = emoji_react_json(&ctx);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": id,
                "type": "EmojiReact",
                "actor": ALICE_AP,
                "object": STATUS_URI,
                "content": "🎉",
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [FOLLOWERS],
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
        let id = stored_id();
        let ctx = EmojiReactContext {
            config: &config,
            id: &id,
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            to: &AUTHOR_TO,
            cc: &FOLLOWERS_CC,
            emoji: ":railroad_track:",
            emoji_url: Some(EMOJI_URL),
            published: &published,
        };
        let document = emoji_react_json(&ctx);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": id,
                "type": "EmojiReact",
                "actor": ALICE_AP,
                "object": STATUS_URI,
                "content": ":railroad_track:",
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [FOLLOWERS],
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
    fn an_undo_embeds_the_activity_it_withdraws_under_its_addressing() {
        let followed = follow_of_bob();
        let config = config();
        let published = published();
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: BOB_AP,
            to: &AUTHOR_TO,
            cc: &PUBLIC_CC,
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
                "to": [BOB_AP],
                "cc": [FOLLOWERS, AS_PUBLIC],
            }),
            "the withdrawn Follow rides inside as an object, and the \
             Undo travels under the addressing the answerer names — the \
             withdrawal of a delivered interaction repeats that \
             interaction's own lines"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn an_accept_answers_the_followers_request() {
        let followed = follow_of_bob();
        let config = config();
        let published = published();
        let to = [BOB_AP];
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            to: &to,
            cc: &NO_CC,
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
        let to = [BOB_AP];
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            to: &to,
            cc: &NO_CC,
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
        let id = stored_id();
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            to: &AUTHOR_TO,
            cc: &NO_CC,
            answered: &followed,
            published: &published,
        };
        let react_ctx = EmojiReactContext {
            config: &config,
            id: &id,
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            to: &AUTHOR_TO,
            cc: &FOLLOWERS_CC,
            emoji: "🎉",
            emoji_url: None,
            published: &published,
        };
        let object_ctx = ObjectActivityContext {
            config: &config,
            id: &id,
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            to: &AUTHOR_TO,
            cc: &PUBLIC_CC,
            published: &published,
        };
        let documents = [
            ("Like", like_json(&object_ctx)),
            ("Announce", announce_json(&object_ctx)),
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
