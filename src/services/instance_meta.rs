//! Instance-describing metadata shared by the client-facing discovery
//! endpoints (`docs/ARCHITECTURE.md` decision 1).
//!
//! `NodeInfo` (`src/api/nodeinfo.rs`) and the Mastodon instance endpoints
//! (`/api/v1/instance`, its `pleroma.metadata` block) advertise the same
//! capability facts about this instance; Pleroma builds both from one place
//! (`InstanceView.features/0` and `InstanceView.federation/0` in
//! `lib/pleroma/web/mastodon_api/views/instance_view.ex`), and this module
//! is wired-be's. The builders here are the single source of truth: later
//! tickets adding the instance endpoints reuse them instead of restating
//! the lists.

use serde_json::{Value, json};

/// The feature flags Pleroma 2.10.2 advertises under
/// `pleroma.metadata.features` (`InstanceView.features/0`,
/// `lib/pleroma/web/mastodon_api/views/instance_view.ex`) when read with
/// that release's shipped `config.exs` defaults: the unconditional entries
/// plus every conditional one its defaults enable (`blockers_visible`,
/// `chat`, `shout`, `relay`, `pleroma_chat_messages`, `pin_chats`,
/// `exposable_reactions`, `profile_directory`) and none its defaults
/// disable (`media_proxy`, `gopher`, `safe_dm_mentions`, language
/// detection). The list describes the API dialect — which is what clients
/// sniff — not which endpoint a given build has wired up yet, exactly as
/// in Pleroma.
const FEATURES: [&str; 26] = [
    "pleroma_api",
    "mastodon_api",
    "mastodon_api_grouped_notifications",
    "mastodon_api_streaming",
    "polls",
    "v2_suggestions",
    "pleroma_explicit_addressing",
    "shareable_emoji_packs",
    "multifetch",
    "pleroma:api/v1/notifications:include_types_filter",
    "editing",
    "quote_posting",
    "blockers_visible",
    "chat",
    "shout",
    "relay",
    "pleroma_emoji_reactions",
    "pleroma_custom_emoji_reactions",
    "pleroma_chat_messages",
    "pleroma:pin_chats",
    "exposable_reactions",
    "profile_directory",
    "pleroma:get:main/ostatus",
    "pleroma:group_actors",
    "pleroma:bookmark_folders",
    "pleroma:block_expiration",
];

/// The instance's advertised feature flags, in Pleroma's own order.
#[must_use]
pub const fn features() -> &'static [&'static str] {
    &FEATURES
}

/// The `restrict_unauthenticated` block: which reads an unauthenticated
/// request must not make.
///
/// Pleroma's `:restrict_unauthenticated` config (`config/config.exs`) sets
/// every key to `:if_instance_is_private`, so a public instance — wired-be
/// has no private-instance mode to configure — answers all-false with the
/// same three groups and sub-keys. This is that fixed advertisement: the
/// operator-side setting that closes a read is
/// [`RESTRICT_UNAUTHENTICATED`](crate::config::RESTRICT_UNAUTHENTICATED_VAR),
/// read through [`crate::config::Config::restricts`], and the endpoints that
/// gate on it are the ones that say so in their own docs; wiring this block
/// to it is the instance documents' ticket.
#[must_use]
pub fn restrict_unauthenticated() -> Value {
    json!({
        "timelines": { "local": false, "federated": false },
        "profiles": { "local": false, "remote": false },
        "activities": { "local": false, "remote": false },
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// The complete feature list, spelled out entry by entry in order.
    /// The Pleroma compatibility surface is deliberate: a later cleanup
    /// must change this test on purpose for a flag to leave the list.
    #[test]
    fn the_full_feature_list_is_pinned() {
        assert_eq!(
            features(),
            &[
                "pleroma_api",
                "mastodon_api",
                "mastodon_api_grouped_notifications",
                "mastodon_api_streaming",
                "polls",
                "v2_suggestions",
                "pleroma_explicit_addressing",
                "shareable_emoji_packs",
                "multifetch",
                "pleroma:api/v1/notifications:include_types_filter",
                "editing",
                "quote_posting",
                "blockers_visible",
                "chat",
                "shout",
                "relay",
                "pleroma_emoji_reactions",
                "pleroma_custom_emoji_reactions",
                "pleroma_chat_messages",
                "pleroma:pin_chats",
                "exposable_reactions",
                "profile_directory",
                "pleroma:get:main/ostatus",
                "pleroma:group_actors",
                "pleroma:bookmark_folders",
                "pleroma:block_expiration",
            ][..],
            "every Pleroma-dialect flag stays advertised, in Pleroma's order"
        );
    }

    #[test]
    fn features_are_unique_and_hold_the_core_dialects() {
        let listed = features();
        assert!(!listed.is_empty(), "the list advertises features");
        let unique: HashSet<&str> = listed.iter().copied().collect();
        assert_eq!(unique.len(), listed.len(), "no feature is listed twice");
        for core in ["pleroma_api", "mastodon_api", "shareable_emoji_packs"] {
            assert!(listed.contains(&core), "{core} is advertised");
        }
        assert_eq!(
            listed.first().copied(),
            Some("pleroma_api"),
            "Pleroma's order starts with pleroma_api"
        );
    }

    #[test]
    fn a_public_instance_restricts_nothing() {
        let restrictions = restrict_unauthenticated();
        let groups = restrictions
            .as_object()
            .expect("an object of access groups");
        assert_eq!(
            groups.keys().collect::<Vec<_>>(),
            vec!["activities", "profiles", "timelines"],
            "Pleroma's three groups"
        );
        for group in groups.values() {
            let keys = group.as_object().expect("a group is an object");
            assert_eq!(keys.len(), 2, "each group has two access keys");
            for value in keys.values() {
                assert_eq!(value, &json!(false), "a public instance restricts nothing");
            }
        }
    }
}
