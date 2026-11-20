//! The Mastodon `Account` entity: its core fields, `source` object and
//! `pleroma` block.
//!
//! The reference shape is Pleroma's `AccountView.do_render("show.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/account_view.ex`); the JSON keys are
//! emitted exactly as that view names them, restricted to the core fields
//! and the Pleroma extension. [`Account::from_user`] is pure (decision 3):
//! it renders a [`User`] row and the instance [`Config`] — no database, so
//! remote users' counts come from the stored counters and nothing is
//! recomputed per request.
//!
//! [`Account::source`] is the one key not every render answers with:
//! Pleroma puts the editing block in only when the viewer is the account
//! itself, so it is an `Option` that `skip_serializing_if` hides and only
//! [`Account::for_owner`] — the `verify_credentials` and
//! `update_credentials` render — sets. Deliberately absent here:
//! `hide_collections`.
//!
//! Remote users' `avatar`, `avatar_static`, `header` and `header_static`
//! pass through the media proxy helper
//! ([`proxy_url`](crate::services::media_proxy::proxy_url)): with
//! `MEDIA_PROXY_ENABLED` their remote image URLs become signed proxy
//! links; local users, instance defaults and a disabled proxy all render
//! the URL as stored.
//!
//! The `pleroma` block is [`PleromaMetadata`]: it renders the Pleroma
//! columns of `users` (see the `users_pleroma_extensions` migration), and
//! the seven keys Pleroma only sends to the account's own user are gated
//! on the *viewer* — the account making the request, Mastodon's
//! `AccountViewer`. Pass it with [`Account::from_user_for`];
//! [`Account::from_user`] renders the anonymous view.
//!
//! Two privacy rules follow the viewer too, as in Pleroma's `show.json`
//! renderer: a hidden follow/follower count renders as `0` for everyone
//! but the owner, and the admin/moderator flags render `false` for
//! everyone but the owner when the account asked not to show its role.
//!
//! This module is a tree. `source` holds the `source` object with its
//! Pleroma half and the [`AccountViewer`] type; `pleroma` the
//! [`PleromaMetadata`] block and the column rules behind its keys;
//! [`Account`] itself the `entity` child; `entity_render` the `impl` block
//! with the pure renders; `helpers` the privacy rules and URL builders
//! those share; and `test_support` the unit-test fixtures the children
//! use. Every item is re-exported here, so an outside path stays
//! `crate::entities::account::…`.

use serde::Serialize;

#[cfg(doc)]
use crate::{config::Config, models::user::User};

mod entity;
mod entity_render;
pub mod helpers;
mod pleroma;
mod source;

#[cfg(test)]
mod test_support;

pub(crate) use self::helpers::{full_nickname, nickname_part, profile_image};
pub use self::{
    entity::Account,
    helpers::local_url,
    pleroma::PleromaMetadata,
    source::{AccountSource, AccountViewer, MuteExpiresAt, SourcePleroma},
};

/// The `actor_type` value that makes the entity's `group` flag true.
const GROUP_ACTOR_TYPE: &str = "Group";

/// Path appended to [`Config::frontend_base_url`] for the default avatar,
/// matching Pleroma's `[:assets, :default_user_avatar]` fallback
/// (`lib/pleroma/user.ex`, `avatar_url/2`). Crate-visible because the
/// `push.deliver` job renders the same fallback into a payload's `icon`
/// (see [`crate::jobs::web_push`]).
pub(crate) const DEFAULT_AVATAR_PATH: &str = "/images/avi.png";

/// Path appended to [`Config::frontend_base_url`] for the default banner,
/// matching Pleroma's `banner_url/2` fallback.
pub(crate) const DEFAULT_BANNER_PATH: &str = "/images/banner.png";

/// Format of `last_status_at`: a bare `YYYY-MM-DD` date, the counterpart
/// of Pleroma's `NaiveDateTime.to_date() |> Date.to_iso8601()`.
const DATE_FORMAT: &str = "%Y-%m-%d";

/// A JSON boolean field of an [`Account`].
///
/// `Account` carries six booleans, past the denied `struct_excessive_bools`
/// threshold, so each is wrapped in this newtype; serde serializes a
/// newtype struct as its inner value, so on the wire these stay plain
/// `true`/`false`.
#[derive(Clone, Debug, Serialize)]
pub struct JsonBool(pub bool);

/// One profile metadata line, as Mastodon renders it: a `{name, value}`
/// pair of the stored jsonb plus a verification time.
///
/// The same shape serves both renderings: the top-level
/// [`fields`](Account::fields) carry the `fields` jsonb's sanitized-HTML
/// value, while [`AccountSource::fields`] hands the owner the `raw_fields`
/// jsonb's editing text.
#[derive(Debug, Serialize)]
pub struct Field {
    /// The field label, as the user typed it.
    pub name: String,
    /// The field contents: sanitized HTML in the top-level entity, raw
    /// source text in [`AccountSource::fields`].
    pub value: String,
    /// When a linked `value` was verified against the account, in Mastodon
    /// timestamp format; nothing verifies links yet, so this is always
    /// JSON `null`.
    pub verified_at: Option<String>,
}
