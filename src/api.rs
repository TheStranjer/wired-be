//! The HTTP API surface: the composition point for the area routers
//! (`docs/ARCHITECTURE.md` decision 3).
//!
//! Each API area lives in its own module and exposes a
//! `pub fn router() -> Router<AppState>`; [`router`] nests them under their
//! URL prefixes and [`crate::app`] merges the result into the application
//! router.
//!
//! Unmatched paths are not handled here. The app-wide JSON fallback in
//! [`crate::app`] answers every unrouted path under `/api` with 404
//! `{"error":"Not implemented"}` (Pleroma's `api_not_implemented`,
//! `lib/pleroma/web/fallback/redirect_controller.ex`), so the area routers
//! must never install a `Router::fallback` of their own: it would shadow the
//! app-wide one, and merging two routers that each set one panics.
//!
//! [`media_files`], [`activitypub`], [`feeds`], [`well_known`], [`nodeinfo`],
//! [`oauth`], [`oauth_external`] and [`emoji_packs::files_router`] are the
//! routers in this module that [`crate::app`] does not nest under `/api`:
//! uploaded files live at `/media/{key}`, the address [`crate::uploads`]
//! writes into activities; `ActivityPub` actors live at `/users/{nickname}`,
//! the address federation sends as an actor's id; [`feeds`] joins it at
//! `/users/{nickname}/feed*` and `/tags/{tag}`, the syndication addresses
//! readers discover; `WebFinger` and host-meta live at `/.well-known/…`,
//! the discovery addresses fixed by their RFCs;
//! `NodeInfo` lives at `/.well-known/nodeinfo` and `/nodeinfo/{version}`,
//! the addresses fixed by its schema; the OAuth endpoints live at
//! `/oauth/…`, the token addresses fixed by RFC 6749, and so do the
//! external-provider logins of [`oauth_external`], whose paths the
//! providers are configured to answer to; and pack images
//! live at `/emoji/{pack}/{file}`, the address
//! [`crate::services::emoji_packs`] stores in `custom_emojis`.
//! [`misc::router`] joins that list at the root: the `MongooseIM` auth
//! endpoints and `manifest.json` live at `/…`, the addresses `MongooseIM`'s
//! `extauth` http contract and the manifest `<link>` convention fix.
//! [`html_pages::router`] joins it at the root too: the shared HTML helper
//! pages live at the top-level paths Pleroma routes them on,
//! `/frontend_switcher`, `/embed/{id}`, `/notice/{id}/embed_player` and
//! `/mailer/unsubscribe/{token}`.
//! [`password_reset::auth_router`] joins it at `POST /auth/password`, the
//! address Pleroma routes the reset request on outside `/api` (its two
//! `/api/v1/pleroma/password_reset` pages ride on [`v1`]).

pub mod account_lists;
pub mod accounts;
pub mod activitypub;
pub mod announcements;
pub mod apps;
pub mod blocks;
pub mod chats;
pub mod conversations;
pub mod custom_emojis;
pub mod domain_blocks;
pub mod emoji_packs;
pub mod emoji_reactions;
pub mod endorsements;
pub mod federation_status;
pub mod feeds;
pub mod filters;
pub mod follow_requests;
pub mod helpers;
pub mod html_pages;
pub mod instance;
pub mod lists;
pub mod markers;
pub mod media;
pub mod media_files;
pub mod mfa;
pub mod misc;
pub mod mutes;
pub mod nodeinfo;
pub mod notification_settings;
pub mod notifications;
pub mod notifications_v2;
pub mod oauth;
pub mod oauth_external;
pub mod oauth_tokens;
pub mod password_reset;
pub mod pleroma;
pub mod pleroma_account_settings;
pub mod pleroma_accounts;
pub mod pleroma_bookmark_folders;
pub mod pleroma_import;
pub mod pleroma_settings;
pub mod polls;
pub mod push;
pub mod reports;
pub mod scheduled_statuses;
pub mod scrobbles;
pub mod search;
pub mod statuses;
pub mod streaming;
pub mod tags;
pub mod timelines;
pub mod v1;
pub mod v2;
pub mod well_known;

use axum::Router;

use crate::state::AppState;

/// Composes the API area routers under their URL prefixes.
///
/// `/api/v1` ([`v1`]) and `/api/v2` ([`v2`]) hold the Mastodon-compatible
/// client API, `/api/pleroma` ([`pleroma`]) Pleroma's own extensions,
/// `/api/oauth_tokens` ([`oauth_tokens`]) Pleroma's token-management
/// endpoints and `/api/account` ([`accounts::email_confirmation_router`])
/// the email-confirmation link Pleroma serves outside its versioned API;
/// later tickets add the remaining `/api` areas (admin) here the same way.
/// [`media_files`] and [`activitypub`] are the exceptions:
/// they serve `/media` and `/users`, addresses that go out in
/// `ActivityPub` documents, and [`crate::app`] merges them directly.
///
/// `state` is passed down to the two version routers because the `/api/v1`
/// registration and search routes mount the rate limiter (see
/// [`crate::rate_limit`], and the same on `/api/v2`'s search route), and the
/// media upload route sizes its body limit from
/// [`state.config`](AppState::config) (see
/// [`crate::extract::multipart::upload_body_limit`]).
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .nest("/api/v1", v1::router(state))
        .nest("/api/v2", v2::router(state))
        .nest("/api/pleroma", pleroma::router())
        .nest("/api/oauth_tokens", oauth_tokens::router())
        .nest("/api/account", accounts::email_confirmation_router())
}
