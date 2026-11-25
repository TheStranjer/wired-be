//! Filters over HTTP (`/api/v1/filters`, WBE-98): the Mastodon v1 CRUD,
//! and (WBE-129) the v1 application of those filters to the rendered
//! listings.
//!
//! The CRUD tests pin what the ticket promises: create, read, list, replace
//! and delete round-trip the six-key `Filter` entity; `context[]` array keys
//! work in form bodies; an expired filter leaves the list but is still a
//! row its owner can read; a blank phrase and a bad context answer the
//! `{"error":"Validation failed: …"}` 422s; every route enforces its half
//! of the `AuthUser` + scope pipeline (`read:filters` to look,
//! `write:filters` to change); and an id that is malformed, unknown or
//! another account's is one and the same 404.
//!
//! The application tests under `services::filter`'s semantics: a warning
//! filter keeps its matches and sets the status entity's `filtered` array
//! to the v2 `FilterResult` shape; an `irreversible` one removes them from
//! the home, public, account and thread pages — and drops the whole
//! notification whose status it matched from the listing; whole-word and
//! context scoping, expiry, reblog originals and the viewer's own-status
//! exemption all decide the page, and an unauthenticated (or unfiltered)
//! read carries the empty array everywhere.
//!
//! And (WBE-191 slice 5) every successful write publishes Mastodon's
//! `filters_changed` onto the hub — the owner's `user:` stream, the owner's
//! audience, no payload — while a refused write publishes nothing.

#![cfg(test)]

/// The application of stored filters to the home, public, account,
/// notification and thread listings.
mod application;
/// The Mastodon v1 CRUD: the lifecycle round trip, the form bodies with
/// `context[]` array keys, the whole replacement on `PUT`, the expiry's
/// effect on the list, Mastodon's validation wording and the 404 that
/// malformation, a stranger's id and a missing row all answer.
mod crud;
/// The `filters_changed` events: what each successful write publishes onto
/// the hub and the silence every refusal keeps.
mod events;
/// The credential and scope half of the pipeline: the 401, the app-only
/// 403 and the `read:filters` / `write:filters` gates on every route.
mod gates;
/// Shared consts, fixtures, HTTP wrappers and seeds every submodule draws
/// on; everything here is `pub(super)`.
mod support;
/// v2 filters applied to rendered listings: annotation, hiding, contexts,
/// whole-word matching, expiry, status entries and the v1 view of the same
/// storage.
mod v2_apply;
/// The Mastodon v2 CRUD: filters, keywords and status entries, the scope
/// gates, the validation wording and the 404s.
mod v2_crud;
