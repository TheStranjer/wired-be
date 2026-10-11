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
//! [`media_files`], [`activitypub`], [`well_known`] and [`oauth`] are the
//! routers in this module that [`crate::app`] does not nest under `/api`:
//! uploaded files live at `/media/{key}`, the address [`crate::uploads`]
//! writes into activities; `ActivityPub` actors live at
//! `/users/{nickname}`, the address federation sends as an actor's id;
//! `WebFinger` and host-meta live at `/.well-known/…`, the discovery
//! addresses fixed by their RFCs; and the OAuth endpoints live at
//! `/oauth/…`, the token addresses fixed by RFC 6749.

pub mod activitypub;
pub mod apps;
pub mod custom_emojis;
pub mod media_files;
pub mod oauth;
pub mod v1;
pub mod v2;
pub mod well_known;

use axum::Router;

use crate::state::AppState;

/// Composes the API area routers under their URL prefixes.
///
/// `/api/v1` ([`v1`]) and `/api/v2` ([`v2`]) hold the Mastodon-compatible
/// client API; later tickets add the remaining `/api` areas (Pleroma
/// extensions, well-known) here the same way. [`media_files`] and
/// [`activitypub`] are the exceptions: they serve `/media` and `/users`,
/// addresses that go out in `ActivityPub` documents, and [`crate::app`]
/// merges them directly.
pub fn router() -> Router<AppState> {
    Router::new()
        .nest("/api/v1", v1::router())
        .nest("/api/v2", v2::router())
}
