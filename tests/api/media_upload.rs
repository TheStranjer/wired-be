//! Tests for the media upload endpoints: `POST /api/v1/media` and
//! `POST /api/v2/media` (`src/api/media.rs`). They cover the whole path a
//! client takes — a multipart body in, the `Attachment` entity out, and the
//! file then readable over the URLs the entity announced — plus every
//! rejection the endpoint owes: 401 and 403 for credentials and scopes, 413
//! over `UPLOAD_LIMIT_BYTES`, and 422 for a missing or empty file, a type
//! outside `image/*`, `video/*` and `audio/*`, an undecodable or oversized
//! image, or a malformed `focus`. The `GET /media/{key}` route those
//! announced URLs point at is covered by `tests/media.rs`.
//!
//! Uploads land in a [`TempDir`], never in the repository's own `./uploads`,
//! through the `common::test_app_with_*` helpers. Image fixtures are real,
//! decodable files built with the `image` crate — the endpoint now analyzes
//! `image/*` uploads, so a magic-bytes header is only good for bodies the
//! analysis never reaches (rejected earlier, or not an image). Audio and
//! video fixtures stay header-only: the detector names their type from the
//! magic bytes and nothing decodes them.

#![cfg(test)]

mod images;
mod limits_auth;
mod support;
mod timed;
mod validation;
mod versions;
