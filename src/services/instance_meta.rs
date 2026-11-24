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
//!
//! The same goes for the limits and blocks both documents carry: the
//! Pleroma-shipped constants, the `federation` block ([`Federation`]), the
//! poll and profile-field limits ([`PollLimits`], [`FieldsLimits`]) and the
//! `markup` allowances ([`Markup`]) are defined once, here, so a change lands
//! in `NodeInfo` and the instance documents together.

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{
    config::{Config, Toggle},
    federation::mrf,
};

/// The Pleroma release whose HTTP interface this server replicates
/// (decision 1), kept in the version's compatibility comment.
pub const PLEROMA_COMPATIBILITY_VERSION: &str = "2.10.2";
/// The public source repository of this server: the instance v2
/// document's `source_url` and the `NodeInfo` 2.1 `software.repository` and
/// `software.homepage` (Pleroma's `Pleroma.Application.repository()`).
pub const SOURCE_URL: &str = "https://github.com/TheStranjer/wired-be";
/// Characters one poll option may hold (Pleroma's `[:instance,
/// :poll_limits]` `max_option_chars`).
pub const MAX_POLL_OPTION_CHARS: usize = 200;
/// Seconds a poll may run for at least; 0 means "no minimum" (Pleroma's
/// `min_expiration`).
pub const MIN_POLL_EXPIRATION_SECS: usize = 0;
/// Seconds a poll may run for at most: a year (Pleroma's `max_expiration`
/// of `365 * 24 * 60 * 60`).
pub const MAX_POLL_EXPIRATION_SECS: usize = 365 * 24 * 60 * 60;
/// Bytes an uploaded avatar may hold (Pleroma's `avatar_upload_limit`).
pub const AVATAR_UPLOAD_LIMIT: usize = 2_000_000;
/// Bytes an uploaded banner may hold (Pleroma's `banner_upload_limit`).
pub const BANNER_UPLOAD_LIMIT: usize = 4_000_000;
/// Bytes a profile background may hold (Pleroma's
/// `background_upload_limit`).
pub const BACKGROUND_UPLOAD_LIMIT: usize = 4_000_000;
/// Fields one local account may set (Pleroma's `max_account_fields`).
pub const MAX_ACCOUNT_FIELDS: usize = 10;
/// Fields a remote account may carry before they are dropped (Pleroma's
/// `max_remote_account_fields`).
pub const MAX_REMOTE_ACCOUNT_FIELDS: usize = 20;
/// Characters a profile field name may hold (Pleroma's
/// `account_field_name_length`).
pub const ACCOUNT_FIELD_NAME_LENGTH: usize = 512;
/// Characters a profile field value may hold (Pleroma's
/// `account_field_value_length`).
pub const ACCOUNT_FIELD_VALUE_LENGTH: usize = 2048;
/// Source formats a status may be written in (the three
/// [`crate::util::format`] renders; Pleroma's list names bbcode and
/// Misskey markdown too, which this server has no renderer for).
pub const POST_FORMATS: [&str; 3] = ["text/plain", "text/html", "text/markdown"];

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
/// request must not make, built from the operator's setting.
///
/// Pleroma publishes the same three groups (`timelines`, `profiles`,
/// `activities`), each with a `local` and a `federated`/`remote` key
/// (`restrict_unauthenticated_access?` in `lib/pleroma/config.ex`, and
/// `InstanceView` in `lib/pleroma/web/mastodon_api/views/instance_view.ex`).
/// Each flag here is [`Config::restricts`] for the matching
/// [`RESTRICT_UNAUTHENTICATED_AREAS`](crate::config::RESTRICT_UNAUTHENTICATED_AREAS)
/// name, so what is advertised is what the gated endpoints enforce; with
/// `RESTRICT_UNAUTHENTICATED` unset every flag is `false`.
#[must_use]
pub fn restrict_unauthenticated(config: &Config) -> Value {
    json!({
        "timelines": {
            "local": config.restricts("timelines_local"),
            "federated": config.restricts("timelines_federated"),
        },
        "profiles": {
            "local": config.restricts("profiles_local"),
            "remote": config.restricts("profiles_remote"),
        },
        "activities": {
            "local": config.restricts("activities_local"),
            "remote": config.restricts("activities_remote"),
        },
    })
}

/// The `federation` block both documents publish.
///
/// It sits in `NodeInfo`'s `metadata` and the instance documents'
/// `pleroma.metadata`: a federating instance beside whatever the live MRF
/// chain publishes under the operator's transparency settings.
#[derive(Debug, Serialize)]
pub struct Federation {
    /// Whether this instance federates.
    enabled: bool,
    /// The moderation filters in force, Pleroma-style names in chain
    /// order (the [MRF](crate::federation::mrf) chain's
    /// [`describe`](crate::federation::mrf::describe)); empty when
    /// transparency is off, which publishes no policy at all.
    mrf_policies: Vec<String>,
    /// Instances whose posts are held back; none is configured. Absent
    /// from the transparency-off answer, which keeps only the three keys
    /// the switch leaves standing.
    #[serde(skip_serializing_if = "Option::is_none")]
    quarantined_instances: Option<Vec<&'static str>>,
    /// Whether the published lists hold back any host
    /// (`MRF_TRANSPARENCY_EXCLUSIONS` in use, Pleroma deriving the same
    /// flag from its `[:mrf, :transparency_exclusions]`); with
    /// transparency off, false.
    exclusions: bool,
    /// Each policy's transparency blocks under the keys they name
    /// themselves — `mrf_simple`, `mrf_simple_info`, `mrf_<name>` —
    /// flattened into this object beside the fixed keys.
    #[serde(flatten)]
    mrf_descriptions: Map<String, Value>,
}

impl Federation {
    /// A plain federating instance with no moderation filters and
    /// transparency on: what an empty chain publishes, which is the answer
    /// the live read collapses to when nothing is configured. Test support
    /// for the document builders; production always reads the live chain and
    /// the live settings.
    #[cfg(test)]
    #[must_use]
    pub fn federating() -> Self {
        let published = mrf::ChainTransparency {
            published: true,
            ..mrf::ChainTransparency::default()
        };
        Self::with_mrf(&published)
    }

    /// The block of an instance whose chain published `described`.
    #[must_use]
    pub fn with_mrf(described: &mrf::ChainTransparency) -> Self {
        Self {
            enabled: true,
            mrf_policies: described.policies.clone(),
            quarantined_instances: described.quarantined_instances(),
            exclusions: described.exclusions,
            mrf_descriptions: described.descriptions.clone(),
        }
    }
}

/// The poll limits block (`NodeInfo`'s `metadata.pollLimits`, the instance
/// document's top-level `poll_limits`); its keys keep Pleroma's `snake_case`
/// config names.
#[derive(Debug, Serialize)]
pub struct PollLimits {
    /// Options one poll may offer (`MAX_POLL_OPTIONS`).
    max_options: usize,
    /// Characters one option may hold.
    max_option_chars: usize,
    /// Shortest poll duration, in seconds.
    min_expiration: usize,
    /// Longest poll duration, in seconds.
    max_expiration: usize,
}

impl PollLimits {
    /// The limits this instance enforces: its configured option count with
    /// Pleroma's shipped values around it.
    #[must_use]
    pub const fn for_config(config: &Config) -> Self {
        Self {
            max_options: config.max_poll_options,
            max_option_chars: MAX_POLL_OPTION_CHARS,
            min_expiration: MIN_POLL_EXPIRATION_SECS,
            max_expiration: MAX_POLL_EXPIRATION_SECS,
        }
    }
}

/// The profile-field limits both documents publish.
///
/// The instance documents publish them as
/// `pleroma.metadata.fields_limits` under Pleroma's own `snake_case` config
/// names (this type's [`Serialize`]); `NodeInfo` publishes the same values
/// as `metadata.fieldsLimits` with camelCase keys
/// ([`FieldsLimits::camel_case`]).
#[derive(Debug, Serialize)]
pub struct FieldsLimits {
    /// Fields one local account may set.
    max_fields: usize,
    /// Fields a remote account may carry before they are dropped.
    max_remote_fields: usize,
    /// Characters a field name may hold.
    name_length: usize,
    /// Characters a field value may hold.
    value_length: usize,
}

impl FieldsLimits {
    /// Pleroma 2.10.2's shipped values; wired-be enforces no stricter
    /// profile-field settings.
    #[must_use]
    pub const fn pleroma_defaults() -> Self {
        Self {
            max_fields: MAX_ACCOUNT_FIELDS,
            max_remote_fields: MAX_REMOTE_ACCOUNT_FIELDS,
            name_length: ACCOUNT_FIELD_NAME_LENGTH,
            value_length: ACCOUNT_FIELD_VALUE_LENGTH,
        }
    }

    /// The same limits under `NodeInfo`'s camelCase key spelling.
    #[must_use]
    pub fn camel_case(&self) -> Value {
        json!({
            "maxFields": self.max_fields,
            "maxRemoteFields": self.max_remote_fields,
            "nameLength": self.name_length,
            "valueLength": self.value_length,
        })
    }
}

/// The `markup` block: the formatting this instance allows.
///
/// It describes what [`crate::util::sanitize`] implements. The field names drop Pleroma's
/// `allow_` config prefix for Clippy's `struct_field_names`; `serde`
/// restores it on the wire, so consumers still read `allow_inline_images`
/// and friends.
#[derive(Debug, Serialize)]
pub struct Markup {
    /// Whether a status may show inline images; the sanitizer keeps `img`
    /// (restricted to custom emoji, Pleroma's default).
    #[serde(rename = "allow_inline_images")]
    inline_images: Toggle,
    /// Whether heading markup survives; the sanitizer keeps `h1`–`h5`.
    #[serde(rename = "allow_headings")]
    headings: Toggle,
    /// Whether tables survive; they do not.
    #[serde(rename = "allow_tables")]
    tables: Toggle,
    /// Whether font styling survives; it does not.
    #[serde(rename = "allow_fonts")]
    fonts: Toggle,
}

impl Markup {
    /// What [`crate::util::sanitize`] lets through: inline images and
    /// headings, neither tables nor fonts.
    #[must_use]
    pub const fn sanitizer_allowances() -> Self {
        Self {
            inline_images: Toggle::new(true),
            headings: Toggle::new(true),
            tables: Toggle::new(false),
            fonts: Toggle::new(false),
        }
    }
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
        let restrictions = restrict_unauthenticated(&Config::for_tests());
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

    /// A [`Config`] parsed through `from_lookup` with `RESTRICT_UNAUTHENTICATED=areas`.
    fn config_restricting(areas: &str) -> Config {
        Config::from_lookup(|key| match key {
            crate::config::DATABASE_URL_VAR => Some("postgres://localhost/test".to_owned()),
            crate::config::BASE_URL_VAR => Some("https://test.example".to_owned()),
            crate::config::RESTRICT_UNAUTHENTICATED_VAR => Some(areas.to_owned()),
            _ => None,
        })
        .expect("a valid configuration")
    }

    #[test]
    fn the_configured_areas_are_exactly_the_true_flags() {
        let config = config_restricting("profiles_local,timelines_federated");
        assert_eq!(
            restrict_unauthenticated(&config),
            json!({
                "timelines": { "local": false, "federated": true },
                "profiles": { "local": true, "remote": false },
                "activities": { "local": false, "remote": false },
            })
        );
    }

    #[test]
    fn a_second_mix_of_areas_is_published_as_configured() {
        let config = config_restricting("timelines_federated,activities_local");
        assert_eq!(
            restrict_unauthenticated(&config),
            json!({
                "timelines": { "local": false, "federated": true },
                "profiles": { "local": false, "remote": false },
                "activities": { "local": true, "remote": false },
            })
        );
    }

    #[test]
    fn every_area_flag_equals_config_restricts() {
        let mut config = Config::for_tests();
        config.restrict_unauthenticated = crate::config::RESTRICT_UNAUTHENTICATED_AREAS
            .iter()
            .map(|area| (*area).to_owned())
            .collect();
        let value = restrict_unauthenticated(&config);
        for (area, flag) in [
            ("timelines_local", &value["timelines"]["local"]),
            ("timelines_federated", &value["timelines"]["federated"]),
            ("profiles_local", &value["profiles"]["local"]),
            ("profiles_remote", &value["profiles"]["remote"]),
            ("activities_local", &value["activities"]["local"]),
            ("activities_remote", &value["activities"]["remote"]),
        ] {
            assert_eq!(flag, &json!(config.restricts(area)), "{area}");
            assert_eq!(flag, &json!(true), "{area} is restricted");
        }
    }

    /// Whether `html` still holds a `<tag` element after sanitizing.
    fn survives(html: &str, tag: &str) -> bool {
        crate::util::sanitize::sanitize_html(html).contains(&format!("<{tag}"))
    }

    /// The published `markup` flags are what the sanitizer actually keeps.
    #[test]
    fn markup_claims_match_the_sanitizer() {
        let markup = serde_json::to_value(Markup::sanitizer_allowances()).unwrap();
        assert_eq!(
            markup["allow_headings"],
            json!(survives("<h1>x</h1>", "h1")),
            "headings claim matches h1 handling"
        );
        for tag in ["h1", "h2", "h3", "h4", "h5"] {
            assert_eq!(
                survives(&format!("<{tag}>x</{tag}>"), tag),
                markup["allow_headings"] == json!(true),
                "{tag} agrees with allow_headings"
            );
        }
        assert_eq!(
            markup["allow_inline_images"],
            json!(survives(r#"<img src="https://x.example/e.png">"#, "img")),
            "inline images claim matches img handling"
        );
        assert_eq!(
            markup["allow_tables"],
            json!(survives("<table><tr><td>x</td></tr></table>", "table")),
            "tables claim matches table handling"
        );
        assert_eq!(
            markup["allow_fonts"],
            json!(survives(r#"<font color="red">x</font>"#, "font")),
            "fonts claim matches font handling"
        );
    }

    #[test]
    fn markup_is_pinned_to_the_sanitizer_policy() {
        assert_eq!(
            serde_json::to_value(Markup::sanitizer_allowances()).unwrap(),
            json!({
                "allow_inline_images": true,
                "allow_headings": true,
                "allow_tables": false,
                "allow_fonts": false,
            })
        );
    }

    #[test]
    fn fields_limits_serialize_in_both_spellings() {
        let limits = FieldsLimits::pleroma_defaults();
        assert_eq!(
            serde_json::to_value(&limits).unwrap(),
            json!({"max_fields": 10, "max_remote_fields": 20, "name_length": 512, "value_length": 2048}),
            "snake_case for the instance documents"
        );
        assert_eq!(
            limits.camel_case(),
            json!({"maxFields": 10, "maxRemoteFields": 20, "nameLength": 512, "valueLength": 2048}),
            "camelCase for NodeInfo"
        );
    }

    #[test]
    fn poll_limits_follow_the_configured_option_count() {
        let mut config = Config::for_tests();
        config.max_poll_options = 7;
        assert_eq!(
            serde_json::to_value(PollLimits::for_config(&config)).unwrap(),
            json!({"max_options": 7, "max_option_chars": 200, "min_expiration": 0, "max_expiration": 31_536_000})
        );
    }

    #[test]
    fn a_federating_block_has_the_four_fixed_keys() {
        assert_eq!(
            serde_json::to_value(Federation::federating()).unwrap(),
            json!({"enabled": true, "mrf_policies": [], "quarantined_instances": [], "exclusions": false})
        );
    }
}
