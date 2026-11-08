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
//! [`application`] the Mastodon `Application` entity,
//! [`media_attachment`] the Mastodon `Attachment` entity,
//! [`custom_emoji`] the Mastodon `CustomEmoji` entity, [`filter`] the
//! Mastodon v1 `Filter` entity, [`list`] the Mastodon v1 `List` entity,
//! [`chat`] the
//! Pleroma chat `Chat` and `ChatMessage` entities, [`marker`] the
//! Mastodon v1 `Marker` entity, [`mention`] the Mastodon `Mention` entity,
//! [`scrobble`] the Pleroma `Scrobble` entity, [`tag`] the Mastodon
//! `Tag` entity
//! and [`relationship`]
//! the Mastodon `Relationship` entity (with Pleroma's `subscribing`
//! alias), which [`models::relationship`](crate::models::relationship)
//! batch-loads. [`status`] renders the Mastodon `Status` entity with
//! Pleroma's extension block; its batch loader is
//! [`services::render_status`](crate::services::render_status).
//! [`report`] renders the Mastodon `Report` entity from a
//! [`models::report`](crate::models::report) row and the reported account,
//! [`emoji_reaction`] Pleroma's two emoji-reaction shapes: the entry of a
//! status's `pleroma.emoji_reactions` array and the entry of its reactions
//! endpoint. [`notification`] one entry of the v2 grouped-notifications
//! answer and the `{accounts, statuses, notification_groups}` envelope
//! around it. [`translation`] the Mastodon `Translation` entity, built from a
//! [`crate::services::translate`] provider answer.

pub mod account;
pub mod announcement;
pub mod application;
pub mod chat;
pub mod custom_emoji;
pub mod emoji_reaction;
pub mod filter;
pub mod list;
pub mod marker;
pub mod media_attachment;
pub mod mention;
pub mod notification;
pub mod push_subscription;
pub mod relationship;
pub mod report;
pub mod scrobble;
pub mod status;
pub mod tag;
pub mod translation;
