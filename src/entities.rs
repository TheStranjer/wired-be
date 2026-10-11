//! API JSON serializers: the Mastodon "entities".
//!
//! Per `docs/ARCHITECTURE.md` decision 3 each module here owns one
//! response shape: a `serde::Serialize` struct built from the
//! [`models`](crate::models) rows — plus the instance
//! [`Config`](crate::config::Config) wherever URLs or the domain are
//! needed — by pure functions. Nothing here touches the database or a
//! request, so every entity is testable with `serde_json::to_value`
//! alone. [`account`] renders the Mastodon `Account` entity,
//! [`media_attachment`] the Mastodon `Attachment` entity and
//! [`custom_emoji`] the Mastodon `CustomEmoji` entity.

pub mod account;
pub mod custom_emoji;
pub mod media_attachment;
