//! Tests for the public instance endpoints (`src/api/instance.rs`):
//! `GET /api/v1/instance`, `GET /api/v2/instance`, and the
//! `/api/v1/instance` sub-resources `peers`, `rules`, `domain_blocks`,
//! `translation_languages`, `activity` and `extended_description`.
//!
//! These endpoints are public (Pleroma's `skip_auth`) and take no
//! parameters, so there is no auth failure, 404 or validation arm to
//! cover: what these tests own is the exact key set and value types of
//! the documents, the three cached counters against local/remote
//! fixtures, the priority-ordered rules, the contact account (present
//! and absent), the config mirroring through non-default settings, the
//! 60-second stats cache window, the distinct-and-capped peer list with
//! the rejected hosts kept out, the MRF transparency block — published
//! lists, exclusions, and the switch that closes both — the domain lists'
//! digest and severity mapping, and the empty answers of the completeness
//! endpoints. The cache helper's own expiry arithmetic is unit-tested
//! where it lives, in `src/util/cache.rs`.

#![cfg(test)]

mod federation;
mod sub_resources;
mod support;
mod v1;
mod v2_peers;
mod wired;
