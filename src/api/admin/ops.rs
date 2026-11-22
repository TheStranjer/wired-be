//! The admin ops endpoints: the media-proxy cache ledger, the frontend
//! installer and the admin's backup trigger under
//! `/api/v1/pleroma/admin` (WBE-208).
//!
//! The references are Pleroma's
//! `AdminAPI.MediaProxyCacheController`
//! (`lib/pleroma/web/admin_api/controllers/media_proxy_cache_controller.ex`),
//! `AdminAPI.FrontendController`
//! (`lib/pleroma/web/admin_api/controllers/frontend_controller.ex`) and
//! `AdminAPIController.create_backup`
//! (`lib/pleroma/web/admin_api/controllers/admin_api_controller.ex`), over
//! `Pleroma.Frontend` (`lib/pleroma/frontend.ex`). Pleroma's cache lives
//! in a Cachex table; here it is the `media_proxy_cache_urls` table
//! ([`crate::models::media_proxy_cache`]) that the `/proxy` route itself
//! records every served URL into, pruned to `MEDIA_PROXY_CACHE_LIMIT`
//! rows. Every route takes the [`AdminUser`] extractor and its
//! `admin:read` / `admin:write` scope gate on the area named in the path:
//! `media_proxy_caches`, `frontends`, `backups`.
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
//! The frontends live in `FRONTENDS_DIR` and are described by the
//! `FRONTENDS_AVAILABLE` JSON array:
//!
//! - `GET /frontends` — the configured frontends as
//!   `[{name, git, build_url, ref, installed}]`, the flag saying whether
//!   `FRONTENDS_DIR/<name>/<ref>` stands on disk.
//! - `POST /frontends/install` body `name` (required), `ref`, `build_url`,
//!   `build_dir`, and the optional multipart `file` — the zip either
//!   uploaded or downloaded from `build_url` (through the SSRF-guarded
//!   outbound client, capped at 100 MB) is extracted safely (no `..` or
//!   absolute entries, at most 20 000 files) and landed at
//!   `FRONTENDS_DIR/<name>/<ref>`; 200 `{"status":"ok"}`. An unknown
//!   frontend name without a `build_url` — and every other install
//!   failure — is the 400 `{"error":"Could not install frontend: ..."}`.
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
use frontends::{install_frontend, list_frontends};

use crate::{config::Config, extract::multipart::upload_body_limit, state::AppState};

#[cfg(doc)]
use crate::auth::admin::AdminUser;

mod backups;
mod caches;
mod frontend_archive;
mod frontends;
#[cfg(test)]
mod test_support;

/// The read endpoint class of [`AdminUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`AdminUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area of the cache ledger routes
/// (`admin:read:media_proxy_caches` / `admin:write:media_proxy_caches`).
const AREA_CACHE: &str = "media_proxy_caches";
/// The admin scope area of the frontend routes.
const AREA_FRONTENDS: &str = "frontends";
/// The admin scope area of the admin backup trigger.
const AREA_BACKUPS: &str = "backups";
/// Default `page_size` of the cache listing when absent or unusable
/// (the ticket's `page_size=20`).
const DEFAULT_CACHE_PAGE_SIZE: i64 = 20;
/// 400 body of a delete/purge body that carries no `urls` array, the
/// wording the ticket pins.
const INVALID_PARAMETERS: &str = "Invalid parameters";
/// 400 body prefix of every frontend install refusal, the wording the
/// ticket pins: `{"error":"Could not install frontend: ..."}`.
const INSTALL_FAILURE: &str = "Could not install frontend: ";
/// 400 body for a create-backup or install body without its required
/// parameter, in the `Missing parameter: <key>` spelling the admin API
/// shares with the rest of the project.
const MISSING_NAME: &str = "Missing parameter: name";
/// The `Missing parameter: name` twin for the backup trigger.
const MISSING_NICKNAME: &str = "Missing parameter: nickname";
/// The HTTP method of a cache invalidation request, as
/// `MEDIA_PROXY_PURGE_URL`'s front cache expects it.
const PURGE_METHOD: &[u8] = b"PURGE";
/// First status code a purge/build answer counts as a failure to log.
const ERROR_STATUS_MIN: u16 = 400;
/// The byte cap on one downloaded frontend build, the ticket's 100 MB.
const MAX_BUILD_BYTES: usize = 100_000_000;
/// The entry cap of one extracted frontend archive, the ticket's 20 000.
const MAX_ZIP_ENTRIES: usize = 20_000;
/// The multipart part the install endpoint reads an uploaded archive from.
const FILE_FIELD: &str = "file";
/// The placeholder a `build_url` spells the ref into, Pleroma's
/// `"${ref}"` of `Frontend.download_build` (`lib/pleroma/frontend.ex`).
const REF_PLACEHOLDER: &str = "${ref}";
/// The ref an install falls back to when neither request nor config
/// names one (the git default, and the segment the destination ends in).
const DEFAULT_REF: &str = "master";
/// The archive subdirectory installed when neither request nor config
/// names one — Pleroma's `build_dir || "dist"`.
const DEFAULT_BUILD_DIR: &str = "dist";

/// The `/media_proxy_caches`, `/frontends` and `/backups` routes of the
/// admin API, merged into [`super`].
///
/// `config` is what the install route's [`upload_body_limit`] layer is
/// sized from — the one admin route that takes a file part and therefore
/// cannot read the extractor limit off the state on its own, as in
/// [`crate::api::media::router`].
pub fn router(config: &Config) -> Router<AppState> {
    Router::new()
        .route("/media_proxy_caches", get(list_caches))
        .route("/media_proxy_caches/delete", post(delete_caches))
        .route("/media_proxy_caches/purge", post(purge_caches))
        .route("/frontends", get(list_frontends))
        .route("/backups", post(create_backup))
        .merge(
            Router::new()
                .route("/frontends/install", post(install_frontend))
                .layer(upload_body_limit(config)),
        )
}
