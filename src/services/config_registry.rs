//! The whitelist registry of runtime-configurable settings: the
//! `(group, key)` pairs the admin config store accepts, beside what each
//! one falls back to when the database holds no row.
//!
//! The registry is also the instance's config documentation: every entry
//! carries its `description`, `kind` and `children` metadata (with the
//! `suggestions` example values), the payload of the admin API's
//! `GET /config/descriptions` — Pleroma's `config/description.exs` docs
//! and policy knowledge docs, stated here as the whitelist's own data.
//!
//! The reference is Pleroma's `:database_config_whitelist`
//! (`config/config.exs`) — the list `Pleroma.Web.AdminAPI.ConfigController`
//! gates every `POST /api/pleroma/admin/config` entry against — with the
//! registry itself spelled out here because wired-be whitelists exactly
//! these eight keys, all in group `:pleroma`:
//!
//! - `:instance` — the instance's public identity and registration gates,
//!   whitelist-subkeyed to `name`, `description`, `email`,
//!   `registrations_open`, `account_approval_required`,
//!   `account_activation_required`, `invites_enabled`, `limit` and
//!   `max_media_attachments`;
//! - `:mrf` — the moderation pipeline, whitelist-subkeyed to `policies`;
//! - `:mrf_simple`, `:mrf_keyword`, `:mrf_hellthread` and
//!   `:mrf_object_age` — one whole parameter object per policy;
//! - `:media_proxy` and `:restrict_unauthenticated` — whole values,
//!   reported from the environment but not yet read at runtime, hence
//!   [`RegistryEntry::requires_restart`].
//!
//! [`runtime_config`]: crate::services::runtime_config

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::config::Config;

/// The configuration group every registered entry belongs to
/// (Pleroma's `:pleroma` app group, colon-spelled the way the admin API
/// names it on the wire).
pub const GROUP_PLEROMA: &str = ":pleroma";
/// The instance identity and registration keys.
pub const KEY_INSTANCE: &str = ":instance";
/// The moderation pipeline's enabled policies.
pub const KEY_MRF: &str = ":mrf";
/// The `simple` policy's domain lists.
pub const KEY_MRF_SIMPLE: &str = ":mrf_simple";
/// The `keyword` policy's patterns and replacements.
pub const KEY_MRF_KEYWORD: &str = ":mrf_keyword";
/// The `hellthread` policy's mention gates.
pub const KEY_MRF_HELLTHREAD: &str = ":mrf_hellthread";
/// The `object_age` policy's age gates.
pub const KEY_MRF_OBJECT_AGE: &str = ":mrf_object_age";
/// The media proxy's runtime shape.
pub const KEY_MEDIA_PROXY: &str = ":media_proxy";
/// Which reads anonymous requests may not make.
pub const KEY_RESTRICT_UNAUTHENTICATED: &str = ":restrict_unauthenticated";
/// The `:media_proxy` value's reported subkeys: the public part of the
/// proxy's shape, read off `MEDIA_PROXY_*` and documented here so the
/// registry states its wire shape as data.
const MEDIA_PROXY_SUBKEYS: &[&str] = &["enabled", "blacklist", "max_bytes"];
/// Pleroma's `[:instance, :limit]` is the status text length; the
/// instance document spells it `max_toot_chars`.
const INSTANCE_LIMIT: &str = "limit";

/// The value type of a registry entry or one of its children: the
/// `type` field of the admin API's description payload.
///
/// The names are the strings Pleroma's docs generator puts there
/// (`lib/pleroma/docs/generator.ex` turns the atoms of
/// `config/description.exs` into their string spelling), with
/// [`ValueKind::AtomList`] spelled as Pleroma spells the tuple type
/// `{:list, :atom}` — `[:atom]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueKind {
    /// Text value (Pleroma's `:string`).
    String,
    /// `true` / `false` (Pleroma's `:boolean`).
    Boolean,
    /// Whole number (Pleroma's `:integer`).
    Integer,
    /// A list of names — Pleroma's `{:list, :atom}`, spelled `[:atom]`.
    #[serde(rename = "[:atom]")]
    AtomList,
    /// A keyword list: a named set of subkeys (Pleroma's `:keyword`).
    Keyword,
    /// A map of named values (Pleroma's `:map`).
    Map,
    /// A container of documented subkeys (Pleroma's `:group`).
    Group,
}

/// One example value of a [`ChildDescription`]'s `suggestions` — the
/// fill-in hints Pleroma's admin frontends offer. A const-constructible
/// stand-in for a [`Value`], serialized untagged as its JSON shape.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(untagged)]
pub enum Suggestion {
    /// A text example.
    Text(&'static str),
    /// A number example.
    Count(i64),
    /// A boolean example.
    Flag(bool),
}

/// The description of one subkey or parameter — a `children` element of
/// the admin API's description payload.
///
/// The data is Pleroma's docs metadata (`config/description.exs` and the
/// policies' knowledge docs) carried here as registry data. A documented
/// container nests its own subkeys in [`children`] — Pleroma's
/// two-level `:map` entries, e.g. `:restrict_unauthenticated`.
///
/// [`children`]: ChildDescription::children
#[derive(Debug, Serialize)]
pub struct ChildDescription {
    /// The subkey's name as the store and the wire spell it.
    pub key: &'static str,
    /// The subkey's value type, serialized as `type`.
    #[serde(rename = "type")]
    pub kind: ValueKind,
    /// Human-readable documentation of what the subkey controls.
    pub description: &'static str,
    /// Example values; empty renders no `suggestions` field.
    #[serde(skip_serializing_if = "no_suggestions")]
    pub suggestions: &'static [Suggestion],
    /// Documented subkeys of a container; empty renders no `children`
    /// field.
    #[serde(skip_serializing_if = "no_children")]
    pub children: &'static [Self],
}

/// The `suggestions` absence predicate: an empty hint list renders no
/// field, Pleroma's `format_child` only echoing what the docs carry.
const fn no_suggestions(suggestions: &&[Suggestion]) -> bool {
    suggestions.is_empty()
}

/// The `children` absence predicate of a leaf description.
const fn no_children(children: &&[ChildDescription]) -> bool {
    children.is_empty()
}

/// One element of the admin API's description payload: a registry entry
/// with its documentation and children.
///
/// The whole payload is the array Pleroma's
/// `ConfigController.descriptions` answers — here generated from the
/// whitelist itself, so a registered key can never lack its description.
#[derive(Debug, Serialize)]
pub struct EntryDescription {
    /// The configuration group, colon-spelled.
    pub group: &'static str,
    /// The configuration key, colon-spelled.
    pub key: &'static str,
    /// The entry's value type, serialized as `type`.
    #[serde(rename = "type")]
    pub kind: ValueKind,
    /// Human-readable documentation of what the entry groups.
    pub description: &'static str,
    /// The entry's documented subkeys.
    #[serde(skip_serializing_if = "no_children")]
    pub children: &'static [ChildDescription],
}

/// The description payload of `GET /config/descriptions`: one element per
/// registry entry, in [`entries`] order.
///
/// [`entries`]: entries()
#[must_use]
pub fn descriptions() -> Vec<EntryDescription> {
    entries()
        .iter()
        .map(|entry| EntryDescription {
            group: entry.group,
            key: entry.key,
            kind: entry.kind,
            description: entry.description,
            children: entry.children,
        })
        .collect()
}

/// One whitelisted runtime setting: what it is called, which subkeys the
/// whitelist enumerates for it, and whether changing it needs a restart.
///
/// Beside the gating data the entry carries its environment fallback
/// (`defaults`) and the documentation metadata (`description`, `kind`,
/// `children`) the admin API's `GET /config/descriptions` answers with.
#[derive(Debug)]
pub struct RegistryEntry {
    /// The configuration group, [`GROUP_PLEROMA`] for every entry.
    pub group: &'static str,
    /// The configuration key, colon-spelled as the admin API names it.
    pub key: &'static str,
    /// The entry's value type for the description payload: the entries
    /// the whitelist subkeys or the docs group are [`ValueKind::Group`],
    /// the whole-policy parameter objects [`ValueKind::Keyword`].
    pub kind: ValueKind,
    /// What the entry is for, in the description payload's words.
    pub description: &'static str,
    /// The entry's documented subkeys, the description payload's
    /// `children`; empty renders no `children` field.
    pub children: &'static [ChildDescription],
    /// The whitelist's subkeys for this entry; empty means the whole
    /// value is one unit (Pleroma's bare `:mrf_simple`-style entries).
    pub subkeys: &'static [&'static str],
    /// Whether a change only takes effect after a restart: `true` for the
    /// entries no runtime consumer reads yet, and what the admin API's
    /// `need_reboot` flag is computed from.
    pub requires_restart: bool,
    /// The entry's environment-derived default, read off [`Config`].
    pub defaults: fn(&Config) -> Value,
}

/// The registry, in the order `GET /config` without `only_db` answers:
/// the identity first, then the moderation family, then the two
/// restart-gated entries.
#[must_use]
pub const fn entries() -> &'static [RegistryEntry] {
    &REGISTRY
}

/// The entry the pair `(group, key)` names, `None` for anything the
/// whitelist does not carry — the answer that becomes the API's 400
/// `Config not allowed: <group>/<key>`.
#[must_use]
pub fn find(group: &str, key: &str) -> Option<&'static RegistryEntry> {
    entries()
        .iter()
        .find(|entry| entry.group == group && entry.key == key)
}

/// Whether changing `(group, key)` needs a restart; `false` for an
/// unregistered pair, which is refused before anything is ever changed.
#[must_use]
pub fn requires_restart(group: &str, key: &str) -> bool {
    find(group, key).is_some_and(|entry| entry.requires_restart)
}

/// A leaf description with no example values.
const fn leaf(key: &'static str, kind: ValueKind, description: &'static str) -> ChildDescription {
    ChildDescription {
        key,
        kind,
        description,
        suggestions: &[],
        children: &[],
    }
}

/// A leaf description with example values.
const fn hinted(
    key: &'static str,
    kind: ValueKind,
    description: &'static str,
    suggestions: &'static [Suggestion],
) -> ChildDescription {
    ChildDescription {
        key,
        kind,
        description,
        suggestions,
        children: &[],
    }
}

/// A nested container description: the `:map` children of
/// [`RESTRICT_UNAUTHENTICATED_CHILDREN`].
const fn nested(
    key: &'static str,
    kind: ValueKind,
    description: &'static str,
    children: &'static [ChildDescription],
) -> ChildDescription {
    ChildDescription {
        key,
        kind,
        description,
        suggestions: &[],
        children,
    }
}

/// The `:instance` children: the nine whitelisted subkeys ([`RegistryEntry::subkeys`]),
/// documented the way Pleroma's `config/description.exs` does.
const INSTANCE_CHILDREN: [ChildDescription; 9] = [
    hinted(
        "name",
        ValueKind::String,
        "Name of the instance",
        &[Suggestion::Text("Wired")],
    ),
    hinted(
        "description",
        ValueKind::String,
        "The instance's description. It can be seen in nodeinfo and `/api/v1/instance`",
        &[Suggestion::Text("Very cool instance")],
    ),
    hinted(
        "email",
        ValueKind::String,
        "Email used to reach an Administrator/Moderator of the instance",
        &[Suggestion::Text("email@example.com")],
    ),
    leaf(
        "registrations_open",
        ValueKind::Boolean,
        "Enable registrations for anyone. Invitations require this setting to be disabled.",
    ),
    leaf(
        "account_approval_required",
        ValueKind::Boolean,
        "Require users to be manually approved by an admin before signing in",
    ),
    leaf(
        "account_activation_required",
        ValueKind::Boolean,
        "Require users to confirm their emails before signing in",
    ),
    leaf(
        "invites_enabled",
        ValueKind::Boolean,
        "Enable user invitations for admins (depends on `registrations_open` being disabled)",
    ),
    hinted(
        INSTANCE_LIMIT,
        ValueKind::Integer,
        "Posts character limit (CW/Subject included in the counter)",
        &[Suggestion::Count(5_000)],
    ),
    hinted(
        "max_media_attachments",
        ValueKind::Integer,
        "Maximum number of post media attachments",
        &[Suggestion::Count(4)],
    ),
];

/// The `:mrf` children: the one whitelisted subkey, the policy chain.
const MRF_CHILDREN: [ChildDescription; 1] = [hinted(
    "policies",
    ValueKind::AtomList,
    "The MRF policies applied to incoming activities, by short name.",
    &[
        Suggestion::Text("anti_link_spam"),
        Suggestion::Text("hellthread"),
        Suggestion::Text("keyword"),
        Suggestion::Text("normalize_markup"),
        Suggestion::Text("object_age"),
        Suggestion::Text("simple"),
        Suggestion::Text("tag"),
    ],
)];

/// The `:mrf_simple` children: the eleven per-instance lists the policy
/// reads, described in the words of Pleroma's `MRFSimple` knowledge doc.
const MRF_SIMPLE_CHILDREN: [ChildDescription; 11] = [
    leaf(
        "accept",
        ValueKind::AtomList,
        "List of instances to accept, even if they violate the reject policies",
    ),
    leaf(
        "reject",
        ValueKind::AtomList,
        "List of instances to reject all activities from",
    ),
    leaf(
        "reject_deletes",
        ValueKind::AtomList,
        "List of instances whose Delete activities will be rejected",
    ),
    leaf(
        "media_removal",
        ValueKind::AtomList,
        "List of instances to remove media from posts of",
    ),
    leaf(
        "media_nsfw",
        ValueKind::AtomList,
        "List of instances to mark media in posts from as sensitive",
    ),
    leaf(
        "avatar_removal",
        ValueKind::AtomList,
        "List of instances to remove the avatars of",
    ),
    leaf(
        "banner_removal",
        ValueKind::AtomList,
        "List of instances to remove the banners of",
    ),
    leaf(
        "background_removal",
        ValueKind::AtomList,
        "List of instances to remove the profiles' backgrounds of",
    ),
    leaf(
        "report_removal",
        ValueKind::AtomList,
        "List of instances to reject reports from",
    ),
    leaf(
        "federated_timeline_removal",
        ValueKind::AtomList,
        "List of instances to remove public posts from, so that they only appear in the local timeline",
    ),
    leaf(
        "followers_only",
        ValueKind::AtomList,
        "List of instances whose followers-only posts will be rejected",
    ),
];

/// The `:mrf_keyword` children: the three phrase lists the policy reads.
const MRF_KEYWORD_CHILDREN: [ChildDescription; 3] = [
    hinted(
        "reject",
        ValueKind::AtomList,
        "List of phrases and regular expressions which will be rejected if they are found in a post's content.",
        &[Suggestion::Text("/^buy\\s+now\\b/i")],
    ),
    leaf(
        "federated_timeline_removal",
        ValueKind::AtomList,
        "List of phrases and regular expressions which will cause a post to be removed from the federated timeline.",
    ),
    hinted(
        "replace",
        ValueKind::AtomList,
        "List of `[pattern, replacement]` pairs rewritten in a post's content.",
        &[Suggestion::Text("bad word")],
    ),
];

/// The `:mrf_hellthread` children: the two mention gates, with this
/// repo's defaults as the example values.
const MRF_HELLTHREAD_CHILDREN: [ChildDescription; 2] = [
    hinted(
        "delist_threshold",
        ValueKind::Integer,
        "The number of local users mentioned which will cause a post to be removed from public timelines. Set to `0` to disable delisting.",
        &[Suggestion::Count(10)],
    ),
    hinted(
        "reject_threshold",
        ValueKind::Integer,
        "The number of local users mentioned which will cause a post to be rejected entirely. Set to `0` to disable rejection.",
        &[Suggestion::Count(20)],
    ),
];

/// The `:mrf_object_age` children: the age gate and the actions it runs.
const MRF_OBJECT_AGE_CHILDREN: [ChildDescription; 2] = [
    hinted(
        "threshold",
        ValueKind::Integer,
        "Age in seconds past which the configured actions run.",
        &[Suggestion::Count(604_800)],
    ),
    hinted(
        "actions",
        ValueKind::AtomList,
        "The actions to run on objects older than `threshold`.",
        &[
            Suggestion::Text("delist"),
            Suggestion::Text("strip_followers"),
            Suggestion::Text("reject"),
        ],
    ),
];

/// The `:media_proxy` children: the three reported subkeys.
const MEDIA_PROXY_CHILDREN: [ChildDescription; 3] = [
    leaf(
        "enabled",
        ValueKind::Boolean,
        "Enables proxying of remote media via the instance's proxy",
    ),
    leaf(
        "blacklist",
        ValueKind::AtomList,
        "List of hosts the proxy refuses to fetch media from",
    ),
    hinted(
        "max_bytes",
        ValueKind::Integer,
        "Maximum file size (in bytes) allowed through the proxy",
        &[Suggestion::Count(25_000_000)],
    ),
];

/// The booleans under `:restrict_unauthenticated`'s `timelines` map.
const RESTRICT_TIMELINES_CHILDREN: [ChildDescription; 3] = [
    leaf(
        "local",
        ValueKind::Boolean,
        "Disallow view public timeline.",
    ),
    leaf(
        "federated",
        ValueKind::Boolean,
        "Disallow view federated timeline.",
    ),
    leaf(
        "remote",
        ValueKind::Boolean,
        "Disallow viewing remote users' profiles' timelines.",
    ),
];

/// The booleans under the `profiles` and `activities` maps.
const RESTRICT_HALFS_CHILDREN: [ChildDescription; 2] = [
    leaf(
        "local",
        ValueKind::Boolean,
        "Disallow view local content in this area.",
    ),
    leaf(
        "remote",
        ValueKind::Boolean,
        "Disallow view remote content in this area.",
    ),
];

/// The `:restrict_unauthenticated` children: Pleroma's three nested
/// maps, spelled out.
const RESTRICT_UNAUTHENTICATED_CHILDREN: [ChildDescription; 3] = [
    nested(
        "timelines",
        ValueKind::Map,
        "Settings for public and federated timelines.",
        &RESTRICT_TIMELINES_CHILDREN,
    ),
    nested(
        "profiles",
        ValueKind::Map,
        "Settings for user profiles.",
        &RESTRICT_HALFS_CHILDREN,
    ),
    nested(
        "activities",
        ValueKind::Map,
        "Settings for statuses.",
        &RESTRICT_HALFS_CHILDREN,
    ),
];

/// The whole registry: the eight entries the ticket whitelists.
static REGISTRY: [RegistryEntry; 8] = [
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_INSTANCE,
        kind: ValueKind::Group,
        description: "Instance-related settings",
        children: &INSTANCE_CHILDREN,
        subkeys: &[
            "name",
            "description",
            "email",
            "registrations_open",
            "account_approval_required",
            "account_activation_required",
            "invites_enabled",
            INSTANCE_LIMIT,
            "max_media_attachments",
        ],
        requires_restart: false,
        defaults: instance_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_MRF,
        kind: ValueKind::Group,
        description: "General MRF settings",
        children: &MRF_CHILDREN,
        subkeys: &["policies"],
        requires_restart: false,
        defaults: mrf_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_MRF_SIMPLE,
        kind: ValueKind::Keyword,
        description: "Simple policy: the per-instance lists it applies",
        children: &MRF_SIMPLE_CHILDREN,
        subkeys: &[],
        requires_restart: false,
        defaults: empty_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_MRF_KEYWORD,
        kind: ValueKind::Keyword,
        description: "Keyword policy: the phrase filters and replacements it applies",
        children: &MRF_KEYWORD_CHILDREN,
        subkeys: &[],
        requires_restart: false,
        defaults: empty_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_MRF_HELLTHREAD,
        kind: ValueKind::Keyword,
        description: "Hellthread policy: the mention thresholds it gates on",
        children: &MRF_HELLTHREAD_CHILDREN,
        subkeys: &[],
        requires_restart: false,
        defaults: empty_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_MRF_OBJECT_AGE,
        kind: ValueKind::Keyword,
        description: "Object age policy: the age gate and the actions it runs",
        children: &MRF_OBJECT_AGE_CHILDREN,
        subkeys: &[],
        requires_restart: false,
        defaults: empty_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_MEDIA_PROXY,
        kind: ValueKind::Group,
        description: "Media proxy",
        children: &MEDIA_PROXY_CHILDREN,
        subkeys: MEDIA_PROXY_SUBKEYS,
        requires_restart: true,
        defaults: media_proxy_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_RESTRICT_UNAUTHENTICATED,
        kind: ValueKind::Group,
        description: "Disallow viewing timelines, user profiles and statuses for unauthenticated users.",
        children: &RESTRICT_UNAUTHENTICATED_CHILDREN,
        subkeys: &[],
        requires_restart: true,
        defaults: restrict_unauthenticated_defaults,
    },
];

/// The `:instance` fallback: the nine whitelisted subkeys, each read off
/// the environment-derived [`Config`] it comes from — `email` as the
/// empty string when unconfigured, `account_approval_required` as
/// `REGISTRATIONS_APPROVAL_REQUIRED`, and `limit`
/// ([`INSTANCE_LIMIT`]) as `MAX_STATUS_CHARS`.
fn instance_defaults(config: &Config) -> Value {
    json!({
        "name": config.instance_name,
        "description": config.instance_description,
        "email": config.contact_email.clone().unwrap_or_default(),
        "registrations_open": config.registrations_open.get(),
        "account_approval_required": config.registrations_approval_required.get(),
        "account_activation_required": config.account_activation_required.get(),
        "invites_enabled": config.invites_enabled.get(),
        INSTANCE_LIMIT: config.max_status_chars,
        "max_media_attachments": config.max_media_attachments,
    })
}

/// The `:mrf` fallback: the `MRF_POLICIES` chain, which is the pipeline's
/// env source exactly as [`crate::federation::mrf`] reads it today.
fn mrf_defaults(config: &Config) -> Value {
    json!({ "policies": config.mrf.policies })
}

/// The fallback of the entries with no environment counterpart: the empty
/// keyword list, which on the wire is the empty tuple array.
fn empty_defaults(_config: &Config) -> Value {
    Value::Object(Map::new())
}

/// The `:media_proxy` fallback: the three public subkeys
/// ([`MEDIA_PROXY_SUBKEYS`]) read off `MEDIA_PROXY_*`. The HMAC secret is
/// deliberately absent — the store never carries it, and an admin document
/// readable by any token holder is no place for signing key material.
fn media_proxy_defaults(config: &Config) -> Value {
    json!({
        "enabled": config.media_proxy.enabled,
        "blacklist": config.media_proxy.blacklist,
        "max_bytes": config.media_proxy.max_bytes,
    })
}

/// The `:restrict_unauthenticated` fallback: Pleroma's nested shape —
/// the `RESTRICT_UNAUTHENTICATED` flat `group_half` names regrouped into
/// `{"timelines": {"local": …, "federated": …, "remote": …}, …}`.
fn restrict_unauthenticated_defaults(config: &Config) -> Value {
    let restricted = |area: &str| config.restrict_unauthenticated.contains(area);
    json!({
        "timelines": {
            "local": restricted("timelines_local"),
            "federated": restricted("timelines_federated"),
            "remote": restricted("timelines_remote"),
        },
        "profiles": {
            "local": restricted("profiles_local"),
            "remote": restricted("profiles_remote"),
        },
        "activities": {
            "local": restricted("activities_local"),
            "remote": restricted("activities_remote"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry holds exactly the ticket's eight `:pleroma` keys, in
    /// order, and each one resolves through [`find`].
    #[test]
    fn the_registry_carries_the_whitelisted_keys() {
        let keys: Vec<&str> = entries().iter().map(|entry| entry.key).collect();
        assert_eq!(
            keys,
            vec![
                KEY_INSTANCE,
                KEY_MRF,
                KEY_MRF_SIMPLE,
                KEY_MRF_KEYWORD,
                KEY_MRF_HELLTHREAD,
                KEY_MRF_OBJECT_AGE,
                KEY_MEDIA_PROXY,
                KEY_RESTRICT_UNAUTHENTICATED,
            ],
            "the ticket's whitelist, in the GET order"
        );
        assert!(
            entries().iter().all(|entry| entry.group == GROUP_PLEROMA),
            "every entry lives in :pleroma"
        );
        for entry in entries() {
            let found = find(entry.group, entry.key).expect("every registered pair resolves");
            assert_eq!(found.key, entry.key, "{} resolves to itself", entry.key);
        }
    }

    /// `find` refuses everything the whitelist does not carry, and the
    /// refusal is what the API answers 400 for.
    #[test]
    fn unregistered_pairs_find_nothing() {
        assert!(find(GROUP_PLEROMA, ":feeds").is_none(), "not whitelisted");
        assert!(
            find(":wired", KEY_INSTANCE).is_none(),
            "wrong group is no pair"
        );
        assert!(
            find(GROUP_PLEROMA, "instance").is_none(),
            "the colon spelling is part of the key"
        );
        assert!(
            !requires_restart(GROUP_PLEROMA, ":nope"),
            "an unregistered pair restarts nothing"
        );
    }

    /// `:instance` and `:mrf` are subkey-whitelisted; the whole-value
    /// entries are bare, and only the two entries no runtime consumer
    /// reads are flagged for a restart.
    #[test]
    fn subkeys_and_restart_flags_follow_the_ticket() {
        let instance = find(GROUP_PLEROMA, KEY_INSTANCE).expect("registered");
        assert_eq!(
            instance.subkeys,
            &[
                "name",
                "description",
                "email",
                "registrations_open",
                "account_approval_required",
                "account_activation_required",
                "invites_enabled",
                "limit",
                "max_media_attachments",
            ],
            "the nine :instance subkeys"
        );
        let mrf = find(GROUP_PLEROMA, KEY_MRF).expect("registered");
        assert_eq!(mrf.subkeys, &["policies"], ":mrf gates its one subkey");
        for key in [
            KEY_MRF_SIMPLE,
            KEY_MRF_KEYWORD,
            KEY_MRF_HELLTHREAD,
            KEY_MRF_OBJECT_AGE,
        ] {
            let entry = find(GROUP_PLEROMA, key).expect("registered");
            assert!(entry.subkeys.is_empty(), "{key} is a whole value");
            assert!(
                !entry.requires_restart,
                "{key} takes effect without a restart"
            );
        }
        assert!(
            requires_restart(GROUP_PLEROMA, KEY_MEDIA_PROXY),
            "the media proxy shape is read at boot"
        );
        assert!(
            requires_restart(GROUP_PLEROMA, KEY_RESTRICT_UNAUTHENTICATED),
            "and the restriction set is read from the environment"
        );
        assert!(
            !requires_restart(GROUP_PLEROMA, KEY_INSTANCE)
                && !requires_restart(GROUP_PLEROMA, KEY_MRF),
            "the wired consumers need no restart"
        );
    }

    /// The `:instance` fallback spells the nine whitelisted subkeys off
    /// the environment-derived config defaults.
    #[test]
    fn instance_defaults_carry_the_whitelisted_subkeys() {
        let config = Config::for_tests();
        let Value::Object(map) = instance_defaults(&config) else {
            panic!("the defaults are a keyword object");
        };
        assert_eq!(
            map.len(),
            instance_keys().len(),
            "exactly the whitelisted subkeys"
        );
        for subkey in instance_keys() {
            assert!(map.contains_key(*subkey), "{subkey} defaults present");
        }
        assert_eq!(map["name"], json!(config.instance_name), "name from env");
        assert_eq!(map["email"], json!(""), "unset email is empty");
        assert_eq!(map["registrations_open"], json!(true), "the env default");
        assert_eq!(
            map["limit"],
            json!(config.max_status_chars),
            "limit is the status length"
        );
    }

    /// The whitelist's `:instance` subkey list and the defaults' key set
    /// are the same nine names, stated once.
    fn instance_keys() -> &'static [&'static str] {
        find(GROUP_PLEROMA, KEY_INSTANCE)
            .expect("registered")
            .subkeys
    }

    /// The `:mrf` fallback is the `MRF_POLICIES` list; the bare entries
    /// fall back to the empty object, and the two derived ones mirror
    /// their env settings.
    #[test]
    fn the_other_defaults_follow_their_environment() {
        let mut config = Config::for_tests();
        config.mrf.policies = vec!["simple".to_owned()];
        assert_eq!(
            mrf_defaults(&config),
            json!({"policies": ["simple"]}),
            "the env chain"
        );
        assert_eq!(
            empty_defaults(&config),
            json!({}),
            "a bare entry defaults to nothing configured"
        );
        config.media_proxy.enabled = true;
        config.media_proxy.blacklist = vec!["evil.example".to_owned()];
        let Value::Object(proxy) = media_proxy_defaults(&config) else {
            panic!("the media proxy defaults are a keyword object");
        };
        assert_eq!(proxy["enabled"], json!(true));
        assert_eq!(proxy["blacklist"], json!(["evil.example"]));
        assert!(
            !proxy.contains_key("secret"),
            "the signing secret is never part of the store"
        );
        config.restrict_unauthenticated = std::iter::once("timelines_local".to_owned()).collect();
        let restricted = restrict_unauthenticated_defaults(&config);
        assert_eq!(
            restricted["timelines"]["local"],
            json!(true),
            "the flat name regroups under its half"
        );
        assert_eq!(
            restricted["timelines"]["federated"],
            json!(false),
            "and the siblings stay open"
        );
    }

    /// The `GET /config/descriptions` payload: one documented object per
    /// registry entry in table order, the `:instance` element spelled
    /// like the ticket's example, leaves without example values
    /// rendering no `suggestions` field, and every subkey-whitelisted
    /// entry documenting exactly its whitelist's children.
    #[test]
    fn descriptions_document_every_whitelisted_key() {
        let rendered = serde_json::to_value(descriptions()).expect("the payload serializes");
        let Value::Array(objects) = &rendered else {
            panic!("the payload is an array");
        };
        assert_eq!(objects.len(), entries().len(), "one object per entry");
        let keys: Vec<&str> = objects
            .iter()
            .map(|object| {
                assert_eq!(
                    object["group"],
                    json!(":pleroma"),
                    "every entry is :pleroma"
                );
                assert!(
                    object["description"]
                        .as_str()
                        .is_some_and(|text| !text.is_empty()),
                    "no entry goes undocumented"
                );
                object["key"].as_str().expect("the key is a string")
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                KEY_INSTANCE,
                KEY_MRF,
                KEY_MRF_SIMPLE,
                KEY_MRF_KEYWORD,
                KEY_MRF_HELLTHREAD,
                KEY_MRF_OBJECT_AGE,
                KEY_MEDIA_PROXY,
                KEY_RESTRICT_UNAUTHENTICATED,
            ],
            "the ticket's order, the ticket's eight keys"
        );
    }

    /// The ticket's example element in full: `:instance` as a `group`
    /// whose children carry `key`, `type`, `description` and the
    /// `suggestions` hint only where the docs have one.
    #[test]
    fn the_instance_element_is_the_tickets_example() {
        let rendered = serde_json::to_value(descriptions()).expect("the payload serializes");
        let Value::Array(objects) = &rendered else {
            panic!("the payload is an array");
        };
        let instance = &objects[0];
        assert_eq!(instance["key"], json!(KEY_INSTANCE), "the example key");
        assert_eq!(instance["type"], json!("group"), "a documented container");
        assert_eq!(
            instance["description"],
            json!("Instance-related settings"),
            "Pleroma's `config/description.exs` wording"
        );
        let Value::Array(children) = &instance["children"] else {
            panic!("children is an array");
        };
        assert_eq!(children.len(), 9, "one child per whitelisted subkey");
        assert_eq!(children[0]["key"], json!("name"), "the example child");
        assert_eq!(children[0]["type"], json!("string"), "its type");
        assert_eq!(
            children[0]["description"],
            json!("Name of the instance"),
            "and its description"
        );
        assert_eq!(
            children[0]["suggestions"],
            json!(["Wired"]),
            "the example value, as the ticket's example shows one"
        );
        let registrations = &children[3];
        assert_eq!(registrations["key"], json!("registrations_open"));
        assert_eq!(registrations["type"], json!("boolean"), "booleans type");
        assert!(
            registrations.get("suggestions").is_none(),
            "a leaf without hints renders no suggestions field"
        );
    }

    /// The children track the whitelist: every subkey-whitelisted entry
    /// documents exactly those subkeys; the whole-value policy entries
    /// document their parameters, and the `policies` chain names the
    /// registry's own type spelling `[:atom]` with the wired policies as
    /// suggestions.
    #[test]
    fn children_follow_the_subkeys_and_the_policies() {
        for entry in entries() {
            let Value::Array(children) =
                serde_json::to_value(entry.children).expect("children serialize")
            else {
                panic!("children are an array");
            };
            assert!(!children.is_empty(), "{} carries child metadata", entry.key);
            if entry.subkeys.is_empty() {
                continue;
            }
            let documented: Vec<&str> = children
                .iter()
                .map(|child| child["key"].as_str().expect("a string key"))
                .collect();
            assert_eq!(
                documented, entry.subkeys,
                "{} documents its whitelisted subkeys",
                entry.key
            );
        }
        let mrf = find(GROUP_PLEROMA, KEY_MRF).expect("registered");
        assert_eq!(mrf.kind, ValueKind::Group, ":mrf is a documented group");
        let policies = &mrf.children[0];
        let rendered = serde_json::to_value(policies).expect("the child serializes");
        assert_eq!(
            rendered["type"],
            json!("[:atom]"),
            "the list-of-names spelling"
        );
        let suggestions = rendered["suggestions"].as_array().expect("suggestions");
        assert!(
            suggestions.contains(&json!("simple")) && suggestions.contains(&json!("keyword")),
            "the wired policy names are offered as examples"
        );
    }

    /// The nested `:map` documentation survives to the payload:
    /// `:restrict_unauthenticated` groups its halves, and each half
    /// nests its own booleans.
    #[test]
    fn nested_maps_carry_their_own_children() {
        let rendered = serde_json::to_value(descriptions()).expect("the payload serializes");
        let Value::Array(objects) = &rendered else {
            panic!("the payload is an array");
        };
        let restricted = &objects[7];
        assert_eq!(restricted["key"], json!(KEY_RESTRICT_UNAUTHENTICATED));
        assert_eq!(restricted["type"], json!("group"));
        let Value::Array(halves) = &restricted["children"] else {
            panic!("the halves are an array");
        };
        assert_eq!(halves.len(), 3, "timelines, profiles, activities");
        assert_eq!(halves[0]["key"], json!("timelines"));
        assert_eq!(halves[0]["type"], json!("map"), "a map of booleans");
        let Value::Array(fields) = &halves[0]["children"] else {
            panic!("the map nests its fields");
        };
        assert_eq!(fields.len(), 3, "local, federated and remote");
        assert_eq!(fields[0]["type"], json!("boolean"));
        assert!(
            halves[1].get("children").is_some() && halves[2].get("children").is_some(),
            "every documented container renders its children"
        );
    }

    /// The `type` strings stay inside the ticket's vocabulary everywhere
    /// in the payload, at every nesting depth.
    #[test]
    fn every_type_is_a_ticket_spelling() {
        const VOCABULARY: [&str; 7] = [
            "string", "boolean", "integer", "[:atom]", "keyword", "map", "group",
        ];
        let rendered = serde_json::to_value(descriptions()).expect("the payload serializes");
        let Value::Array(objects) = &rendered else {
            panic!("the payload is an array");
        };
        let mut seen = Vec::new();
        for object in objects {
            seen.push(object["type"].as_str().expect("every entry has a type"));
            collect_child_types(&object["children"], &mut seen);
        }
        for name in &seen {
            assert!(VOCABULARY.contains(name), "{name} is not a ticket type");
        }
        assert_eq!(
            seen.first().copied(),
            Some("group"),
            "the example entry types as the example spells it"
        );
    }

    /// Walks one `children` array, adding each `type` to `seen`.
    fn collect_child_types<'a>(children: &'a Value, seen: &mut Vec<&'a str>) {
        if let Value::Array(items) = children {
            for child in items {
                seen.push(child["type"].as_str().expect("every child has a type"));
                collect_child_types(&child["children"], seen);
            }
        }
    }
}
