//! The registry itself: the whitelisted entries, their lookups and the
//! environment-derived fallbacks.

use serde_json::{Map, Value, json};

use super::{
    GROUP_PLEROMA, INSTANCE_LIMIT, KEY_INSTANCE, KEY_MRF, KEY_MRF_HASHTAG, KEY_MRF_HELLTHREAD,
    KEY_MRF_KEYWORD, KEY_MRF_OBJECT_AGE, KEY_MRF_SIMPLE, KEY_OAUTH_TOKEN_TTL_SECS,
    children::{
        INSTANCE_CHILDREN, MRF_CHILDREN, MRF_HASHTAG_CHILDREN, MRF_HELLTHREAD_CHILDREN,
        MRF_KEYWORD_CHILDREN, MRF_OBJECT_AGE_CHILDREN, MRF_SIMPLE_CHILDREN,
        OAUTH_TOKEN_TTL_CHILDREN,
    },
    types::{EntryDescription, RegistryEntry, ValueKind},
};
use crate::config::Config;

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

/// The registry, in the order `GET /config` without `only_db` answers:
/// the identity first, then the moderation family.
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

/// The whole registry: the eight entries the admin API may change, each one
/// read at runtime.
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
        key: KEY_MRF_HASHTAG,
        kind: ValueKind::Keyword,
        description: "Hashtag policy: the tag lists it marks, unlists and rejects by",
        children: &MRF_HASHTAG_CHILDREN,
        subkeys: &[],
        requires_restart: false,
        defaults: empty_defaults,
    },
    RegistryEntry {
        group: GROUP_PLEROMA,
        key: KEY_OAUTH_TOKEN_TTL_SECS,
        kind: ValueKind::Group,
        description: "OAuth access-token lifetime in seconds, per role of the token's user",
        children: &OAUTH_TOKEN_TTL_CHILDREN,
        subkeys: &["admin", "bot", "user"],
        requires_restart: false,
        defaults: token_ttl_defaults,
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

/// The `:oauth_token_ttl_secs` fallback: the three tier lifetimes the
/// `OAUTH_TOKEN_TTL_SECS*` variables set.
fn token_ttl_defaults(config: &Config) -> Value {
    let ttls = config.oauth_token_ttls;
    json!({ "admin": ttls.admin, "bot": ttls.bot, "user": ttls.user })
}

/// The fallback of the entries with no environment counterpart: the empty
/// keyword list, which on the wire is the empty tuple array.
fn empty_defaults(_config: &Config) -> Value {
    Value::Object(Map::new())
}

#[cfg(test)]
mod tests;
