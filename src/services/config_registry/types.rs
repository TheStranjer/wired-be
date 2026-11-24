//! The registry's vocabulary: value kinds, suggestions, the description
//! payload types and the entry type with the const builders for children.

use serde::Serialize;
use serde_json::Value;

use crate::config::Config;

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
/// container may nest its own subkeys in [`children`]; no registered entry
/// does today.
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

/// One whitelisted runtime setting: what it is called, which subkeys the
/// whitelist enumerates for it, and whether changing it needs a restart.
///
/// Beside the gating data the entry carries its environment fallback
/// (`defaults`) and the documentation metadata (`description`, `kind`,
/// `children`) the admin API's `GET /config/descriptions` answers with.
#[derive(Debug)]
pub struct RegistryEntry {
    /// The configuration group, [`GROUP_PLEROMA`](super::GROUP_PLEROMA) for
    /// every entry.
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
    /// Whether a change only takes effect after a restart, what the admin
    /// API's `need_reboot` flag is computed from. `false` for every entry
    /// today: an entry nothing reads at runtime is not whitelisted at all.
    pub requires_restart: bool,
    /// The entry's environment-derived default, read off [`Config`].
    pub defaults: fn(&Config) -> Value,
}

/// A leaf description with no example values.
pub(super) const fn leaf(
    key: &'static str,
    kind: ValueKind,
    description: &'static str,
) -> ChildDescription {
    ChildDescription {
        key,
        kind,
        description,
        suggestions: &[],
        children: &[],
    }
}

/// A leaf description with example values.
pub(super) const fn hinted(
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
