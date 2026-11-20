//! The admin foundation over HTTP (`/api/v1/pleroma/admin`, WBE-194): the
//! [`AdminUser`] and [`StaffUser`] extractors, the moderation-log listing
//! and its filters and pagination, and the visibility `stats`, all against
//! the real router (plus a two-route test router that exercises the
//! [`StaffUser`] gate in isolation; its production routes joined with the
//! admin reports surface, WBE-200). The admin config store (WBE-206) —
//! `GET`/`POST /config` over the whitelist registry, the `:mrf`/`:instance`
//! consumers and the `config_change` log line, its
//! `GET /config/descriptions` payload and the `GET /need_reboot` /
//! `GET /restart` pair (WBE-207) — joins beside it, and with it the admin
//! invites (WBE-199): `users/invite_token`, `users/invites`,
//! `users/revoke_invite` and `users/email_invite`, their `invite_create`
//! and `invite_revoke` log lines, and the `GET /registration/{token}`
//! redirect the invitation letters carry; and the webhook maintenance API
//! (WBE-209) too — `/webhooks` CRUD, the enable / disable / rotate_secret
//! actions, their `{"errors": …}` validations, the internal protection and
//! the `webhook_*` log lines.
//!
//! The extractor matrix follows Pleroma's `UserIsAdminPlug` /
//! `UserIsStaffPlug` order — a missing token is 401, an unusable one
//! 401/403, a valid non-staff token 403 naming the role, and only then the
//! `admin:read` scope gate. The log entries are seeded through the model
//! helpers (the write endpoints arrive with the later admin tickets), and
//! time is made controllable by back-dating `inserted_at` rather than
//! sleeping.
//!
//! The area lives below `tests/api/admin/`, one submodule per section:
//! `support` holds the consts and staff fixtures every section draws on;
//! `gates` runs the extractor matrix and the production routes' admin
//! demand; `moderation_log` the log listing and `stats`; `config_writes`,
//! `config_consumers`, `config_log`, `config_gates` and
//! `config_descriptions` the config store, its live consumers, its log
//! line, its gates and its descriptions and reboot pair; `invites`,
//! `invite_email` and `invite_gates` the invite surface, its letter and
//! its gates; and `webhooks_crud`, `webhooks_validation` and
//! `webhooks_internal_and_gates` the webhook API and its gates.
//!
//! [`AdminUser`]: wired_be::auth::admin::AdminUser
//! [`StaffUser`]: wired_be::auth::admin::StaffUser

#![cfg(test)]

/// The live consumers: the `:mrf*` chain, the `:instance` overrides and
/// the `need_reboot` flag following the registry.
mod config_consumers;
/// The descriptions payload and the reboot pair's successful paths.
mod config_descriptions;
/// The config store's own read/write gate and the reboot pair's gate.
mod config_gates;
/// The config store's `config_change` log line and the silence of the
/// batches that changed nothing.
mod config_log;
/// The `GET`/`POST /config` round-trips over the whitelist registry.
mod config_writes;
/// The extractor matrix and the admin demand on every production route.
mod gates;
/// The emailed invite, its refusals and the registration-link redirect.
mod invite_email;
/// The invite routes' scope, role and credential gates.
mod invite_gates;
/// The invite create, listing, revocation and their moderation-log lines.
mod invites;
/// The moderation-log listing, its filters and windows, and the `stats`.
mod moderation_log;
/// Shared consts and staff fixtures every submodule of this area draws on.
mod support;
/// The webhook create, patch and delete round trip.
mod webhooks_crud;
/// The internal-webhook reads and protection, the listing window, the
/// `webhook_*` log lines and the webhook gates.
mod webhooks_internal_and_gates;
/// The webhook `{id}` refusals and the enable / disable / rotate actions.
mod webhooks_validation;
