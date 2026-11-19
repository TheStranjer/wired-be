//! The admin users endpoints over HTTP (`/api/v1/pleroma/admin/users`):
//! Pleroma's `AdminAPI.UserController` `index` and `show`, the eight
//! `filters`, the `query`/`name`/`email`/`tags[]`/`actor_types[]` criteria,
//! the `page`/`page_size` window with its `count`, the admin account
//! entity, and the `admin:read:accounts` gate in front of all of it
//! (WBE-195); and beside them the credentials, security and follow
//! maintenance of WBE-198 — the `"credentials.json"` read, the changeset
//! PATCH with its 400 `{"errors": …}` field map, the issued and forced
//! password resets, the confirmation pair, the MFA removal and the forced
//! follow, every write gated `admin:write:accounts` and every one of them
//! (except the token mint) landing its moderation-log entry.
//!
//! The shared fixture seeds one account per state a filter weighs — an
//! admin, a moderator, a plain active account, a deactivated one, one
//! awaiting approval, one awaiting its email check, a tagged `Service` and
//! a remote mirror — beside the instance actor, which the listing must
//! never answer with. Every test reads the same rows through the real
//! router, so the order the ticket pins, the total that ignores the window
//! and the entity's fifteen keys are asserted against stored facts rather
//! than a hand-built row.
//!
//! The area lives below `tests/api/admin_users/`, one submodule per
//! section: `support` holds the fixture and the consts and helpers the
//! sections share; `listing` and `show` run the WBE-195 reads and their
//! gate; `credentials`, `credentials_write`, `maintenance_resets` and
//! `maintenance_security` the WBE-198 writes, with `route_gates` weighing
//! the gate over the whole set; `tags`, `permission_groups`,
//! `permission_groups_bulk` and `suggestions` the WBE-197 surface; and
//! `lifecycle_create` and `lifecycle_activation` the WBE-196 lifecycle.

#![cfg(test)]

/// The credentials read and the validating half of the write.
mod credentials;
/// The writing half of the credentials PATCH.
mod credentials_write;
/// The activation pair, the toggle, the approval and the whole gate.
mod lifecycle_activation;
/// The lifecycle create batch and the admin's delete.
mod lifecycle_create;
/// The listing: order, window, filters, query and named criteria.
mod listing;
/// The reset pair, the confirmation pair and their links.
mod maintenance_resets;
/// The MFA removal and the forced follow pair.
mod maintenance_security;
/// The group reads and the single-account grants.
mod permission_groups;
/// The bulk group writes, the mirror and the self-revoke.
mod permission_groups_bulk;
/// The gate matrix over every maintenance, role and credentials route.
mod route_gates;
/// The show route, the admin entity and the read gate.
mod show;
/// The suggestion pool and the pair that moves it.
mod suggestions;
/// Shared consts and fixture helpers every submodule of this area draws on.
mod support;
/// The profile-tag pair and its parameter spellings.
mod tags;
