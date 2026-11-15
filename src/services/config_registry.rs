//! The whitelist registry of runtime-configurable settings: the
//! `(group, key)` pairs the admin config store accepts, beside what each
//! one falls back to when the database holds no row.
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

/// One whitelisted runtime setting: what it is called, which subkeys the
/// whitelist enumerates for it, whether changing it needs a restart, and
/// where its value comes from while the database holds no row.
#[derive(Debug)]
pub struct RegistryEntry {
    /// The configuration group, [`GROUP_PLEROMA`] for every entry.
    pub group: &'static str,
    /// The configuration key, colon-spelled as the admin API names it.
    pub key: &'static str,
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

/// The whole registry: the eight entries the ticket whitelists.
static REGISTRY: [RegistryEntry; 8] = [
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_INSTANCE,
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
        subkeys: &["policies"],
        requires_restart: false,
        defaults: mrf_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_MRF_SIMPLE,
        subkeys: &[],
        requires_restart: false,
        defaults: empty_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_MRF_KEYWORD,
        subkeys: &[],
        requires_restart: false,
        defaults: empty_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_MRF_HELLTHREAD,
        subkeys: &[],
        requires_restart: false,
        defaults: empty_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_MRF_OBJECT_AGE,
        subkeys: &[],
        requires_restart: false,
        defaults: empty_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_MEDIA_PROXY,
        subkeys: MEDIA_PROXY_SUBKEYS,
        requires_restart: true,
        defaults: media_proxy_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_RESTRICT_UNAUTHENTICATED,
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
}
