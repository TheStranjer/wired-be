//! Integration tests for `PATCH /api/v1/accounts/update_credentials`
//! (WBE-67, the profile-text-fields ticket, extended by WBE-69, the
//! profile-metadata ticket, by the profile-images ticket, and by this
//! ticket's posting defaults and Pleroma settings): the fields this slice
//! of the endpoint owns — `display_name` (length-capped against
//! `MAX_DISPLAY_NAME_CHARS`, sanitized, empty stored as empty),
//! `note` (length-capped against `MAX_BIO_CHARS`, stored raw in `raw_bio`
//! and rendered to `bio` with URL, hashtag and local-mention links), the
//! `locked` / `bot` / `discoverable` booleans, `fields_attributes` (the
//! profile metadata: every body shape, the `MAX_PROFILE_FIELDS` count cap,
//! the per-entry name and value character caps, both-empty entries dropped,
//! empty clearing, raw values kept beside the rendered HTML), the three
//! profile images — `avatar`, `header` and `pleroma_background_image` file
//! parts stored through the uploader and served over `/media`, replaced
//! images whose file goes away, the empty-string and JSON-`null` clears,
//! the `AVATAR_LIMIT_BYTES` / `BANNER_LIMIT_BYTES` caps and the
//! `png`/`jpeg`/`gif`/`webp` allowlist, both 422s — and, from this ticket,
//! the posting defaults of the `source` block (`source[privacy]` as the
//! nested object or Pleroma's flat `default_scope`, `source[sensitive]`,
//! `source[language]` with its length cap and its clearing empty string),
//! the eleven Pleroma preference flags, `birthday` (set, cleared, and both
//! 422s), `pleroma_settings_store` (the shallow key-by-key merge, the
//! JSON-text spelling of a form body, the non-object and over-1-MiB
//! refusals) and `actor_type` (the `Person`/`Service` pair that keeps `bot`
//! in step) — each updated individually through JSON, form and multipart
//! bodies, with absent fields left untouched, invalid boolean strings and
//! over-long texts answered 422, the 200 answer the owner entity with its
//! `source` block and its `pleroma` block reflecting the new values (the
//! `verify_credentials` render), the route behind `AuthUser` and
//! `write:accounts` (401 unauthenticated, 403 for the wrong credential),
//! the `POST` alias serving the old clients, and the profile-federation
//! hook: one accepted write queues the actor document as an `Update` under
//! the per-account debounce key — a burst of writes pays for one queued
//! activity — and a row without a followers collection queues nothing.
//!
//! The tests live in submodules by field family; `support` holds the consts,
//! request verbs and fixtures they share.

#![cfg(test)]

/// The `display_name` and `note` fields.
mod text_fields;

/// The boolean flags, absent and null fields, the auth gate, the `POST`
/// alias and the ride-along file parts.
mod flags_and_basics;

/// The `fields_attributes` profile metadata.
mod profile_fields;

/// The caps on `fields_attributes`.
mod profile_fields_limits;

/// The three profile images.
mod images;

/// The posting defaults, the Pleroma preference flags and `birthday`.
mod source_and_prefs;

/// The settings store, `actor_type`, the response and the publish hook.
mod store_actor_publish;

/// Shared consts, request verbs and fixtures.
mod support;
