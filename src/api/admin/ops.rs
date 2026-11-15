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
//! `BACKUP_LIMIT_DAYS` rate gate, and answers the account's backup list
//! in the user endpoint's shape. An unknown nickname is the 404.
//!
//! [`AdminUser`]: crate::auth::admin::AdminUser

use std::{
    collections::HashMap,
    io::{Cursor, Read as _},
    path::{Path, PathBuf},
};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::Method,
    routing::{get, post},
};
use bytes::Bytes;
use serde::Deserialize;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    auth::admin::AdminUser,
    config::{Config, FrontendConfig},
    entities::{
        admin::{AdminFrontend, FrontendInstallStatus, MediaProxyCachePage, MediaProxyCacheUrls},
        backup::Backup,
    },
    error::{AppError, AppResult},
    extract::{
        ApiBody, flex,
        multipart::{MultipartForm, UploadedFiles as _, upload_body_limit},
    },
    models::{backup, media_proxy_cache, user::get_by_nickname},
    services::media_proxy,
    state::AppState,
};

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

/// `GET /api/v1/pleroma/admin/media_proxy_caches` — Pleroma's `index`:
/// the filtered ledger windowed by `page` / `page_size` (defaults 1 / 20,
/// capped like the rest of the admin listings), answered as
/// `{urls, page_size, count}` with `count` the whole filtered total.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::Database`] from the two reads.
async fn list_caches(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<MediaProxyCachePage>> {
    admin.require(CLASS_READ, AREA_CACHE)?;
    let query = super::text_of(&params, "query");
    let page = super::page_number(&params, "page", super::DEFAULT_PAGE);
    let size = cache_page_size(&params);
    let offset = (page - 1).saturating_mul(size);
    let count = media_proxy_cache::count(&state.db, query.as_deref()).await?;
    let urls = media_proxy_cache::list(&state.db, query.as_deref(), size, offset).await?;
    debug!(
        shown = urls.len(),
        count, page, "admin media proxy cache listed"
    );
    Ok(Json(MediaProxyCachePage {
        urls,
        page_size: size,
        count,
    }))
}

/// The cache listing's `page_size`: the admin lists' lenient reading with
/// the ticket's 20-row default and the shared [`super::MAX_PAGE_SIZE`] cap.
fn cache_page_size(params: &HashMap<String, String>) -> i64 {
    super::page_number_of(
        super::text_of(params, "page_size").as_deref(),
        DEFAULT_CACHE_PAGE_SIZE,
    )
    .min(super::MAX_PAGE_SIZE)
}

/// The body of both cache write actions: Pleroma's required `urls` array
/// and the purge's optional `ban` flag, read as `Option`s so the missing
/// `urls` answers the ticket's own 400 and not the extractor's 422.
#[derive(Debug, Deserialize)]
struct CacheWriteBody {
    /// The URLs the action takes; absence is the refusal, an empty list
    /// an action on nothing.
    #[serde(default)]
    urls: Option<Vec<String>>,
    /// The purge's ban flag, accepted through [`flex::opt_bool`] so a
    /// form-encoded `"true"` counts. This ledger records served URLs and
    /// has no ban state to carry, so the flag changes nothing.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    ban: Option<bool>,
}

/// `POST /api/v1/pleroma/admin/media_proxy_caches/delete` — Pleroma's
/// `delete`: remove the URLs from the ledger, issuing the same optional
/// `PURGE` invalidation [`purge_caches`] does, and answer
/// `{"urls":[...]}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] with
/// [`INVALID_PARAMETERS`] for a body without `urls`;
/// [`AppError::Database`] from the delete.
async fn delete_caches(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<CacheWriteBody>,
) -> AppResult<Json<MediaProxyCacheUrls>> {
    admin.require(CLASS_WRITE, AREA_CACHE)?;
    let urls = require_urls(body.urls)?;
    let removed = clear_cache(&state, &urls).await?;
    debug!(
        requested = urls.len(),
        removed, "admin media proxy cache deleted"
    );
    Ok(Json(MediaProxyCacheUrls::new(&urls)))
}

/// `POST /api/v1/pleroma/admin/media_proxy_caches/purge` — Pleroma's
/// `purge`: a `PURGE` invalidation per URL, the URLs out of the ledger,
/// `{"urls":[...]}` back. `ban` is read for API shape and changes nothing
/// (see [`CacheWriteBody::ban`]).
///
/// # Errors
///
/// As [`delete_caches`]'s — the purge requests themselves never fail the
/// action, they only reach the log.
async fn purge_caches(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<CacheWriteBody>,
) -> AppResult<Json<MediaProxyCacheUrls>> {
    admin.require(CLASS_WRITE, AREA_CACHE)?;
    let urls = require_urls(body.urls)?;
    if body.ban.unwrap_or(false) {
        debug!("admin media proxy cache purge asked to ban: this ledger only records served urls");
    }
    let removed = clear_cache(&state, &urls).await?;
    debug!(
        requested = urls.len(),
        removed, "admin media proxy cache purged"
    );
    Ok(Json(MediaProxyCacheUrls::new(&urls)))
}

/// The `urls` half of the ticket: absent (or JSON `null`) is the 400
/// `{"error":"Invalid parameters"}`, present passes through.
///
/// # Errors
///
/// [`AppError::BadRequest`] with [`INVALID_PARAMETERS`].
fn require_urls(urls: Option<Vec<String>>) -> AppResult<Vec<String>> {
    urls.ok_or_else(|| AppError::BadRequest(INVALID_PARAMETERS.to_owned()))
}

/// The shared delete/purge write: first the optional `PURGE` requests,
/// then the ledger removal. Pleroma's `remove_from_banned_urls` and
/// `Invalidation.purge` in the one order that keeps a refused purge from
/// resurrecting an entry.
///
/// # Errors
///
/// [`AppError::Database`] from the delete.
async fn clear_cache(state: &AppState, urls: &[String]) -> AppResult<u64> {
    for url in urls {
        send_purge(state, url).await;
    }
    media_proxy_cache::remove(&state.db, urls).await
}

/// One URL's `PURGE` invalidation, when `MEDIA_PROXY_PURGE_URL` names the
/// origin and the proxy signs links ([`purge_target`]): the request is
/// best-effort — the ticket logs failures and moves on, because the
/// admin's ledger write must not hinge on a cache they pointed at.
///
/// [`purge_target`]: media_proxy::purge_target
async fn send_purge(state: &AppState, url: &str) {
    let Some(target) = media_proxy::purge_target(&state.config, url) else {
        return;
    };
    let Ok(method) = Method::from_bytes(PURGE_METHOD) else {
        warn!(%target, "media proxy purge: PURGE is not a usable HTTP method");
        return;
    };
    match state.http_client.request(method, &target, &[]).await {
        Ok(response) if response.status < ERROR_STATUS_MIN => {
            debug!(status = response.status, %target, "media proxy cache purged");
        }
        Ok(response) => {
            warn!(status = response.status, %target, "media proxy purge request refused");
        }
        Err(err) => {
            warn!(%err, %target, "media proxy purge request failed");
        }
    }
}

/// `GET /api/v1/pleroma/admin/frontends` — Pleroma's `FrontendController.index`
/// narrowed to the ticket's array: the configured frontends, each with
/// its `installed` flag read off the disk.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate.
async fn list_frontends(
    State(state): State<AppState>,
    admin: AdminUser,
) -> AppResult<Json<Vec<AdminFrontend>>> {
    admin.require(CLASS_READ, AREA_FRONTENDS)?;
    let frontends = state
        .config
        .frontends
        .available
        .iter()
        .map(|entry| AdminFrontend::from_config(entry, frontend_installed(&state.config, entry)))
        .collect();
    Ok(Json(frontends))
}

/// Whether `entry` is installed: `FRONTENDS_DIR/<name>/<ref>` is a
/// directory (Pleroma's `installed()` / `installed_refs()` ls checks,
/// `frontend_controller.ex`). An entry without a ref is flagged by its
/// `<name>` directory alone.
fn frontend_installed(config: &Config, entry: &FrontendConfig) -> bool {
    let mut path = config.frontends.dir.join(&entry.name);
    if let Some(reference) = &entry.reference {
        path.push(reference);
    }
    path.is_dir()
}

/// The text parameters of `POST /frontends/install`: every field but
/// `name` optional, present values winning over the config entry's
/// (Pleroma's `Frontend.install` merge, `lib/pleroma/frontend.ex`). The
/// `file` part is not a field: it arrives through [`MultipartForm`]'s
/// file half.
#[derive(Debug, Default, Deserialize)]
struct InstallParams {
    /// The frontend to install; required.
    #[serde(default)]
    name: Option<String>,
    /// The build ref — the JSON/form key `ref`, a Rust keyword.
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    /// Where to download the build archive, overriding the config.
    #[serde(default)]
    build_url: Option<String>,
    /// The archive subdirectory to install, overriding the config.
    #[serde(default)]
    build_dir: Option<String>,
}

/// A resolved install: what the request and the `FRONTENDS_AVAILABLE`
/// entry together decide, before a byte moves.
#[derive(Debug)]
struct Install {
    /// The `<name>` directory segment.
    name: String,
    /// The `<ref>` directory segment under it.
    reference: String,
    /// The archive subdirectory to install; `None` means the whole
    /// archive root.
    build_dir: Option<String>,
    /// Where to download the archive when the body uploads no file.
    build_url: Option<String>,
}

/// `POST /api/v1/pleroma/admin/frontends/install` — Pleroma's
/// `FrontendController.install` over `Pleroma.Frontend.install`: take the
/// uploaded `file` or download the `build_url` archive, extract it safely
/// and land it at `FRONTENDS_DIR/<name>/<ref>`; 200 `{"status":"ok"}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] with
/// [`MISSING_NAME`] for a body without `name`, and with
/// `Could not install frontend: ...` for an unknown name without a
/// `build_url`, an unsafe name, ref or build directory, a failed
/// download, and an archive that is unreadable, over-full or unsafe;
/// [`AppError::PayloadTooLarge`] for an uploaded file past the upload
/// limits. Database work belongs to none of this.
async fn install_frontend(
    State(state): State<AppState>,
    admin: AdminUser,
    mut form: MultipartForm<InstallParams>,
) -> AppResult<Json<FrontendInstallStatus>> {
    admin.require(CLASS_WRITE, AREA_FRONTENDS)?;
    let upload = form.files.take(FILE_FIELD).map(|file| file.bytes);
    let install = resolve_install(&state.config, &form.fields, upload.is_some())?;
    let archive = match upload {
        Some(bytes) => bytes,
        None => download_build(&state, &install).await?,
    };
    let destination = state
        .config
        .frontends
        .dir
        .join(&install.name)
        .join(&install.reference);
    land_archive(
        &state.config.frontends.dir,
        destination,
        install.build_dir.as_deref(),
        archive,
    )
    .await?;
    debug!(
        frontend = %install.name,
        reference = %install.reference,
        "frontend installed"
    );
    Ok(Json(FrontendInstallStatus::OK))
}

/// The request-over-config merge, Pleroma's `Map.merge(config, cmd)` with
/// the `cmd || config` fallback: the name must be there and safe, the ref
/// falls to [`DEFAULT_REF`], the `build_url` to the config entry — and
/// the ticket's refusal answers before anything else when the name matches
/// no entry and the request carries neither a `build_url` nor a file.
///
/// # Errors
///
/// [`AppError::BadRequest`] with [`MISSING_NAME`], or with the
/// `Could not install frontend: ` prefix for the unknown name, the
/// source-less install and the unsafe name/ref/build directory.
fn resolve_install(config: &Config, params: &InstallParams, has_file: bool) -> AppResult<Install> {
    let name = params
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::BadRequest(MISSING_NAME.to_owned()))?
        .to_owned();
    let entry = config
        .frontends
        .available
        .iter()
        .find(|candidate| candidate.name == name);
    let build_url = first_of(
        params.build_url.as_deref(),
        entry.and_then(|e| e.build_url.as_deref()),
    )
    .map(str::to_owned);
    if entry.is_none() && build_url.is_none() && !has_file {
        return Err(install_failure(&format!("unknown frontend {name:?}")));
    }
    let reference = first_of(
        params.reference.as_deref(),
        entry.and_then(|e| e.reference.as_deref()),
    )
    .unwrap_or(DEFAULT_REF)
    .to_owned();
    let build_dir = first_of(
        params.build_dir.as_deref(),
        entry.and_then(|e| e.build_dir.as_deref()),
    )
    .map(str::to_owned);
    if build_url.is_none() && !has_file {
        return Err(install_failure(
            "no build_url is configured and no file was uploaded",
        ));
    }
    check_segment(&name, "frontend name")?;
    check_segment(&reference, "frontend ref")?;
    if let Some(build_dir) = &build_dir {
        check_relative_path(build_dir, "build directory")?;
    }
    Ok(Install {
        name,
        reference,
        build_dir,
        build_url,
    })
}

/// The request value when it carries text, the config value when the
/// request omits it; `None` when neither does.
fn first_of<'a>(sent: Option<&'a str>, configured: Option<&'a str>) -> Option<&'a str> {
    let usable = |text: &'a str| (!text.trim().is_empty()).then(|| text.trim());
    sent.and_then(usable)
        .or_else(|| configured.and_then(usable))
}

/// Fetches the build archive from `install`'s `build_url` — `${ref}`
/// substituted, Pleroma's `download_build` — through the instance's
/// SSRF-guarded outbound client, capped at [`MAX_BUILD_BYTES`].
///
/// # Errors
///
/// [`AppError::BadRequest`] with the install refusal for a missing
/// source, a failed or refused fetch, and a body over the cap.
async fn download_build(state: &AppState, install: &Install) -> AppResult<Bytes> {
    let Some(build_url) = &install.build_url else {
        return Err(install_failure(
            "no build_url is configured and no file was uploaded",
        ));
    };
    let url = build_url.replace(REF_PLACEHOLDER, &install.reference);
    match state
        .http_client
        .get_limited(&url, &[], MAX_BUILD_BYTES)
        .await
    {
        Ok(response) if response.status < ERROR_STATUS_MIN => Ok(response.body),
        Ok(response) => {
            warn!(status = response.status, %url, "frontend install: the build server refused");
            Err(install_failure(&format!(
                "the build url answered {}",
                response.status
            )))
        }
        Err(err) => {
            warn!(%err, %url, "frontend install: the build could not be downloaded");
            Err(install_failure(&format!(
                "the build could not be downloaded: {err}"
            )))
        }
    }
}

/// The on-disk half of an install, run on the blocking pool like the
/// media analysis: extract the archive into a staging directory inside
/// `frontends_dir` (same filesystem, so landing it is a rename), then
/// move the `build_dir` subtree — or the whole archive when the request
/// and config name none or the archive carries none — into
/// `destination`, replacing any earlier install.
///
/// # Errors
///
/// As [`install_frontend`]'s install refusals, for the extraction and the
/// filesystem; [`AppError::Internal`] only when the blocking task itself
/// is gone.
async fn land_archive(
    frontends_dir: &Path,
    destination: PathBuf,
    build_dir: Option<&str>,
    archive: Bytes,
) -> AppResult<()> {
    let frontends = frontends_dir.to_path_buf();
    let build_dir = build_dir.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        let staging = frontends.join(format!(".tmp-install-{}", Uuid::now_v7()));
        install_staged(&staging, &destination, build_dir.as_deref(), &archive)
    })
    .await
    .map_err(|err| AppError::Internal(format!("frontend install task did not finish: {err}")))?
}

/// Extract `archive` under `staging` and land the chosen subtree at
/// `destination`; the staging directory goes either way.
fn install_staged(
    staging: &Path,
    destination: &Path,
    build_dir: Option<&str>,
    archive: &[u8],
) -> AppResult<()> {
    let entries = read_zip(archive)?;
    std::fs::create_dir_all(staging).map_err(|err| io_failure(&err))?;
    let staged = stage_entries(staging, &entries);
    let outcome = staged.and_then(|()| {
        let source = installable_source(staging, build_dir);
        land_directory(&source, destination)
    });
    let _cleanup = std::fs::remove_dir_all(staging);
    outcome.map_err(|err| io_failure(&err))
}

/// The staged entries of one archive: validated relative path to bytes.
type ZipEntries = Vec<(String, Vec<u8>)>;

/// Read an archive's file entries with the ticket's safety gates up
/// front: the entry-count bound before any read, every name through
/// [`safe_entry_name`]. Directory entries carry nothing to land and are
/// dropped — the file writes create their own parents.
///
/// # Errors
///
/// [`AppError::BadRequest`] with the install refusal for an unreadable
/// archive, the over-full entry count and an unsafe name.
fn read_zip(archive: &[u8]) -> AppResult<ZipEntries> {
    let mut reader = zip::ZipArchive::new(Cursor::new(archive))
        .map_err(|err| install_failure(&format!("the archive could not be read: {err}")))?;
    if reader.len() > MAX_ZIP_ENTRIES {
        return Err(install_failure(&format!(
            "the archive carries {} entries, at most {MAX_ZIP_ENTRIES} are allowed",
            reader.len()
        )));
    }
    let mut entries: ZipEntries = Vec::new();
    for index in 0..reader.len() {
        let mut file = reader.by_index(index).map_err(|err| {
            install_failure(&format!("an archive entry could not be read: {err}"))
        })?;
        let name = file.name().map_err(|err| {
            install_failure(&format!("an archive entry name is undecodable: {err}"))
        })?;
        let name = safe_entry_name(&name)?;
        if file.is_dir() {
            continue;
        }
        let mut bytes: Vec<u8> = Vec::new();
        let _read = file.read_to_end(&mut bytes).map_err(|err| {
            install_failure(&format!("an archive entry could not be read: {err}"))
        })?;
        entries.push((name, bytes));
    }
    Ok(entries)
}

/// Writes the validated `entries` under `staging`, creating parent
/// directories as the names ask for.
///
/// # Errors
///
/// Whatever the filesystem says.
fn stage_entries(staging: &Path, entries: &[(String, Vec<u8>)]) -> std::io::Result<()> {
    for (name, bytes) in entries {
        let path = staging.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)?;
    }
    Ok(())
}

/// The staged subtree to install: `build_dir` when the archive carried
/// one (Pleroma's `build_dir || "dist"` — and a plain "dist" is tried
/// even when the request named nothing, so the stock build layout
/// installs its `dist`); otherwise the archive root.
fn installable_source(staging: &Path, build_dir: Option<&str>) -> PathBuf {
    let wanted = build_dir.unwrap_or(DEFAULT_BUILD_DIR);
    let subtree = staging.join(wanted);
    if subtree.is_dir() {
        subtree
    } else {
        staging.to_path_buf()
    }
}

/// Lands `source` at `destination`: an earlier install goes, the parent
/// chain appears, and the staged tree renames over — a same-filesystem
/// move, staging being inside `FRONTENDS_DIR` itself.
fn land_directory(source: &Path, destination: &Path) -> std::io::Result<()> {
    if destination.exists() {
        std::fs::remove_dir_all(destination)?;
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(source, destination)
}

/// The ticket's archive-entry gate: a relative `/`-separated path whose
/// components are none of them `..` or `.`, empty, or a leading slash —
/// backslashes refuse outright, since on the wire they belong to the
/// Windows spellings this endpoint never installs from.
fn safe_entry_name(name: &str) -> AppResult<String> {
    let backslashed = name.chars().any(|c| c == '\\');
    let clean = !name.is_empty()
        && !backslashed
        && !name.starts_with('/')
        && name
            .split('/')
            .all(|c| !c.is_empty() && c != ".." && c != ".");
    if clean {
        return Ok(name.to_owned());
    }
    Err(install_failure(&format!(
        "the archive entry {name:?} is not a safe relative path"
    )))
}

/// Whether `c` may stand in a single install path segment: the
/// `uploads::check_key` charset minus the separator.
const fn is_segment_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// A single safe path segment (a frontend `name` or `ref`): the
/// [`is_segment_char`] charset, non-empty, no bare `.` or `..`.
///
/// # Errors
///
/// [`AppError::BadRequest`] with the install refusal.
fn check_segment(text: &str, what: &str) -> AppResult<()> {
    let safe = !text.is_empty() && text != "." && text != ".." && text.chars().all(is_segment_char);
    if safe {
        return Ok(());
    }
    Err(install_failure(&format!(
        "the {what} {text:?} is not a safe name"
    )))
}

/// A safe relative directory path (a `build_dir`): `/`-separated
/// [`is_segment_char`] segments, no empty ones, no `.` or `..`.
///
/// # Errors
///
/// [`AppError::BadRequest`] with the install refusal.
fn check_relative_path(text: &str, what: &str) -> AppResult<()> {
    let safe = !text.is_empty()
        && !text.starts_with('/')
        && text
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && c.chars().all(is_segment_char));
    if safe {
        return Ok(());
    }
    Err(install_failure(&format!(
        "the {what} {text:?} is not a safe relative path"
    )))
}

/// The ticket's install refusal: 400 `{"error":"Could not install
/// frontend: <detail>"}`.
fn install_failure(detail: &str) -> AppError {
    AppError::BadRequest(format!("{INSTALL_FAILURE}{detail}"))
}

/// A filesystem failure at install time, in the same refusal shape: this
/// is an operator-facing endpoint, and the operator is the one who can
/// fix a `Permission denied`.
fn io_failure(err: &std::io::Error) -> AppError {
    install_failure(&format!("the filesystem said: {err}"))
}

/// The body of `POST /backups`: the account to request the export for.
#[derive(Debug, Deserialize)]
struct BackupBody {
    /// The target account's nickname; required.
    #[serde(default)]
    nickname: Option<String>,
}

/// `POST /api/v1/pleroma/admin/backups` — Pleroma's `create_backup`:
/// record a backup for the named account and queue its job through the
/// user endpoint's own service, skipping the `BACKUP_LIMIT_DAYS` gate
/// that stands between one account and its own next export, and answer
/// the account's backup list in the user endpoint's shape.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] with
/// [`MISSING_NICKNAME`]; [`AppError::NotFound`] for a nickname no account
/// carries; [`AppError::Database`] from the record, the queue write and
/// the list read.
async fn create_backup(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<BackupBody>,
) -> AppResult<Json<Vec<Backup>>> {
    admin.require(CLASS_WRITE, AREA_BACKUPS)?;
    let Some(nickname) = first_of(body.nickname.as_deref(), None) else {
        return Err(AppError::BadRequest(MISSING_NICKNAME.to_owned()));
    };
    let target = get_by_nickname(&state.db, nickname)
        .await?
        .ok_or(AppError::NotFound)?;
    let row = backup::create(&state.db, &target).await?;
    crate::api::backups::enqueue_backup_job(&state.db, row.id).await?;
    debug!(
        backup = %row.id,
        user = %target.id,
        admin = %admin.0.id,
        "admin backup requested"
    );
    let rows = backup::list_for_user(&state.db, target.id).await?;
    Ok(Json(crate::api::backups::render_entities(
        &state.uploader,
        &rows,
    )))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::FrontendSettings;

    /// Whether `err` is the install refusal naming `detail` — the ticket's
    /// pinned prefix, with the tail left to the reason.
    fn is_install_failure(err: &AppError, detail_start: &str) -> bool {
        matches!(
            err,
            AppError::BadRequest(message)
                if message
                    .strip_prefix(INSTALL_FAILURE)
                    .is_some_and(|rest| rest.starts_with(detail_start))
        )
    }

    /// A [`Config`] over the test constants with the frontends settings
    /// applied directly (the fields, not the environment).
    fn config_with_frontends(frontends: &[(&str, Option<&str>)]) -> Config {
        let mut config = Config::for_tests();
        config.frontends.available = frontends
            .iter()
            .map(|(name, reference)| FrontendConfig {
                name: (*name).to_owned(),
                git: None,
                build_url: None,
                reference: reference.map(|r| (*r).to_owned()),
                build_dir: None,
            })
            .collect();
        config
    }

    /// The install the params resolve to, panicking on a refusal the
    /// test did not expect.
    fn resolved(config: &Config, params: &InstallParams, has_file: bool) -> Install {
        resolve_install(config, params, has_file).expect("the fixture installs")
    }

    #[test]
    fn the_cache_body_reads_the_ticket_shape() {
        let full: CacheWriteBody = serde_json::from_value(json!({
            "urls": ["https://remote.example/a.png"],
            "ban": "true",
        }))
        .expect("the ticket's shape deserializes");
        assert_eq!(
            full.urls.as_deref(),
            Some(&["https://remote.example/a.png".to_owned()][..])
        );
        assert_eq!(full.ban, Some(true), "the form spelling reads through flex");
        let empty: CacheWriteBody =
            serde_json::from_value(json!({})).expect("an empty body deserializes");
        assert_eq!(
            empty.urls, None,
            "no urls is the handler's refusal, not the extractor's"
        );
        let zeroed: CacheWriteBody =
            serde_json::from_value(json!({"urls": []})).expect("the empty list deserializes");
        assert_eq!(
            require_urls(zeroed.urls)
                .expect("an empty list is an action on nothing")
                .len(),
            0,
            "and passes the gate"
        );
    }

    #[test]
    fn the_missing_urls_are_the_pinned_400() {
        let err = require_urls(None).expect_err("absence is refused");
        assert_eq!(err.to_string(), INVALID_PARAMETERS, "the ticket wording");
    }

    #[test]
    fn the_cache_page_size_defaults_to_the_ticket_window() {
        let parse = |pairs: &[(&str, &str)]| {
            let params: HashMap<String, String> = pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect();
            cache_page_size(&params)
        };
        assert_eq!(parse(&[]), DEFAULT_CACHE_PAGE_SIZE, "absent is 20");
        assert_eq!(parse(&[("page_size", "nope")]), 20, "junk defaults");
        assert_eq!(parse(&[("page_size", "3")]), 3, "a usable size passes");
        assert_eq!(
            parse(&[("page_size", "5000")]),
            super::super::MAX_PAGE_SIZE,
            "and the admin cap holds"
        );
    }

    #[test]
    fn an_unknown_frontend_without_a_build_url_is_the_pinned_refusal() {
        let config = config_with_frontends(&[("plume", Some("stable"))]);
        let err = resolve_install(
            &config,
            &InstallParams {
                name: Some("ghost".to_owned()),
                ..InstallParams::default()
            },
            false,
        )
        .expect_err("no config entry and no build_url install nothing");
        assert!(is_install_failure(&err, "unknown frontend"), "{err:?}");
    }

    #[test]
    fn the_body_without_a_name_is_the_missing_parameter_400() {
        let config = Config::for_tests();
        let err = resolve_install(&config, &InstallParams::default(), false)
            .expect_err("no name is no install");
        assert_eq!(err.to_string(), MISSING_NAME, "the named parameter");
    }

    #[test]
    fn an_unknown_name_with_a_build_url_installs_anyway() {
        let config = Config::for_tests();
        let install = resolved(
            &config,
            &InstallParams {
                name: Some("custom-fe".to_owned()),
                reference: Some("v2.0.0".to_owned()),
                build_url: Some("https://build.example/${ref}.zip".to_owned()),
                build_dir: Some("webout".to_owned()),
            },
            false,
        );
        assert_eq!(install.name, "custom-fe", "the name passes through");
        assert_eq!(install.reference, "v2.0.0", "and the sent ref");
        assert_eq!(
            install.build_dir.as_deref(),
            Some("webout"),
            "and the sent build dir"
        );
        assert!(
            install
                .build_url
                .is_some_and(|url| url.contains(REF_PLACEHOLDER)),
            "the url keeps its placeholder for the fetch"
        );
    }

    #[test]
    fn the_config_entry_supplies_what_the_request_omits() {
        let config = Config {
            frontends: FrontendSettings {
                dir: Config::for_tests().frontends.dir,
                available: vec![FrontendConfig {
                    name: "plume".to_owned(),
                    git: Some("https://example.org/plume".to_owned()),
                    build_url: Some("https://build.example/${ref}.zip".to_owned()),
                    reference: Some("stable".to_owned()),
                    build_dir: Some("output".to_owned()),
                }],
            },
            ..Config::for_tests()
        };

        let from_config = resolved(
            &config,
            &InstallParams {
                name: Some("plume".to_owned()),
                ..InstallParams::default()
            },
            false,
        );
        assert_eq!(from_config.reference, "stable", "the config ref");
        assert_eq!(
            from_config.build_dir.as_deref(),
            Some("output"),
            "the config build dir"
        );

        let overridden = resolved(
            &config,
            &InstallParams {
                name: Some("plume".to_owned()),
                reference: Some("nightly".to_owned()),
                build_url: None,
                build_dir: None,
            },
            false,
        );
        assert_eq!(overridden.reference, "nightly", "the request wins");

        let no_ref = Config {
            frontends: FrontendSettings {
                dir: Config::for_tests().frontends.dir,
                available: vec![FrontendConfig {
                    name: "bare".to_owned(),
                    git: None,
                    build_url: Some("https://build.example/x.zip".to_owned()),
                    reference: None,
                    build_dir: None,
                }],
            },
            ..Config::for_tests()
        };
        let defaulted = resolved(
            &no_ref,
            &InstallParams {
                name: Some("bare".to_owned()),
                ..InstallParams::default()
            },
            false,
        );
        assert_eq!(defaulted.reference, DEFAULT_REF, "the ref defaults");
        assert_eq!(
            defaulted.build_dir, None,
            "and no build dir means the dist-then-root choice"
        );
    }

    #[test]
    fn a_known_name_without_any_source_is_refused() {
        let config = config_with_frontends(&[("plume", Some("stable"))]);
        let err = resolve_install(
            &config,
            &InstallParams {
                name: Some("plume".to_owned()),
                ..InstallParams::default()
            },
            false,
        )
        .expect_err("no url and no file installs nothing");
        assert!(
            is_install_failure(&err, "no build_url"),
            "the source-less refusal: {err:?}"
        );
    }

    #[test]
    fn a_known_name_with_an_uploaded_file_needs_no_build_url() {
        let config = config_with_frontends(&[("plume", Some("stable"))]);
        let install = resolved(
            &config,
            &InstallParams {
                name: Some("plume".to_owned()),
                ..InstallParams::default()
            },
            true,
        );
        assert_eq!(install.build_url, None, "the config entry carries no url");
        assert_eq!(install.reference, "stable", "the config ref still stands");
    }

    #[test]
    fn unsafe_names_refs_and_build_dirs_are_refused() {
        let config = Config::for_tests();
        let base = InstallParams {
            name: Some("../../etc".to_owned()),
            reference: None,
            build_url: Some("https://build.example/x.zip".to_owned()),
            build_dir: None,
        };
        let name_refusal = resolve_install(&config, &base, false).expect_err("a traversal name");
        assert!(
            is_install_failure(&name_refusal, "the frontend name"),
            "{name_refusal:?}"
        );

        let slashy = InstallParams {
            name: Some("fe".to_owned()),
            reference: Some("a/b".to_owned()),
            build_url: Some("https://build.example/x.zip".to_owned()),
            build_dir: None,
        };
        let ref_refusal = resolve_install(&config, &slashy, false).expect_err("a slashed ref");
        assert!(
            is_install_failure(&ref_refusal, "the frontend ref"),
            "{ref_refusal:?}"
        );

        let escaping = InstallParams {
            name: Some("fe".to_owned()),
            reference: None,
            build_url: Some("https://build.example/x.zip".to_owned()),
            build_dir: Some("../dist".to_owned()),
        };
        let dir_refusal =
            resolve_install(&config, &escaping, false).expect_err("a traversing build dir");
        assert!(
            is_install_failure(&dir_refusal, "the build directory"),
            "{dir_refusal:?}"
        );
    }

    #[test]
    fn archive_entry_names_walk_the_ticket_gate() {
        assert_eq!(
            safe_entry_name("dist/index.html").expect("a plain path stands"),
            "dist/index.html"
        );
        assert!(
            safe_entry_name(".hidden").is_ok(),
            "a dotfile is no traversal"
        );
        for bad in [
            "",
            "/",
            "/absolute",
            "../escape",
            "a/../../b",
            "a//b",
            "./x",
            "C:\\evil",
        ] {
            let err = safe_entry_name(bad).expect_err(&format!("{bad:?} must refuse"));
            assert!(
                is_install_failure(&err, "the archive entry"),
                "the pinned refusal shape for {bad:?}"
            );
        }
    }

    /// A staging tree with `dist/` and root files, for the source choice.
    fn staged_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a staging dir");
        std::fs::create_dir_all(dir.path().join("dist")).expect("the dist dir");
        std::fs::write(dir.path().join("dist/index.html"), b"d").expect("the dist file");
        std::fs::write(dir.path().join("index.html"), b"r").expect("the root file");
        dir
    }

    #[test]
    fn the_installable_source_prefers_the_named_dist() {
        let staging = staged_tree();
        assert_eq!(
            installable_source(staging.path(), Some("dist")),
            staging.path().join("dist"),
            "the named subtree, when it is there"
        );
        assert_eq!(
            installable_source(staging.path(), None),
            staging.path().join("dist"),
            "and the default dist, when the archive carries one"
        );
        assert_eq!(
            installable_source(staging.path(), Some("webout")),
            staging.path().to_path_buf(),
            "a named subtree the archive lacks falls back to the root"
        );
    }

    #[test]
    fn landing_replaces_the_earlier_install() {
        let root = tempfile::tempdir().expect("a frontends dir");
        let source = root.path().join("staged");
        let destination = root.path().join("fe").join("v1");
        std::fs::create_dir_all(&source).expect("the source dir");
        std::fs::write(source.join("new.js"), b"new").expect("the new file");
        std::fs::create_dir_all(&destination).expect("the old destination");
        std::fs::write(destination.join("old.js"), b"old").expect("the old file");

        land_directory(&source, &destination).expect("the landing runs");

        assert!(!source.exists(), "the staging tree renamed away");
        assert!(
            !destination.join("old.js").exists(),
            "the old install is gone"
        );
        assert!(
            destination.join("new.js").is_file(),
            "the new tree stands in its place"
        );
    }

    #[test]
    fn the_frontends_read_both_build_paths() {
        let root = tempfile::tempdir().expect("a frontends dir");
        let config = Config {
            frontends: FrontendSettings {
                dir: root.path().to_path_buf(),
                available: vec![
                    FrontendConfig {
                        name: "plume".to_owned(),
                        git: None,
                        build_url: None,
                        reference: Some("stable".to_owned()),
                        build_dir: None,
                    },
                    FrontendConfig {
                        name: "soap".to_owned(),
                        git: None,
                        build_url: None,
                        reference: Some("master".to_owned()),
                        build_dir: None,
                    },
                    FrontendConfig {
                        name: "bare".to_owned(),
                        git: None,
                        build_url: None,
                        reference: None,
                        build_dir: None,
                    },
                ],
            },
            ..Config::for_tests()
        };
        std::fs::create_dir_all(root.path().join("plume").join("stable"))
            .expect("the installed dir");
        std::fs::write(
            root.path().join("plume").join("stable").join("index.html"),
            b"",
        )
        .expect("the installed file");
        std::fs::create_dir_all(root.path().join("bare")).expect("the bare dir");

        assert!(
            frontend_installed(&config, &config.frontends.available[0]),
            "plume/stable stands"
        );
        assert!(
            !frontend_installed(&config, &config.frontends.available[1]),
            "soap/master was never built"
        );
        assert!(
            frontend_installed(&config, &config.frontends.available[2]),
            "a refless entry reads its name directory"
        );
    }

    /// The entity renders exactly the ticket's five keys, `ref` spelled
    /// as JSON demands and the nulls as nulls.
    #[test]
    fn a_frontend_entity_carries_the_five_pinned_keys() {
        let entry = FrontendConfig {
            name: "plume".to_owned(),
            git: Some("https://example.org/plume".to_owned()),
            build_url: None,
            reference: Some("stable".to_owned()),
            build_dir: Some("dist".to_owned()),
        };
        let rendered = serde_json::to_value(AdminFrontend::from_config(&entry, true))
            .expect("the entity serializes");
        assert_eq!(
            rendered,
            json!({
                "name": "plume",
                "git": "https://example.org/plume",
                "build_url": null,
                "ref": "stable",
                "installed": true,
            }),
            "build_dir stays server-side, ref is the JSON key"
        );
    }

    /// A minimal valid archive, and one unsafe entry, for the reader.
    fn archive_with(name: &str, bytes: &[u8]) -> Vec<u8> {
        use std::io::Write as _;

        use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file(name, options).expect("entry starts");
        writer.write_all(bytes).expect("bytes write");
        let cursor = writer.finish().expect("the fixture archive closes");
        cursor.into_inner()
    }

    #[test]
    fn the_archive_reader_gates_names_and_returns_files() {
        let bytes = archive_with("dist/index.html", b"hello");
        let entries = read_zip(&bytes).expect("the fixture archive reads");
        assert_eq!(entries.len(), 1, "the one file entry");
        assert_eq!(entries[0].0, "dist/index.html", "under its own name");
        assert_eq!(entries[0].1, b"hello", "with its bytes");

        let slip_err = read_zip(&archive_with("../evil.txt", b"x")).expect_err("a zip-slip entry");
        assert!(
            is_install_failure(&slip_err, "the archive entry"),
            "{slip_err:?}"
        );
        let absolute_err =
            read_zip(&archive_with("/etc/passwd", b"x")).expect_err("an absolute entry");
        assert!(
            is_install_failure(&absolute_err, "the archive entry"),
            "{absolute_err:?}"
        );
    }

    #[test]
    fn an_unreadable_archive_is_the_install_refusal() {
        let err = read_zip(b"not a zip at all").expect_err("junk cannot open");
        assert!(is_install_failure(&err, "the archive"), "{err:?}");
    }
}
