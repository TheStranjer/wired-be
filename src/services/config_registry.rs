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

mod children;
mod registry;
mod types;

pub use self::{
    registry::{descriptions, entries, find, requires_restart},
    types::{ChildDescription, EntryDescription, RegistryEntry, Suggestion, ValueKind},
};
