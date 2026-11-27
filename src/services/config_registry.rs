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
//! these nine keys, all in group `:pleroma`:
//!
//! - `:instance` — the instance's public identity and registration gates,
//!   whitelist-subkeyed to `name`, `description`, `email`,
//!   `registrations_open`, `account_approval_required`,
//!   `account_activation_required`, `invites_enabled`, `limit` and
//!   `max_media_attachments`;
//! - `:mrf` — the moderation pipeline, whitelist-subkeyed to `policies`;
//! - `:mrf_simple`, `:mrf_keyword`, `:mrf_hellthread`, `:mrf_object_age`
//!   and `:mrf_hashtag` — one whole parameter object per policy;
//! - `:oauth_token_ttl_secs` — the OAuth access-token lifetime per role,
//!   whitelist-subkeyed to `admin`, `bot` and `user` (wired-be's own key:
//!   Pleroma has one global `[:oauth2, :token_expires_in]`);
//! - `:frontend_configurations` — one arbitrary settings object per
//!   frontend, keyed by frontend name (`soapbox_fe`, `pleroma_fe`, …); a
//!   write replaces each named subkey's whole object.
//!
//! A key is whitelisted only while a runtime consumer reads its stored
//! row. `:media_proxy` and `:restrict_unauthenticated` are deliberately
//! absent: the media proxy and the anonymous-read restrictions come from
//! the environment alone, so accepting a row for them would report a
//! restart that changes nothing. The admin API refuses both as not
//! whitelisted.
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
/// The always-on `hashtag` policy's tag lists.
pub const KEY_MRF_HASHTAG: &str = ":mrf_hashtag";
/// The access-token lifetimes per role: the `admin`, `bot` and `user`
/// subkeys, in seconds, overriding `OAUTH_TOKEN_TTL_SECS_ADMIN`, `_BOT`
/// and `_USER` for the next token issued.
pub const KEY_OAUTH_TOKEN_TTL_SECS: &str = ":oauth_token_ttl_secs";
/// The frontend settings, one object per frontend (`:pleroma_fe`,
/// `:soapbox_fe`, …), served at `GET /api/pleroma/frontend_configurations`.
pub const KEY_FRONTEND_CONFIGURATIONS: &str = ":frontend_configurations";
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
