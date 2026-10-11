//! API JSON serializers: the Mastodon "entities".
//!
//! Per `docs/ARCHITECTURE.md` decision 3 each module here owns one
//! response shape: a `serde::Serialize` struct built from the
//! [`models`](crate::models) rows — plus the instance
//! [`Config`](crate::config::Config) wherever URLs or the domain are
//! needed — by pure functions. Nothing here touches the database or a
//! request, so every entity is testable with `serde_json::to_value`
//! alone. [`account`] renders the Mastodon `Account` entity,
//! [`announcement`] the Mastodon `Announcement` entity,
//! [`media_attachment`] the Mastodon `Attachment` entity,
//! [`custom_emoji`] the Mastodon `CustomEmoji` entity, [`filter`] the
//! Mastodon v1 `Filter` entity, [`list`] the Mastodon v1 `List` entity,
//! [`chat`] the
//! Pleroma chat `Chat` and `ChatMessage` entities, [`marker`] the
//! Mastodon v1 `Marker` entity and [`relationship`]
//! the Mastodon `Relationship` entity (with Pleroma's `subscribing`
//! alias), which [`models::relationship`](crate::models::relationship)
//! batch-loads.

pub mod account;
pub mod announcement;
pub mod chat;
pub mod custom_emoji;
pub mod filter;
pub mod list;
pub mod marker;
pub mod media_attachment;
pub mod relationship;
