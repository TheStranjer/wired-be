//! Domain types and database access.
//!
//! Per `docs/ARCHITECTURE.md` decisions 2 and 3 each module owns one table
//! (or one closely-coupled group of them, like the OAuth token and
//! authorization-code tables, or the three that make up a chat): a
//! `FromRow` struct for the row plus the
//! runtime-checked queries that touch
//! it (`sqlx::query_as::<_, T>`; never the `query!` family). Two modules are
//! the exceptions: [`relationship`] owns no table; it is the batched loader
//! that reads `follows` to build the `Relationship` entity (and will read
//! each later flag table as its ticket lands); [`status_visibility`] owns
//! no table either — it is the single statement of the who-may-see-a-status
//! rules, reading `statuses` beside `follows`, `status_mentions` and
//! `blocks`. And one more follows the rule with a twist:
//! [`scheduled_status`] owns a single table but keeps its payload as opaque
//! `jsonb` — the create-status params as received — for a later slice to
//! replay. [`activity`] is a second table-less reader: one `UNION ALL`
//! across the tables that store a federating fact, answering the incoming
//! dispatcher's lookup of a stored remote activity by its `ap_id` text and
//! the `GET /activities/{uuid}` route's lookup of this instance's own
//! activity by the row's `UUIDv7`. Nothing here knows
//! about HTTP, though every fallible function returns
//! [`AppError`](crate::error::AppError), the crate's single error type, so
//! handlers can use `?`.

pub mod account_note;
pub mod activity;
pub mod announcement;
pub mod backup;
pub mod block;
pub mod bookmark;
pub mod bookmark_folder;
pub mod chat;
pub mod conversation;
pub mod conversation_mute;
pub mod custom_emoji;
pub mod domain_block;
pub mod emoji_reaction;
pub mod endorsement;
pub mod favourite;
pub mod filter;
pub mod follow;
pub mod hashtag;
pub mod hashtag_follow;
pub mod idempotency_key;
pub mod instance;
pub mod instance_policy_domain;
pub mod instance_rule;
pub mod instance_secret;
pub mod instance_setting;
pub mod invite_token;
pub mod job;
pub mod link_card;
pub mod list;
pub mod marker;
pub mod media_attachment;
pub mod media_proxy_cache;
pub mod mention;
pub mod mfa_token;
pub mod moderation_log;
pub mod mute;
pub mod notification;
pub mod oauth_app;
pub mod oauth_login_state;
pub mod oauth_token;
pub mod password_reset;
pub mod pinned_status;
pub mod poll;
pub mod relationship;
pub mod relay;
pub mod report;
pub mod scheduled_status;
pub mod scrobble;
pub mod status;
pub mod status_edit;
pub mod status_search;
pub mod status_visibility;
pub mod suggestion;
pub mod user;
pub mod user_identity;
pub mod user_search;
pub mod web_push_subscription;
pub mod webhook;
