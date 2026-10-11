//! API JSON serializers: the Mastodon "entities".
//!
//! Per `docs/ARCHITECTURE.md` decision 3 each module here owns one
//! response shape: a `serde::Serialize` struct built from the
//! [`models`](crate::models) rows by pure functions. Nothing here touches
//! the database or a request, so every entity is testable with
//! `serde_json::to_value` alone. [`application`] renders the Mastodon
//! `Application` entity.

pub mod application;
