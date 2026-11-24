//! Fixtures the outbound activity builders' unit tests share.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::util::time::parse_time;

/// The instance origin of [`Config::for_tests`].
pub(super) const BASE: &str = "https://test.example";
/// The schema URL every document's `@context` names.
pub(super) const SCHEMA_URL: &str = "https://test.example/schemas/litepub-0.1.jsonld";
/// The activity fixture's id, spelled out for equality.
pub(super) const ACTIVITY_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// A second id: the `Follow` that `Accept`, `Reject` and `Undo`
/// embed, so wrapper and wrapped never share an address.
pub(super) const FOLLOW_ID: &str = "01934569-2222-7000-8000-abcdef012345";
/// The acting account's actor id, on this instance.
pub(super) const ALICE_AP: &str = "https://test.example/users/alice";
/// The other account's actor id, on a peer instance.
pub(super) const BOB_AP: &str = "https://remote.example/users/bob";
/// The acting account's followers collection, the interaction `cc`
/// entry the outbound hooks compute.
pub(super) const FOLLOWERS: &str = "https://test.example/users/alice/followers";
/// The liked/reacted-to/boosted status' object id.
pub(super) const STATUS_URI: &str =
    "https://test.example/objects/01934568-1111-7000-8000-abcdef012345";
/// The custom emoji's image URL, as the reaction row stores it.
pub(super) const EMOJI_URL: &str = "https://test.example/emoji/railroad_track.png";
/// The acting account's featured (pinned) collection, the `target` an
/// `Add` or `Remove` names.
pub(super) const FEATURED: &str = "https://test.example/users/alice/featured";
/// The instance actor's id, the `actor` a forwarded `Flag` carries.
pub(super) const INSTANCE_AP: &str = "https://test.example/internal/fetch";
/// Every fixture's creation moment, in the wire format.
pub(super) const PUBLISHED: &str = "2024-01-31T12:34:56.789Z";

/// The public `to` line the C2S route keeps on the relation documents
/// it serves.
pub(super) const PUBLIC_TO: [&str; 1] = [AS_PUBLIC];
/// The empty copy line.
pub(super) const NO_CC: [&str; 0] = [];
/// The delivered interaction's `to`: the acted-on status' author.
pub(super) const AUTHOR_TO: [&str; 1] = [BOB_AP];
/// A public interaction's `cc`: the actor's followers, and the world.
pub(super) const PUBLIC_CC: [&str; 2] = [FOLLOWERS, AS_PUBLIC];
/// A reaction's `cc`: the actor's followers, and nobody else.
pub(super) const FOLLOWERS_CC: [&str; 1] = [FOLLOWERS];

/// The keys every activity document carries; the two-actor builders
/// and the object-addressed ones all emit exactly these, and
/// [`EmojiReactContext`]'s builder adds `content` (custom reactions
/// also `tag`).
pub(super) const ACTIVITY_KEYS: [&str; 8] = [
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
pub(super) fn json_keys(value: &Value) -> Vec<&str> {
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
pub(super) fn config() -> Config {
    Config::for_tests()
}

/// The activity fixture's uuid.
pub(super) fn activity_uuid() -> Uuid {
    Uuid::parse_str(ACTIVITY_ID).unwrap()
}

/// The embedded `Follow`'s uuid.
pub(super) fn follow_uuid() -> Uuid {
    Uuid::parse_str(FOLLOW_ID).unwrap()
}

/// The fixtures' shared creation moment.
pub(super) fn published() -> DateTime<Utc> {
    parse_time(PUBLISHED).unwrap()
}

/// The `@context` every document of the test instance carries.
pub(super) fn expected_context() -> Value {
    json!([
        "https://www.w3.org/ns/activitystreams",
        SCHEMA_URL,
        { "@language": "und" },
    ])
}

/// The `Follow` of `BOB_AP` asking to follow `ALICE_AP`, the document
/// `Accept`, `Reject` and `Undo` embed.
pub(super) fn follow_of_bob() -> Value {
    follow_json(
        &config(),
        &activity_id(&config(), follow_uuid()),
        BOB_AP,
        ALICE_AP,
        &published(),
    )
}

/// The stored id every fixture document carries: [`activity_id`] of
/// the fixture uuid, the shape the relation rows' `ap_id` columns and
/// a reblog row's `uri` hold (decision 6).
pub(super) fn stored_id() -> String {
    activity_id(&config(), activity_uuid())
}

/// The `Move`, `Flag`, `Add` and `Remove` fixtures, for the shared
/// convention sweep every document in this module must pass.
pub(super) fn lifecycle_documents<'a>(
    config: &'a Config,
    id: &'a str,
    published: &'a DateTime<Utc>,
) -> Vec<(&'static str, Value)> {
    let to = [BOB_AP];
    vec![
        (
            "Move",
            move_json(&TargetedActivityContext {
                config,
                id: activity_uuid(),
                actor_ap_id: ALICE_AP,
                object: ALICE_AP,
                target: BOB_AP,
                to: &to,
                cc: &NO_CC,
                published,
            }),
        ),
        (
            "Flag",
            flag_json(&FlagContext {
                config,
                id,
                actor_ap_id: INSTANCE_AP,
                object_ids: &[BOB_AP.to_owned()],
                content: "spam",
                to: &to,
                cc: &NO_CC,
                published,
            }),
        ),
        (
            "Add",
            add_json(&TargetedActivityContext {
                config,
                id: activity_uuid(),
                actor_ap_id: ALICE_AP,
                object: STATUS_URI,
                target: FEATURED,
                to: &PUBLIC_TO,
                cc: &FOLLOWERS_CC,
                published,
            }),
        ),
        (
            "Remove",
            remove_json(&TargetedActivityContext {
                config,
                id: activity_uuid(),
                actor_ap_id: ALICE_AP,
                object: STATUS_URI,
                target: FEATURED,
                to: &PUBLIC_TO,
                cc: &FOLLOWERS_CC,
                published,
            }),
        ),
    ]
}
