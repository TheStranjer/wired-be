//! The admin ops endpoints: the media-proxy cache ledger and the admin's
//! backup trigger under `/api/v1/pleroma/admin` (WBE-208).
//!
//! The references are Pleroma's
//! `AdminAPI.MediaProxyCacheController`
//! (`lib/pleroma/web/admin_api/controllers/media_proxy_cache_controller.ex`)
//! and `AdminAPIController.create_backup`
//! (`lib/pleroma/web/admin_api/controllers/admin_api_controller.ex`).
//! Pleroma's cache lives in a Cachex table; here it is the
//! `media_proxy_cache_urls` table
//! ([`crate::models::media_proxy_cache`]) that the `/proxy` route itself
//! records every served URL into, pruned to `MEDIA_PROXY_CACHE_LIMIT`
//! rows. Every route takes the [`AdminUser`] extractor and its
//! `admin:read` / `admin:write` scope gate on the area named in the path:
//! `media_proxy_caches` and `backups`.
//!
//! The media-proxy cache actions:
//!
//! - `GET /media_proxy_caches?query=&page=1&page_size=20` — one
//!   `url`-ordered page of the ledger as `{urls, page_size, count}`;
//!   `query` is a case-insensitive substring filter and `count` the whole
//!   filtered total (Pleroma's `index` verbatim).
//! - `POST /media_proxy_caches/delete` body `urls[]` — removes the URLs
//!   from the table and, with a configured `MEDIA_PROXY_PURGE_URL`, issues
//!   the same optional `PURGE` invalidation the purge action below does;
//!   200 `{"urls":[...]}`.
//! - `POST /media_proxy_caches/purge` body `urls[]`, `ban` (bool) — sends
//!   a `PURGE` request per URL to the purge URL (skipped when unset,
//!   failures only logged), removes the URLs from the table, and answers
//!   `{"urls":[...]}`. The `ban` flag is accepted for compatibility:
//!   this ledger records served URLs and has nothing to ban. A body
//!   without `urls` is the 400 `{"error":"Invalid parameters"}`.
//!
//! Pleroma's frontend routes (`GET /frontends`, `POST /frontends/install`)
//! are not ported: wired-be serves no frontend, so there is nothing to
//! install them for.
//!
//! `POST /backups` body `nickname` — Pleroma's `create_backup`: queues a
//! backup for the named account through the very service the user API
//! uses ([`crate::api::backups`]), skipping that endpoint's
//! `BACKUP_LIMIT_DAYS` rate gate, records the `create_backup` moderation
//! entry Pleroma's controller writes, and answers the account's backup
//! list in the user endpoint's shape. An unknown nickname is the 404.
//!
//! [`AdminUser`]: crate::auth::admin::AdminUser

use axum::{
    Router,
    routing::{get, post},
};

use backups::create_backup;
use caches::{delete_caches, list_caches, purge_caches};

use crate::state::AppState;

#[cfg(doc)]
use crate::auth::admin::AdminUser;

mod backups;
mod caches;

/// The read endpoint class of [`AdminUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`AdminUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area of the cache ledger routes
/// (`admin:read:media_proxy_caches` / `admin:write:media_proxy_caches`).
const AREA_CACHE: &str = "media_proxy_caches";
/// The admin scope area of the admin backup trigger.
const AREA_BACKUPS: &str = "backups";
/// Default `page_size` of the cache listing when absent or unusable
/// (the ticket's `page_size=20`).
const DEFAULT_CACHE_PAGE_SIZE: i64 = 20;
/// 400 body of a delete/purge body that carries no `urls` array, the
/// wording the ticket pins.
const INVALID_PARAMETERS: &str = "Invalid parameters";
/// 400 body for a create-backup body without its nickname, in the
/// `Missing parameter: <key>` spelling the admin API shares with the rest
/// of the project.
const MISSING_NICKNAME: &str = "Missing parameter: nickname";
/// The HTTP method of a cache invalidation request, as
/// `MEDIA_PROXY_PURGE_URL`'s front cache expects it.
const PURGE_METHOD: &[u8] = b"PURGE";
/// First status code a purge answer counts as a failure to log.
const ERROR_STATUS_MIN: u16 = 400;

/// The `/media_proxy_caches` and `/backups` routes of the admin API,
/// merged into [`super`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/media_proxy_caches", get(list_caches))
        .route("/media_proxy_caches/delete", post(delete_caches))
        .route("/media_proxy_caches/purge", post(purge_caches))
        .route("/backups", post(create_backup))
}
