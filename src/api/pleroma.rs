//! Pleroma extension endpoints under `/api/pleroma`
//! (`docs/ARCHITECTURE.md` decision 1): the API areas that are Pleroma's
//! own rather than Mastodon-compatible.
//!
//! Like the other area routers this is a composition point: each feature
//! module exposes a `router()` for its own subtree, and [`router`] nests
//! it at the path Pleroma serves it on. [`mfa`] is the first; the rest of
//! the `/api/pleroma` surface joins here the same way. [`notifications`]
//! adds the flat notification API's Pleroma half: `POST /notifications/read`
//! (`PleromaAPI.NotificationController.mark_as_read`), and today [`misc`]
//! adds one route: `frontend_configurations`, Pleroma's
//! `UtilController.frontend_configurations/2`, [`notification_settings`]
//! the same controller's `update_notification_settings/2` at
//! `PUT /notification_settings`, and
//! [`pleroma_account_settings`] adds the account utilities of the same
//! controller: `aliases` and `move_account`, and [`pleroma_import`] the
//! CSV imports of `UserImportController`: `follow_import`,
//! `blocks_import` and `mutes_import`. Do not call
//! [`Router::fallback`] here: unmatched paths must reach the app-wide JSON
//! fallback in [`crate::app`].

use axum::Router;

use crate::{
    api::{
        mfa, misc, notification_settings, notifications, pleroma_account_settings, pleroma_import,
    },
    state::AppState,
};

/// The `/api/pleroma` router, merged into [`crate::api`].
pub fn router() -> Router<AppState> {
    Router::new()
        .nest("/accounts/mfa", mfa::router())
        .nest("/notifications", notifications::pleroma_router())
        .merge(misc::frontend_configurations_router())
        .merge(notification_settings::router())
        .merge(pleroma_account_settings::router())
        .merge(pleroma_import::router())
}
