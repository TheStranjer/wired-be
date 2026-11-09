//! The HTTP integration test suite for wired-be, one crate.
//!
//! Every area file next to this one (`accounts.rs`, `activitypub.rs`, ...,
//! `well_known.rs`) is a module of this crate, not its own test binary.
//! Cargo compiles each top-level `tests/*.rs` file into a separate
//! executable that statically links the whole `wired_be` crate and all its
//! dependencies, so 39 files meant 39 relinks of every change to `src/` and
//! gigabytes of duplicated debuginfo; one crate means one link and
//! `cargo test --test api` runs the whole suite.
//!
//! The shared HTTP harness is [`common`]; the shape every endpoint test
//! takes is documented in `docs/ARCHITECTURE.md` (“Writing tests”), and
//! `health.rs` remains the reference example.

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

/// The directory, suggestions, familiar-followers and birthday listings.
mod account_lists;

/// The account note endpoints (`/api/v1/accounts/{id}/note`).
mod account_notes;

/// The registration endpoint (`POST /api/v1/accounts`), open mode and
/// invite tokens alike.
mod account_registration;

/// The account status listing (`GET /api/v1/accounts/{id}/statuses`): its
/// filters, the visibility each reader reaches, the pinned page and the
/// `RESTRICT_UNAUTHENTICATED` gate.
mod account_statuses;

/// The core account endpoints: `verify_credentials`, `GET /accounts/{id}` and `lookup`.
mod accounts;

/// The `ActivityPub` actor document (`GET /users/{nickname}`).
mod activitypub;

/// Announcements over HTTP (`/api/v1/announcements`).
mod announcements;

/// The API router skeleton (`src/api.rs`) and its JSON fallback.
mod api_skeleton;

/// App registration and verification (`/api/v1/apps`).
mod apps;

/// The bearer extractors and scope guards of `wired_be::auth` over HTTP.
mod auth;

/// The `AUTHORIZED_FETCH` gate on the `ActivityPub` actor route.
mod authorized_fetch;

/// The blocks endpoints (`block`, `unblock` and the blocks list).
mod blocks;

/// The bookmark writes and the bookmarks list over HTTP
/// (`/api/v1/statuses/{id}/bookmark` and `GET /api/v1/bookmarks`).
mod bookmarks;

/// The chat API over HTTP (`/api/v1/pleroma/chats` and its v2 twin).
mod chats;

/// The custom emoji list (`GET /api/v1/custom_emojis`).
mod custom_emojis;

/// Domain blocks (`/api/v1/domain_blocks`).
mod domain_blocks;

/// The emoji-pack endpoints of `src/api/emoji_packs.rs`.
mod emoji_packs;

/// Emoji reactions (`/api/v1/pleroma/statuses/{id}/reactions`).
mod emoji_reactions;

/// The endorsements endpoints (`/api/v1/endorsements`).
mod endorsements;

/// Atom and RSS feeds (`/users/{nickname}/feed*`, `/tags/{tag}`).
mod feeds;

/// Filters over HTTP (`/api/v1/filters`).
mod filters;

/// Favourites (`/api/v1/statuses/{id}/favourite`, `…/favourited_by` and `/favourites`).
mod favourites;

/// Following and unfollowing an account (`/api/v1/accounts/{id}/follow`).
mod follow_actions;

/// The legacy follow write keyed by handle or actor URL
/// (`POST /api/v1/follows`).
mod follow_by_uri;

/// The follower and following lists of an account.
mod follow_lists;

/// Follow requests (`/api/v1/follow_requests` and the answer routes).
mod follow_requests;

/// The `/health` endpoint and the `common` harness itself.
mod health;

/// The public instance endpoints (`src/api/instance.rs`).
mod instance;

/// The instance actor document and its exclusion from public listings.
mod instance_actor;

/// List members (`/api/v1/lists/{id}/accounts`).
mod list_accounts;

/// Lists over HTTP (`/api/v1/lists`).
mod lists;

/// Read markers (`/api/v1/markers`).
mod markers;

/// The media route (`src/api/media_files.rs`) and the local uploader.
mod media;

/// The media metadata endpoints (`GET`/`PUT`/`DELETE /api/v1/media/{id}`).
mod media_metadata;

/// The media proxy route (`GET /proxy/{sig}/{url}[/{filename}]`) against a mock upstream.
mod media_proxy;

/// The media upload endpoints (`POST /api/v1/media` and `/api/v2/media`).
mod media_upload;

/// The MFA endpoints (`/api/pleroma/accounts/mfa`).
mod mfa;

/// The `MongooseIM` auth endpoints and the small utility endpoints.
mod misc;

/// The mutes endpoints.
mod mutes;

/// NodeInfo (`/.well-known/nodeinfo` and `/nodeinfo/{version}`).
mod nodeinfo;

/// Pleroma's notification settings (`PUT /api/pleroma/notification_settings`)
/// and the owner-only render of the flags.
mod notification_settings;

/// The flat notification API: `clear`, the dismissals, `destroy_multiple`
/// and Pleroma's `read` (`/api/v1/notifications`, `/api/pleroma`).
mod notifications;

/// The grouped v2 notification API (`/api/v2/notifications`).
mod notifications_v2;

/// The `/oauth` endpoints: the token, revoke and authorize flows.
mod oauth;

/// The external OAuth login endpoints.
mod oauth_external;

/// The OAuth MFA challenge flow (`/oauth/mfa` and the token grant's challenge).
mod oauth_mfa;

/// OAuth token management (`/api/oauth_tokens` and the pleroma apps).
mod oauth_tokens;

/// Password reset (`/auth/password` and `/api/v1/pleroma/password_reset`).
mod password_reset;

/// Account aliases and `move_account` (`/api/pleroma`).
mod pleroma_account_settings;

/// The Pleroma account extensions merged at `/api/pleroma/accounts`.
mod pleroma_accounts;

/// Bookmark folders (`/api/v1/pleroma/bookmark_folders`) over HTTP.
mod pleroma_bookmark_folders;

/// CSV imports of follows, blocks and mutes (`/api/pleroma/*_import`).
mod pleroma_import;

/// The frontend settings store, mascot and preferred frontend.
mod pleroma_settings;

/// Pinned statuses (`/api/v1/statuses/{id}/pin` and its `…/unpin` twin).
mod pins;

/// Web Push subscriptions (`/api/v1/push/subscription`) and the VAPID key
/// the app and instance responses advertise.
mod push;

/// The per-route rate limiter: mounted buckets, headers, and 429s.
mod rate_limit;

/// Reblogs over HTTP (`/api/v1/statuses/{id}/reblog`, its `…/unreblog`
/// twin and the `…/reblogged_by` listing).
mod reblogs;

/// `GET /api/v1/accounts/relationships`.
mod relationships;

/// `POST /api/v1/accounts/{id}/remove_from_followers`.
mod remove_from_followers;

/// Reports over HTTP (`POST /api/v1/reports`): filing a report.
mod reports;

/// Scrobbles over HTTP (`/api/v1/pleroma/scrobble`).
mod scrobbles;

/// The search endpoints (`/api/v1/accounts/search`, `/api/v1/search` and
/// `/api/v2/search` with their accounts, statuses and hashtag arms).
mod search;

/// The status endpoints: the reads (`GET /api/v1/statuses/{id}` and its
/// batch form) and the write (`POST /api/v1/statuses`).
mod statuses;

/// The streaming WebSocket (`/api/v1/streaming`) over a real listener.
mod streaming;

/// Tags over HTTP (`/api/v1/tags/{name}`, `/api/v1/followed_tags`, trends).
mod tags;

/// Thread mutes over HTTP (`/api/v1/statuses/{id}/mute` and its `…/unmute`
/// twin) and the `muted` / `pleroma.thread_muted` entity flags.
mod thread_mutes;

/// Status timelines over HTTP (`/api/v1/timelines/*`): the tag timeline's
/// selection, filters, cursors and auth.
mod timelines;

/// Status translation over HTTP (`POST /api/v1/statuses/{id}/translate` and
/// the instance endpoints' translation half), against a mock provider.
mod translate;

/// `PATCH /api/v1/accounts/update_credentials`.
mod update_credentials;

/// WebFinger and host-meta (`/.well-known/webfinger`, `/.well-known/host-meta`).
mod well_known;
