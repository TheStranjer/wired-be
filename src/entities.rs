//! API JSON serializers: the Mastodon "entities".
//!
//! Per `docs/ARCHITECTURE.md` decision 3 each module here owns one
//! response shape: a `serde::Serialize` struct built from the
//! [`models`](crate::models) rows — plus the instance
//! [`Config`](crate::config::Config) wherever URLs or the domain are
//! needed — by pure functions. Nothing here touches the database or a
//! request, so every entity is testable with `serde_json::to_value`
//! alone. [`account`] renders the Mastodon `Account` entity,
//! [`admin_account`] the admin API's account entity — the moderation
//! console's row behind `GET /api/v1/pleroma/admin/users` and its show
//! endpoint — [`mastodon_admin`] the Mastodon admin API's account entity,
//! the profile-plus-private-columns row behind
//! `GET /api/v{1,2}/admin/accounts` —
//! [`admin_report`] the admin API's report entity — the
//! moderation console's view of a filed report behind
//! `GET /api/v1/pleroma/admin/reports` — [`mastodon_admin_report`] the
//! Mastodon admin API's report entity, the action-taking console's view
//! of a filed report behind `GET /api/v1/admin/reports` —
//! [`mastodon_admin_report`] the Mastodon admin API's report entity, the
//! action-taking console's view of a filed report behind
//! `GET /api/v1/admin/reports` —
//! [`announcement`] the Mastodon `Announcement` entity,
//! [`application`] the Mastodon `Application` entity,
//! [`media_attachment`] the Mastodon `Attachment` entity,
//! [`custom_emoji`] the Mastodon `CustomEmoji` entity, [`filter`] the
//! Mastodon v1 `Filter` entity, [`list`] the Mastodon v1 `List` entity,
//! [`chat`] the
//! Pleroma chat `Chat` and `ChatMessage` entities, [`conversation`] the
//! Mastodon `Conversation` entity, [`marker`] the
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
//! [`crate::services::translate`] provider answer. [`bookmark_folder`]
//! Pleroma's `BookmarkFolder` entity, whose `emoji_url` the endpoint
//! resolves from `custom_emojis` before the pure render. [`backup`]
//! Pleroma's `Backup` entity, whose download `url` the endpoint resolves
//! from the uploader before the pure render. [`status_edit`] one
//! element of the `GET /api/v1/statuses/{id}/history` answer: a status
//! version, superseded or current. [`scheduled_status`] the Mastodon
//! `ScheduledStatus` entity: one parked create-status request, its params
//! jsonb re-keyed the way Pleroma's `ScheduledActivityView.status_params`
//! renders them — unfilled keys echoed as `null` — beside the rendered
//! attachments of its own row.

pub mod account;
pub mod admin;
pub mod admin_account;
pub mod admin_report;
pub mod announcement;
pub mod application;
pub mod backup;
pub mod bookmark_folder;
pub mod chat;
pub mod conversation;
pub mod custom_emoji;
pub mod emoji_reaction;
pub mod filter;
pub mod list;
pub mod marker;
pub mod mastodon_admin;
pub mod mastodon_admin_report;
pub mod media_attachment;
pub mod mention;
pub mod notification;
pub mod poll;
pub mod push_subscription;
pub mod relationship;
pub mod report;
pub mod scheduled_status;
pub mod scrobble;
pub mod status;
pub mod status_edit;
pub mod tag;
pub mod translation;
