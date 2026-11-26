//! The Mastodon admin accounts surface over HTTP
//! (`/api/v{1,2}/admin/accounts`): the WBE-210 reads and the WBE-213 writes,
//! one test area split across the submodules below.
//!
//! The reads are Pleroma's `MastodonAPI.Admin.AccountController` `index`,
//! `index2` and `show` — the sixteen-key entity with its role object, the v1
//! boolean and v2 word spellings of the same criteria, the criteria this
//! schema cannot weigh and accepts anyway, the page under a `Link` header
//! keyed on the account ids, the show route's two misses, and the admin-and-
//! `admin:read:accounts` gate in front of all of it.
//!
//! The shared fixture seeds one account per state a filter weighs — an admin, a
//! moderator, a plain active account, a deactivated one, one awaiting approval,
//! one awaiting its email check, a tagged `Service` and a remote mirror —
//! beside the instance actor, which the listings must never answer with. Every
//! test reads those rows through the real router, so the order the page comes
//! back in, the cursors the `Link` header names and the entity's keys are
//! asserted against stored facts rather than a hand-built row.
//!
//! The writes are `DELETE …/{id}`, the `enable`, `approve` and `reject`
//! suffixes and the umbrella `action`, each starting the same state moves the
//! Pleroma console starts, on a second fixture and behind the admin-and-
//! `admin:write:accounts` gate. `support` and `write_support` hold the two
//! halves' consts and helpers; the other submodules hold the tests.

#![cfg(test)]

/// The umbrella account action: the five types, the report resolution and
/// the refusals.
mod actions;
/// The approve and reject writes and their letters.
mod approve_reject;
/// The delete and enable writes and their misses.
mod delete_enable;
/// A disabled account stays public while its owner is locked out.
mod disabled_public;
/// The two read gates: the admin user and the accounts read scope.
mod read_gates;
/// The listings, the pinned entity, the page and the show route.
mod reads;
/// Suspended against disabled: the entities, the filters, the actor and the
/// two reversals.
mod standing;
/// The read-section consts, the shared fixture and the readers.
mod support;
/// The two write gates: the admin user and the accounts write scope.
mod write_gates;
/// The write-section consts, fixture, verbs and readers.
mod write_support;
