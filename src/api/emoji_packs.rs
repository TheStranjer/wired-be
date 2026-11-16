//! Pleroma's emoji-pack endpoints, under `/api/v1/pleroma/emoji`
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! These are the public and admin sides of
//! `Pleroma.Web.PleromaAPI.EmojiPackController`
//! (`lib/pleroma/web/pleroma_api/controllers/emoji_pack_controller.ex`)
//! plus Pleroma's util emoji map (`UtilController.emoji`,
//! `lib/pleroma/web/pleroma_api/controllers/util_controller.ex`).
//!
//! The four read endpoints answer without authentication — the Pleroma
//! controller `plug`s `skip_auth` for `index`, `show` and `archive` — and
//! read the packs from `EMOJI_DIR` through [`crate::services::emoji_packs`];
//! the emoji map reads the `custom_emojis` table that
//! [`sync_custom_emojis`](crate::services::emoji_packs::sync_custom_emojis)
//! keeps in step with the packs.
//!
//! - `GET /` — the util emoji map: every local custom emoji as
//!   `{"shortcode": {"image_url": …, "tags": […]}}`.
//! - `GET /packs` — `{"count": …, "packs": …}` over every pack, ordered and
//!   paginated by pack name.
//! - `GET /packs/archive?name=…` — the pack as a zip download, only for
//!   packs that share their files.
//! - `GET /pack?name=…` — one pack, its files paginated by shortcode.
//!
//! The three write routes (WBE-211) are the controller's `create`,
//! `update` and `delete` actions: gated by [`AdminUser`] plus the
//! `admin:write` scope Pleroma's `OAuthScopesPlug` demands, each followed
//! by a
//! [`sync_custom_emojis`](crate::services::emoji_packs::sync_custom_emojis)
//! resync and an `emoji_pack_*` [`moderation_log`] entry.
//!
//! - `POST /pack?name=…` (the name also accepted in the body) — create the
//!   pack directory and its default manifest; an occupied name is the
//!   ticket's 400 `{"error":"Pack already exists"}` (Pleroma's 409).
//! - `PATCH /pack?name=…` with body `metadata` — merge the given keys into
//!   the manifest's `pack` block (fallback archives are stored as named,
//!   never fetched) and answer the resulting metadata object.
//! - `DELETE /pack?name=…` — remove the pack directory recursively.
//!
//! Unknown or unusable pack names are one and the same 404 on every route:
//! the reads answer `{"error":"Pack not found"}`, the writes Pleroma's
//! `{"error":"Pack <name> does not exist"}` — the endpoints cannot be used
//! to probe the filesystem, and `[A-Za-z0-9_.-]`-validated names keep
//! every path inside `EMOJI_DIR`. The archive's refusals follow the ticket
//! exactly: 403 `{"error":"Pack is not shared"}` for a pack that does not
//! share its files, 400 for a missing `name` parameter (Pleroma's `"pack
//! name cannot be empty"`), which the writes share.
//!
//! [`files_router`] is the separate top-level `GET /emoji/{pack}/{file}`:
//! the address [`emoji_image_url`](crate::services::emoji_packs::emoji_image_url)
//! stores in `custom_emojis.image_url`. Like [`crate::api::media_files`] it
//! serves from the instance's own origin, so the type comes from the
//! extension and only image types are offered at all — anything else (the
//! `pack.json` manifests included) answers the same 404 as an absent file.
//!
//! The three file routes (WBE-215) are the controller's `create`,
//! `update` and `delete` actions under `/packs/files`, gated by
//! [`AdminUser`] plus `admin:write` like the pack writes. They take the
//! pack through the query or the body, refuse an unknown or unusable one
//! with the 400 `{"error":"Pack <name> does not exist"}`, and each answer
//! the pack's full `files` object after a
//! [`sync_custom_emojis`](crate::services::emoji_packs::sync_custom_emojis)
//! resync.
//!
//! - `POST /packs/files` — `shortcode` (required, `[A-Za-z0-9_-]+`, new in
//!   the pack else 409), `file` (an uploaded image part, or a URL the
//!   SSRF-guarded [`HttpClient`](crate::http_client::HttpClient) fetches;
//!   at most 2 MiB, png/gif/jpeg/webp by magic bytes — SVG has no magic
//!   bytes and the static route never serves it) and an optional
//!   `filename` (default `<shortcode>.<ext>`, validated as a plain file
//!   name). The bytes land in the pack directory and the entry in
//!   `pack.json`.
//! - `PATCH /packs/files` — rename the shortcode and/or the file of an
//!   entry; a new shortcode already used by another entry is the 409
//!   unless `force`, an unknown one the 400
//!   `{"error":"Emoji \"<shortcode>\" does not exist"}`.
//! - `DELETE /packs/files` — remove an entry and its file, the same 400
//!   for an unknown shortcode.
//!
//! [`moderation_log`]: crate::models::moderation_log

use std::{
    collections::BTreeMap,
    convert::Infallible,
    future::{self, Future},
};

use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, State},
    http::{
        HeaderValue,
        header::{self, CONTENT_DISPOSITION},
        request::Parts,
    },
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;
use url::form_urlencoded;

use crate::{
    auth::admin::AdminUser,
    config::Config,
    error::{AppError, AppResult},
    extract::{
        ApiParams, flex,
        multipart::{MultipartForm, UploadedFile, UploadedFiles as _, upload_body_limit},
    },
    http_client::HttpError,
    models::{custom_emoji, moderation_log},
    services::emoji_packs,
    state::AppState,
    uploads,
};

/// Default page number when `page` is absent or not a number (Pleroma's
/// `page` default, `lib/pleroma/emoji/pack.ex` `paginate/3`).
const DEFAULT_PAGE: usize = 1;
/// Default page size when `page_size` is absent or not a number.
const DEFAULT_PAGE_SIZE: usize = 30;
/// Largest page `page_size` may ask for; anything above is clamped down.
const MAX_PAGE_SIZE: usize = 100;

/// 404 body of the pack endpoints, for unknown and unusable names alike.
const PACK_NOT_FOUND: &str = "Pack not found";
/// 400 body when the required `name` parameter is absent or empty
/// (Pleroma's `pack name cannot be empty`).
const NAME_EMPTY: &str = "pack name cannot be empty";
/// The write endpoint class of [`AdminUser::require`].
const CLASS_WRITE: &str = "write";
/// The scope area of the pack writes: Pleroma's controller demands the
/// `admin:write` root, and [`AdminUser::require`] also accepts its narrow
/// `admin:write:emoji` alias.
const AREA_EMOJI: &str = "emoji";
/// 200 body of the create and delete routes — Pleroma's `json(conn, "ok")`.
const OK_BODY: &str = "ok";
/// `content-type` of the archive download.
const ZIP_CONTENT_TYPE: &str = "application/zip";
/// `cache-control` for pack images: packs are edited in place (unlike the
/// random upload keys), so images are cacheable for a day, not `immutable`.
const CACHE_CONTROL: &str = "public, max-age=86400";
/// `x-content-type-options`: the type sent here is the type to use.
const NOSNIFF: &str = "nosniff";
/// Pack images render inline; nothing else this route could name is served.
const INLINE: &str = "inline";
/// Largest emoji file the `/packs/files` create route accepts, uploaded
/// or fetched: the ticket's 2 MiB.
const MAX_EMOJI_FILE_BYTES: usize = 2 * 1024 * 1024;
/// The image types magic bytes can name for an emoji file. SVG is
/// deliberately absent: it has no magic bytes to read, and the static
/// pack route of this module refuses to serve it either, so accepting it
/// here could only store a file that never renders.
const ACCEPTED_EMOJI_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
/// 422 body for bytes [`detect_emoji_type`] cannot name an accepted image
/// type for (the wording the media and profile uploads use alike).
const UNSUPPORTED_FILE_TYPE: &str = "Unsupported file type";
/// 422 body when the create request carries no file at all, as Pleroma's
/// spec requires one (its own missing-parameter wording, kept by
/// [`crate::api::media`]).
const MISSING_FILE: &str = "Missing parameter: file";
/// The body parameter (and multipart field) the file arrives under.
const FILE_FIELD: &str = "file";
/// 422 body when a rename sends `new_shortcode` or `new_filename` empty
/// (Pleroma's `:empty_values` arm of the update).
const RENAME_EMPTY: &str = "new_shortcode or new_filename cannot be empty";
/// The lowest response status a fetched file URL may answer with
/// (Pleroma's `code in 200..299`).
const MIN_FETCH_STATUS: u16 = 200;
/// One past the highest response status a fetched file URL may answer
/// with.
const MAX_FETCH_STATUS: u16 = 300;

/// The `/api/v1/pleroma/emoji` router, nested by [`crate::api::v1`].
///
/// `config` is what [`upload_body_limit`] sizes the `/packs/files` create
/// route's body limit from: axum's multipart extractor refuses bodies
/// over 2 MiB unless the route raises that first, and the endpoint takes
/// uploaded image files.
pub fn router(config: &Config) -> Router<AppState> {
    Router::new()
        .route("/", get(emoji_map))
        .route(
            "/pack",
            get(show).post(create).patch(update).delete(destroy),
        )
        .route("/packs", get(index))
        .route("/packs/archive", get(archive))
        .route(
            "/packs/files",
            post(create_file)
                .patch(update_file)
                .delete(delete_file)
                .layer(upload_body_limit(config)),
        )
}

/// The static `GET /emoji/{pack}/{file}` router, merged at the top level of
/// [`crate::app`]: synced `custom_emojis.image_url` values point there.
pub fn files_router() -> Router<AppState> {
    Router::new().route("/emoji/{pack}/{file}", get(serve_emoji_file))
}

/// The `name`, `page` and `page_size` query parameters, parsed the way
/// [`crate::pagination::PageParams`] parses its keys: `name` is kept raw
/// for the endpoints to trim and validate, numbers that are absent or not
/// positive whole numbers fall back to the defaults, `page_size` is capped
/// at [`MAX_PAGE_SIZE`], the last occurrence of a repeated key wins and
/// unrelated keys are ignored.
#[derive(Debug, Clone)]
struct PackParams {
    /// Raw `name` value, if present.
    name: Option<String>,
    /// 1-based page number, defaulted.
    page: usize,
    /// Page size, defaulted and capped at [`MAX_PAGE_SIZE`].
    page_size: usize,
}

impl PackParams {
    /// The trimmed pack `name`; the endpoints' 400 when it is absent or
    /// blank (Pleroma's `validate_not_empty([name])`).
    fn required_name(&self) -> AppResult<&str> {
        let name = self.name.as_deref().unwrap_or_default().trim();
        if name.is_empty() {
            return Err(AppError::BadRequest(NAME_EMPTY.to_owned()));
        }
        Ok(name)
    }
}

/// A query-string positive whole number: `None` for absent text, text
/// that is not a number, and zero (the endpoints fall back to their
/// defaults, as [`crate::pagination::PageParams`] does for `limit`).
fn number(text: &str) -> Option<usize> {
    text.parse().ok().filter(|value| *value > 0)
}

impl<S> FromRequestParts<S> for PackParams
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let mut name = None;
        let mut page = None;
        let mut page_size = None;
        for (key, value) in form_urlencoded::parse(parts.uri.query().unwrap_or_default().as_bytes())
        {
            match key.as_ref() {
                "name" => name = Some(value.into_owned()),
                "page" => page = number(&value),
                "page_size" => page_size = number(&value),
                _ => {}
            }
        }
        future::ready(Ok(Self {
            name,
            page: page.unwrap_or(DEFAULT_PAGE),
            page_size: page_size.unwrap_or(DEFAULT_PAGE_SIZE).min(MAX_PAGE_SIZE),
        }))
    }
}

/// The `page`-th 1-based window of `page_size` items of an ordered
/// iterator — Pleroma's `paginate/3` as iterators, empty page beyond the
/// end rather than an error.
fn take_page<I>(items: I, page: usize, page_size: usize) -> Vec<I::Item>
where
    I: Iterator,
{
    let offset = page.saturating_sub(1).saturating_mul(page_size);
    items.skip(offset).take(page_size).collect()
}

/// One pack as the endpoints serialize it: Pleroma's `@derive
/// {Jason.Encoder, only: [:files, :pack, :files_count]}` — `files` may be
/// the paginated window while `files_count` always counts the whole pack.
#[derive(Debug, Serialize)]
struct PackView {
    /// The manifest metadata block.
    pack: emoji_packs::PackMeta,
    /// The pack's files (full list on `/packs`, one page on `/pack`).
    files: BTreeMap<String, String>,
    /// Total files in the pack, before pagination.
    files_count: usize,
}

impl PackView {
    /// The view carrying all of `pack`'s files.
    fn full(pack: emoji_packs::Pack) -> Self {
        let files_count = pack.files.len();
        Self {
            pack: pack.pack,
            files: pack.files,
            files_count,
        }
    }

    /// The view carrying `pack`'s files paginated by shortcode (the map is
    /// ordered, matching Pleroma's `Enum.sort` of the keys).
    fn paginated(pack: emoji_packs::Pack, page: usize, page_size: usize) -> Self {
        let files_count = pack.files.len();
        Self {
            pack: pack.pack,
            files: take_page(pack.files.into_iter(), page, page_size)
                .into_iter()
                .collect(),
            files_count,
        }
    }
}

/// The `/packs` response body: the total pack count and one page of packs
/// keyed by name.
#[derive(Debug, Serialize)]
struct PacksResponse {
    /// Number of packs on disk, across all pages.
    count: usize,
    /// The current page, keyed by pack name.
    packs: BTreeMap<String, PackView>,
}

impl PacksResponse {
    /// The paginated listing over every loaded pack (`all` is name-ordered
    /// by [`list_packs`](emoji_packs::list_packs)).
    fn new(all: Vec<(String, emoji_packs::Pack)>, page: usize, page_size: usize) -> Self {
        Self {
            count: all.len(),
            packs: take_page(all.into_iter(), page, page_size)
                .into_iter()
                .map(|(name, pack)| (name, PackView::full(pack)))
                .collect(),
        }
    }
}

/// One entry of the util emoji map: the image address and the picker tags,
/// Pleroma's `%{image_url: file, tags: tags}` in `UtilController.emoji`.
#[derive(Debug, Serialize)]
struct EmojiMapEntry {
    /// The stored image url of the emoji.
    image_url: String,
    /// The emoji's tags; synced pack emojis carry their pack name.
    tags: Vec<String>,
}

/// `GET /api/v1/pleroma/emoji` — the util emoji map: every local, enabled
/// custom emoji keyed by shortcode. No authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn emoji_map(
    State(state): State<AppState>,
) -> AppResult<Json<BTreeMap<String, EmojiMapEntry>>> {
    let rows = custom_emoji::list_local(&state.db).await?;
    Ok(Json(
        rows.iter()
            .map(|row| {
                (
                    row.shortcode.clone(),
                    EmojiMapEntry {
                        image_url: row.image_url.clone(),
                        tags: row.tags.clone(),
                    },
                )
            })
            .collect(),
    ))
}

/// `GET /api/v1/pleroma/emoji/packs` — every pack, ordered by name, one
/// page per request. No authentication.
///
/// # Errors
///
/// [`AppError::Internal`] for an unlistable packs directory (through
/// [`list_packs`](emoji_packs::list_packs)).
async fn index(
    State(state): State<AppState>,
    params: PackParams,
) -> AppResult<Json<PacksResponse>> {
    let all = emoji_packs::list_packs(&state.config.emoji_dir).await?;
    Ok(Json(PacksResponse::new(all, params.page, params.page_size)))
}

/// `GET /api/v1/pleroma/emoji/pack?name=…` — one pack with its files
/// paginated by shortcode. No authentication.
///
/// # Errors
///
/// [`AppError::BadRequest`] for a missing or empty `name`,
/// [`AppError::NotFoundMessage`] `{"error":"Pack not found"}` for an
/// unknown or unusable pack; other manifest failures arrive as
/// [`AppError::Internal`].
async fn show(State(state): State<AppState>, params: PackParams) -> AppResult<Json<PackView>> {
    let pack = load_or_404(&state, params.required_name()?).await?;
    Ok(Json(PackView::paginated(
        pack,
        params.page,
        params.page_size,
    )))
}

/// `GET /api/v1/pleroma/emoji/packs/archive?name=…` — the pack as a zip
/// download, for packs that share their files. No authentication.
///
/// # Errors
///
/// [`AppError::BadRequest`] for a missing or empty `name`,
/// [`AppError::NotFoundMessage`] for an unknown or unusable pack,
/// [`AppError::Forbidden`] ([`emoji_packs::PACK_NOT_SHARED`]) when the pack
/// does not share its files (or cannot: a listed file is off disk).
async fn archive(State(state): State<AppState>, params: PackParams) -> AppResult<Response> {
    let name = params.required_name()?;
    if !emoji_packs::pack_name_valid(name) {
        return Err(pack_not_found());
    }
    match emoji_packs::pack_archive(&state.config.emoji_dir, name).await {
        Ok(bytes) => zip_response(name, bytes),
        Err(AppError::NotFound) => Err(pack_not_found()),
        Err(err) => Err(err),
    }
}

/// Loads pack `name`, folding a missing manifest and an unusable name into
/// the endpoints' shared 404 so neither reveals which is which.
async fn load_or_404(state: &AppState, name: &str) -> AppResult<emoji_packs::Pack> {
    if !emoji_packs::pack_name_valid(name) {
        return Err(pack_not_found());
    }
    let loaded = emoji_packs::load_pack(&state.config.emoji_dir, name).await;
    match loaded {
        Ok(pack) => Ok(pack),
        Err(AppError::NotFound) => Err(pack_not_found()),
        Err(err) => Err(err),
    }
}

/// The 404 every pack endpoint owes for a name it will not serve.
fn pack_not_found() -> AppError {
    AppError::NotFoundMessage(PACK_NOT_FOUND.to_owned())
}

/// The zip download response for `name`'s archive bytes: `application/zip`
/// and `attachment; filename="<name>.zip"`, Pleroma's `send_download`. The
/// caller has validated `name` against [`emoji_packs::pack_name_valid`], so
/// the header value is ASCII and cannot break out of its quotes.
///
/// # Errors
///
/// [`AppError::Internal`] if the disposition bytes were rejected anyway.
fn zip_response(name: &str, bytes: Vec<u8>) -> AppResult<Response> {
    let disposition = HeaderValue::from_str(&format!(r#"attachment; filename="{name}.zip""#))
        .map_err(|err| AppError::Internal(format!("invalid archive disposition: {err}")))?;
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(ZIP_CONTENT_TYPE),
            ),
            (CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    )
        .into_response())
}

/// The `name` parameter of the create route, from the query string or the
/// body ([`ApiParams`] merges the two, the body wins); the handler trims
/// and demands it, as the reads' [`PackParams::required_name`] does.
#[derive(Debug, Deserialize)]
struct CreateParams {
    /// Raw pack `name` value, if present.
    #[serde(default)]
    name: Option<String>,
}

/// The `name` parameter and `metadata` body of the update route.
#[derive(Debug, Deserialize)]
struct UpdateParams {
    /// Raw pack `name` value, if present.
    #[serde(default)]
    name: Option<String>,
    /// The metadata to merge into the pack's manifest block; required, as
    /// Pleroma's `update` pattern-matches on it.
    metadata: MetadataPatch,
}

/// A `metadata` body: the keys of Pleroma's `update_request` schema, every
/// one optional. The dashed names are [`PackMeta`]'s manifest spellings;
/// `share-files` reads through [`flex::opt_bool`], so a form-encoded
/// `"true"` counts and anything that is no boolean at all is refused as
/// the extractor's 422.
///
/// [`PackMeta`]: emoji_packs::PackMeta
#[derive(Debug, Deserialize)]
struct MetadataPatch {
    /// The pack's license identifier.
    #[serde(default)]
    license: Option<String>,
    /// The pack's home page URL.
    #[serde(default)]
    homepage: Option<String>,
    /// Free-form description of the pack.
    #[serde(default)]
    description: Option<String>,
    /// URL of an external archive holding the pack's files; stored as
    /// given, never fetched.
    #[serde(default, rename = "fallback-src")]
    fallback_src: Option<String>,
    /// SHA-256 of the fallback archive, lowercase hex.
    #[serde(default, rename = "fallback-src-sha256")]
    fallback_src_sha256: Option<String>,
    /// Whether the pack may hand its files to others.
    #[serde(default, rename = "share-files", deserialize_with = "flex::opt_bool")]
    share_files: Option<bool>,
}

/// Merges `patch` into stored metadata: every key the request carried
/// replaces what stands, every key it left out keeps its stored value.
fn merge_metadata(into: &mut emoji_packs::PackMeta, patch: MetadataPatch) {
    if let Some(license) = patch.license {
        into.license = Some(license);
    }
    if let Some(homepage) = patch.homepage {
        into.homepage = Some(homepage);
    }
    if let Some(description) = patch.description {
        into.description = Some(description);
    }
    if let Some(fallback_src) = patch.fallback_src {
        into.fallback_src = Some(fallback_src);
    }
    if let Some(fallback_src_sha256) = patch.fallback_src_sha256 {
        into.fallback_src_sha256 = Some(fallback_src_sha256);
    }
    if let Some(share_files) = patch.share_files {
        into.share_files = share_files;
    }
}

/// The trimmed pack `name` a write route received from the query or the
/// body; absent or blank is Pleroma's empty-value 400, shared with the
/// reads' [`PackParams::required_name`].
fn trimmed_name(raw: Option<&str>) -> AppResult<&str> {
    let name = raw.unwrap_or_default().trim();
    if name.is_empty() {
        return Err(AppError::BadRequest(NAME_EMPTY.to_owned()));
    }
    Ok(name)
}

/// The 404 the write routes owe a name they will not serve, Pleroma's
/// `"Pack <name> does not exist"` (the controller's `:enoent` arms). Like
/// the reads' shared 404 it covers an absent pack and an unusable name
/// alike, so a write cannot probe the filesystem either.
fn pack_missing(name: &str) -> AppError {
    AppError::NotFoundMessage(format!("Pack {name} does not exist"))
}

/// Folds a service refusal that means "there is no pack here to write" —
/// [`AppError::NotFound`] for an absent directory, [`AppError::BadRequest`]
/// for a name [`emoji_packs::pack_name_valid`] rejects — into
/// [`pack_missing`]; every other failure arrives unchanged.
fn write_miss(name: &str, err: AppError) -> AppError {
    if matches!(err, AppError::NotFound | AppError::BadRequest(_)) {
        return pack_missing(name);
    }
    err
}

/// Loads pack `name` for a write, refusing with [`pack_missing`] what is
/// not there to write.
async fn load_for_write(state: &AppState, name: &str) -> AppResult<emoji_packs::Pack> {
    let loaded = emoji_packs::load_pack(&state.config.emoji_dir, name).await;
    loaded.map_err(|err| write_miss(name, err))
}

/// Reconciles `custom_emojis` with the packs directory after a write, so
/// `GET /api/v1/custom_emojis` and the util emoji map see the change.
///
/// # Errors
///
/// Whatever [`sync_custom_emojis`](emoji_packs::sync_custom_emojis) fails
/// with; its stored count is logged, not answered.
async fn resync(state: &AppState) -> AppResult<()> {
    let stored: usize =
        emoji_packs::sync_custom_emojis(&state.db, &state.config.base_url, &state.config.emoji_dir)
            .await?;
    debug!(emojis = stored, "emoji pack write resynced custom emojis");
    Ok(())
}

/// `POST /api/v1/pleroma/emoji/pack?name=…` — Pleroma's `create`: the
/// pack's directory and a default `{"pack": {}, "files": {}}` manifest,
/// the name also accepted in the body. An occupied name answers the
/// ticket's 400 `{"error":"Pack already exists"}` (Pleroma's 409), an
/// absent or blank one its 400 `"pack name cannot be empty"`. On success
/// the emojis resync and an `emoji_pack_create` moderation entry records
/// the act; the answer is the JSON string `"ok"`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] from the name
/// checks; [`AppError::Internal`] for a filesystem failure;
/// [`AppError::Database`] from the resync and the log write.
async fn create(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiParams(params): ApiParams<CreateParams>,
) -> AppResult<Json<&'static str>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let name = trimmed_name(params.name.as_deref())?;
    emoji_packs::create_pack(&state.config.emoji_dir, name).await?;
    resync(&state).await?;
    let entry = moderation_log::emoji_pack_create(&state.db, &admin.0, name).await?;
    debug!(id = %entry.id, pack = name, "emoji pack created");
    Ok(Json(OK_BODY))
}

/// `PATCH /api/v1/pleroma/emoji/pack?name=…` — Pleroma's `update`: the
/// body's `metadata` merged into the pack manifest's `pack` block, absent
/// keys keeping their stored values. A `fallback-src` is stored as given —
/// the archive is never fetched or checked here — and `share-files` must
/// be a boolean. An unknown or unusable pack is the 404
/// `{"error":"Pack <name> does not exist"}`. On success the emojis resync,
/// an `emoji_pack_update` moderation entry records the act, and the
/// resulting metadata object answers.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] for an absent or
/// blank `name`; [`AppError::Unprocessable`] for a non-object `metadata`
/// or a non-boolean `share-files` (the [`ApiParams`] extractor's refusals);
/// [`pack_missing`]'s 404 for a pack that is not there to write;
/// [`AppError::Internal`] for a filesystem failure; [`AppError::Database`]
/// from the resync and the log write.
async fn update(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiParams(params): ApiParams<UpdateParams>,
) -> AppResult<Json<emoji_packs::PackMeta>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let name = trimmed_name(params.name.as_deref())?;
    let mut pack = load_for_write(&state, name).await?;
    merge_metadata(&mut pack.pack, params.metadata);
    emoji_packs::save_pack(&state.config.emoji_dir, name, &pack).await?;
    resync(&state).await?;
    let entry = moderation_log::emoji_pack_update(&state.db, &admin.0, name).await?;
    debug!(id = %entry.id, pack = name, "emoji pack metadata updated");
    Ok(Json(pack.pack))
}

/// `DELETE /api/v1/pleroma/emoji/pack?name=…` — Pleroma's `delete`: the
/// pack's whole directory, files and manifest, removed from `EMOJI_DIR`;
/// an unusable name can never name a directory and answers the same 404
/// as an absent pack. On success the emojis resync — the pack's shortcodes
/// leave `custom_emojis` with it — an `emoji_pack_delete` moderation entry
/// records the act, and the answer is the JSON string `"ok"`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] for an absent or
/// blank `name`; [`pack_missing`]'s 404 for a pack that is not there;
/// [`AppError::Internal`] for a filesystem failure; [`AppError::Database`]
/// from the resync and the log write.
async fn destroy(
    State(state): State<AppState>,
    admin: AdminUser,
    params: PackParams,
) -> AppResult<Json<&'static str>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let name = params.required_name()?;
    let removed = emoji_packs::delete_pack(&state.config.emoji_dir, name).await;
    removed.map_err(|err| write_miss(name, err))?;
    resync(&state).await?;
    let entry = moderation_log::emoji_pack_delete(&state.db, &admin.0, name).await?;
    debug!(id = %entry.id, pack = name, "emoji pack deleted");
    Ok(Json(OK_BODY))
}

/// The body parameters of `POST /packs/files`. `file` is the URL spelling
/// of the image (form or JSON value); an uploaded multipart file part
/// arrives in [`MultipartForm::files`] under the same name instead and
/// wins, as a browser's `FormData` sends it.
#[derive(Debug, Deserialize)]
struct CreateFileParams {
    /// Raw pack `name` value, if present.
    #[serde(default)]
    name: Option<String>,
    /// The required shortcode of the new emoji.
    shortcode: String,
    /// The optional stored file name; defaulted from the shortcode.
    #[serde(default)]
    filename: Option<String>,
    /// The `file` parameter: a URL to fetch when no file part arrived.
    #[serde(default)]
    file: Option<Value>,
}

/// The body parameters of `PATCH /packs/files`: the entry to rename with
/// its optional new shortcode and new file name, and whether an occupied
/// new shortcode may be overwritten. The names and the flexible `force`
/// follow Pleroma's `update_request` schema.
#[derive(Debug, Deserialize)]
struct UpdateFileParams {
    /// Raw pack `name` value, if present.
    #[serde(default)]
    name: Option<String>,
    /// The required shortcode of the existing entry.
    shortcode: String,
    /// The shortcode the entry gets; absent keeps it.
    #[serde(default)]
    new_shortcode: Option<String>,
    /// The file name the entry gets; absent keeps it.
    #[serde(default)]
    new_filename: Option<String>,
    /// Whether a new shortcode already used by another entry may
    /// overwrite that entry.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    force: Option<bool>,
}

/// The body and query parameters of `DELETE /packs/files`.
#[derive(Debug, Deserialize)]
struct DeleteFileParams {
    /// Raw pack `name` value, if present.
    #[serde(default)]
    name: Option<String>,
    /// The required shortcode of the entry to remove.
    shortcode: String,
}

/// One [`update_file`](self::update_file) rename, assembled from the
/// request: the old entry's shortcode with what it should become.
#[derive(Debug)]
struct Rename<'a> {
    /// The existing shortcode to rename.
    shortcode: &'a str,
    /// The shortcode the entry gets; `None` keeps the old one.
    new_shortcode: Option<&'a str>,
    /// The file name the entry gets; `None` keeps the stored one.
    new_filename: Option<&'a str>,
    /// Whether an occupied new shortcode may be overwritten.
    force: bool,
}

/// Whether `code` may be an emoji shortcode: a non-empty run of
/// `[A-Za-z0-9_-]`, the pattern the ticket pins. It is deliberately
/// stricter than a pack name — no dots, no separators — so a shortcode
/// can neither hide an extension nor name a path.
fn shortcode_valid(code: &str) -> bool {
    !code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The 400 the file routes owe a pack that is not there to write, the
/// ticket pinning Pleroma's `"Pack <name> does not exist"` wording to the
/// 400 status (the pack-write routes answer the same message as a 404,
/// [`pack_missing`]). Like [`write_miss`] it folds an unusable name in,
/// so a file write cannot probe the filesystem either.
fn file_pack_missing(name: &str) -> AppError {
    AppError::BadRequest(format!("Pack {name} does not exist"))
}

/// Folds a service refusal that means "there is no pack here to write"
/// into [`file_pack_missing`]; every other failure arrives unchanged.
fn file_write_miss(name: &str, err: AppError) -> AppError {
    if matches!(err, AppError::NotFound | AppError::BadRequest(_)) {
        return file_pack_missing(name);
    }
    err
}

/// Loads pack `name` for a file route, refusing with
/// [`file_pack_missing`]'s 400 what is not there to write.
async fn load_for_file_write(state: &AppState, name: &str) -> AppResult<emoji_packs::Pack> {
    emoji_packs::load_pack(&state.config.emoji_dir, name)
        .await
        .map_err(|err| file_write_miss(name, err))
}

/// The 409 body when the pack already names `code` (the ticket's pinned
/// message, Pleroma's `:already_exists` arm of the create).
fn shortcode_taken(code: &str) -> AppError {
    AppError::Conflict(format!(
        "An emoji with the \"{code}\" shortcode already exists"
    ))
}

/// The 409 body when a rename's new shortcode belongs to another entry
/// and `force` was not given (Pleroma's `:already_exists` arm of the
/// update).
fn shortcode_in_use(code: &str) -> AppError {
    AppError::Conflict(format!(
        "New shortcode \"{code}\" is already used. If you want to override emoji use 'force' option"
    ))
}

/// The 400 body when a rename or delete names a shortcode the pack does
/// not hold (Pleroma's `:doesnt_exist` arm).
fn emoji_missing(code: &str) -> AppError {
    AppError::BadRequest(format!("Emoji \"{code}\" does not exist"))
}

/// The 400 for a shortcode the `[A-Za-z0-9_-]` pattern refuses.
fn bad_shortcode(code: &str) -> AppError {
    AppError::BadRequest(format!("invalid emoji shortcode: {code}"))
}

/// The 400 for a file name [`emoji_packs::file_name_valid`] refuses — the
/// service's own wording for the same refusal.
fn bad_file_name(file: &str) -> AppError {
    AppError::BadRequest(format!("invalid emoji file name: {file}"))
}

/// The 413 for a file over [`MAX_EMOJI_FILE_BYTES`], uploaded or fetched.
fn file_too_large() -> AppError {
    AppError::PayloadTooLarge(format!(
        "Emoji file exceeds the limit of {MAX_EMOJI_FILE_BYTES} bytes"
    ))
}

/// Maps a rename that found nothing to rename — no entry, an entry whose
/// stored file name is unusable, or a stored file that is off disk (the
/// self-rename's `NotFound`) — into [`emoji_missing`]'s 400, Pleroma's
/// answer through `get_filename`; every other failure arrives unchanged.
fn rename_miss(shortcode: &str, err: AppError) -> AppError {
    if matches!(err, AppError::NotFound | AppError::BadRequest(_)) {
        return emoji_missing(shortcode);
    }
    err
}

/// The media type of emoji file bytes, read from the bytes themselves —
/// never from the `content-type` the client or the remote server claimed:
/// the [`infer`] match when its type is one of [`ACCEPTED_EMOJI_TYPES`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`UNSUPPORTED_FILE_TYPE`] for bytes no
/// matcher names and for media an emoji file may not be.
fn detect_emoji_type(bytes: &[u8]) -> AppResult<&'static str> {
    let Some(media_type) = infer::get(bytes)
        .map(|kind| kind.mime_type())
        .filter(|media_type| ACCEPTED_EMOJI_TYPES.contains(media_type))
    else {
        return Err(AppError::Unprocessable(UNSUPPORTED_FILE_TYPE.to_owned()));
    };
    Ok(media_type)
}

/// The extension the detected type gets in the default
/// `<shortcode>.<ext>` file name — the canonical spelling
/// [`crate::uploads`] pairs with each accepted type.
fn emoji_extension(content_type: &str) -> &'static str {
    match content_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        // `image/png`, the only accepted type left.
        _ => "png",
    }
}

/// The file name the new entry gets: `requested` when the body carried a
/// non-empty one, else `<shortcode>.<ext>` from the detected image type;
/// either way it must pass [`emoji_packs::file_name_valid`], so no client
/// name can traverse out of the pack directory.
fn target_filename(
    requested: Option<String>,
    shortcode: &str,
    content_type: &str,
) -> AppResult<String> {
    let filename = match requested {
        Some(name) if !name.is_empty() => name,
        _ => format!("{shortcode}.{}", emoji_extension(content_type)),
    };
    if emoji_packs::file_name_valid(&filename) {
        Ok(filename)
    } else {
        Err(bad_file_name(&filename))
    }
}

/// The 422 for a fetch that never delivered a body to read: the client
/// named a URL this instance will not (SSRF guard) or cannot reach.
fn fetch_refused(err: &HttpError) -> AppError {
    AppError::Unprocessable(format!("Could not fetch the file URL: {err}"))
}

/// Fetches the `file` URL with the state's [`HttpClient`] — the
/// SSRF-guarded client that refuses loopback, private and reserved
/// targets unless `HTTP_ALLOW_PRIVATE_HOSTS` — with the body read capped
/// at [`MAX_EMOJI_FILE_BYTES`], and insists on a 2xx answer whose bytes
/// name an accepted image type.
///
/// # Errors
///
/// [`AppError::PayloadTooLarge`] when the response passes the cap,
/// [`AppError::Unprocessable`] for a refused or failed fetch, a status
/// outside 2xx, and bytes no accepted image type matches.
async fn fetch_emoji_file(state: &AppState, url: &str) -> AppResult<(Bytes, &'static str)> {
    let response = state
        .http_client
        .get_limited(url, &[], MAX_EMOJI_FILE_BYTES)
        .await
        .map_err(|err| fetch_refused(&err))?;
    if !(MIN_FETCH_STATUS..MAX_FETCH_STATUS).contains(&response.status) {
        return Err(AppError::Unprocessable(format!(
            "File URL answered {}",
            response.status
        )));
    }
    let content_type = detect_emoji_type(&response.body)?;
    Ok((response.body, content_type))
}

/// Where the new emoji's bytes come from, as [`file_source`] reads the
/// request: an uploaded part, or the URL the `file` parameter names.
#[derive(Debug)]
enum EmojiFile {
    /// The uploaded `file` part's bytes, already within the size cap.
    Upload(Bytes),
    /// The URL to fetch with the guarded client.
    Url(String),
}

/// Chooses the new emoji's file: the uploaded `file` part wins over the
/// text spelling of the same parameter — it is what a browser's
/// `FormData` sends — and its bytes must fit [`MAX_EMOJI_FILE_BYTES`].
/// A non-blank `file` string is read as a URL to fetch; nothing usable
/// at all is [`MISSING_FILE`]'s 422, and a parameter that is neither a
/// string nor absent names no file this endpoint can store.
///
/// # Errors
///
/// [`AppError::PayloadTooLarge`] for an oversize part,
/// [`AppError::Unprocessable`] for the missing and misplaced parameters.
fn file_source(files: &mut Vec<UploadedFile>, param: Option<&Value>) -> AppResult<EmojiFile> {
    if let Some(upload) = files.take(FILE_FIELD) {
        if upload.bytes.len() > MAX_EMOJI_FILE_BYTES {
            return Err(file_too_large());
        }
        return Ok(EmojiFile::Upload(upload.bytes));
    }
    match param {
        Some(Value::String(url)) if !url.trim().is_empty() => {
            Ok(EmojiFile::Url(url.trim().to_owned()))
        }
        Some(Value::String(_) | Value::Null) | None => {
            Err(AppError::Unprocessable(MISSING_FILE.to_owned()))
        }
        Some(_) => Err(AppError::Unprocessable(UNSUPPORTED_FILE_TYPE.to_owned())),
    }
}

/// The bytes to store and their detected image type: the uploaded
/// `file` part or the fetched URL, per [`file_source`]. Both paths cap
/// the payload at [`MAX_EMOJI_FILE_BYTES`] and demand image magic bytes.
///
/// # Errors
///
/// [`file_source`]'s refusals, [`fetch_emoji_file`]'s, and
/// [`AppError::Unprocessable`] with [`UNSUPPORTED_FILE_TYPE`] for bytes
/// no accepted image type matches.
async fn resolve_emoji_file(
    state: &AppState,
    files: &mut Vec<UploadedFile>,
    param: Option<&Value>,
) -> AppResult<(Bytes, &'static str)> {
    match file_source(files, param)? {
        EmojiFile::Upload(bytes) => {
            let content_type = detect_emoji_type(&bytes)?;
            Ok((bytes, content_type))
        }
        EmojiFile::Url(url) => fetch_emoji_file(state, &url).await,
    }
}

/// `POST /api/v1/pleroma/emoji/packs/files?name=…` — Pleroma's
/// `create` of the file controller: add one emoji file to the pack. The
/// `file` is an uploaded image part or a URL the guarded client fetches
/// (at most 2 MiB, png/gif/jpeg/webp by magic bytes); the `shortcode`
/// must fit `[A-Za-z0-9_-]+` and be new in the pack, else 409; the
/// `filename` defaults to `<shortcode>.<ext>`. The bytes land in the pack
/// directory, `files[shortcode] = filename` lands in `pack.json`, the
/// custom emojis resync, and the pack's full `files` object answers. An
/// unknown or unusable pack is the 400
/// `{"error":"Pack <name> does not exist"}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] for an absent or
/// blank `name`, a refused shortcode pattern or file name, and an unknown
/// pack; [`AppError::Unprocessable`] for a body without `shortcode`, one
/// without any `file`, and non-image bytes;
/// [`AppError::PayloadTooLarge`] for a file over 2 MiB;
/// [`AppError::Conflict`] for a duplicate shortcode;
/// [`AppError::Internal`] for a filesystem failure; [`AppError::Database`]
/// from the resync.
async fn create_file(
    State(state): State<AppState>,
    admin: AdminUser,
    params: PackParams,
    form: MultipartForm<CreateFileParams>,
) -> AppResult<Json<BTreeMap<String, String>>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let MultipartForm { fields, mut files } = form;
    let name = trimmed_name(fields.name.as_deref().or(params.name.as_deref()))?;
    let shortcode = fields.shortcode;
    let mut pack = load_for_file_write(&state, name).await?;
    if !shortcode_valid(&shortcode) {
        return Err(bad_shortcode(&shortcode));
    }
    if pack.files.contains_key(&shortcode) {
        return Err(shortcode_taken(&shortcode));
    }
    let (bytes, content_type) =
        resolve_emoji_file(&state, &mut files, fields.file.as_ref()).await?;
    let filename = target_filename(fields.filename, &shortcode, content_type)?;
    emoji_packs::write_pack_file(&state.config.emoji_dir, name, &filename, &bytes).await?;
    let _previous: Option<String> = pack.files.insert(shortcode, filename);
    emoji_packs::save_pack(&state.config.emoji_dir, name, &pack).await?;
    resync(&state).await?;
    debug!(
        pack = name,
        files = pack.files.len(),
        "emoji pack file added"
    );
    Ok(Json(pack.files))
}

/// Applies one [`Rename`] to `pack`: the entry's stored file name is the
/// starting point, the rename moves the file on disk (Pleroma's
/// `rename_file`, a self-rename for an unchanged name — which is also how
/// an entry whose file vanished off disk is refused), and the manifest
/// loses the old shortcode and gains the new one before `pack.json` is
/// saved.
///
/// # Errors
///
/// [`emoji_missing`]'s 400 for an entry that is not there to rename,
/// [`AppError::BadRequest`] for a new shortcode the pattern refuses and a
/// new file name [`emoji_packs::file_name_valid`] refuses,
/// [`shortcode_in_use`]'s 409 for an occupied new shortcode without
/// `force`; filesystem failures arrive as [`AppError::Internal`].
async fn apply_rename(
    state: &AppState,
    name: &str,
    pack: &mut emoji_packs::Pack,
    rename: &Rename<'_>,
) -> AppResult<()> {
    let old_filename = pack
        .files
        .get(rename.shortcode)
        .cloned()
        .ok_or_else(|| emoji_missing(rename.shortcode))?;
    let new_shortcode = rename.new_shortcode.unwrap_or(rename.shortcode);
    let same_shortcode = new_shortcode == rename.shortcode;
    if !same_shortcode {
        if !shortcode_valid(new_shortcode) {
            return Err(bad_shortcode(new_shortcode));
        }
        if !rename.force && pack.files.contains_key(new_shortcode) {
            return Err(shortcode_in_use(new_shortcode));
        }
    }
    let new_filename = rename.new_filename.unwrap_or(old_filename.as_str());
    if !emoji_packs::file_name_valid(new_filename) {
        return Err(bad_file_name(new_filename));
    }
    emoji_packs::rename_pack_file(&state.config.emoji_dir, name, &old_filename, new_filename)
        .await
        .map_err(|err| rename_miss(rename.shortcode, err))?;
    let _gone: Option<String> = pack.files.remove(rename.shortcode);
    let _taken: Option<String> = pack
        .files
        .insert(new_shortcode.to_owned(), new_filename.to_owned());
    emoji_packs::save_pack(&state.config.emoji_dir, name, pack).await
}

/// `PATCH /api/v1/pleroma/emoji/packs/files?name=…` — Pleroma's `update`
/// of the file controller: rename an entry's shortcode and/or its file.
/// A new shortcode already used by another entry is
/// [`shortcode_in_use`]'s 409 unless `force` overwrites that entry; an
/// unknown shortcode (or one whose stored file vanished off disk) is the
/// 400 `{"error":"Emoji \"<shortcode>\" does not exist"}`, as is an
/// empty `new_shortcode` or `new_filename` the 422. On success the
/// emojis resync and the pack's full `files` object answers.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] for an absent or
/// blank `name` and an unknown pack; [`AppError::Unprocessable`] for a
/// body without `shortcode` or with empty rename values;
/// [`AppError::Conflict`] for an occupied new shortcode;
/// [`AppError::Internal`] for a filesystem failure; [`AppError::Database`]
/// from the resync.
async fn update_file(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiParams(params): ApiParams<UpdateFileParams>,
) -> AppResult<Json<BTreeMap<String, String>>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let name = trimmed_name(params.name.as_deref())?;
    if params.new_shortcode.as_deref().is_some_and(str::is_empty)
        || params.new_filename.as_deref().is_some_and(str::is_empty)
    {
        return Err(AppError::Unprocessable(RENAME_EMPTY.to_owned()));
    }
    let mut pack = load_for_file_write(&state, name).await?;
    let rename = Rename {
        shortcode: &params.shortcode,
        new_shortcode: params.new_shortcode.as_deref(),
        new_filename: params.new_filename.as_deref(),
        force: params.force.unwrap_or_default(),
    };
    apply_rename(&state, name, &mut pack, &rename).await?;
    resync(&state).await?;
    debug!(
        pack = name,
        from = rename.shortcode,
        "emoji pack file renamed"
    );
    Ok(Json(pack.files))
}

/// `DELETE /api/v1/pleroma/emoji/packs/files?name=…` — Pleroma's
/// `delete` of the file controller: remove one entry and the file it
/// names. An unknown shortcode is the 400
/// `{"error":"Emoji \"<shortcode>\" does not exist"}`; an entry whose
/// stored file name is unusable or off disk loses the entry anyway (the
/// manifest is what must stop naming it, and an escaping stored name is
/// never touched). On success the emojis resync and the pack's full
/// `files` object answers.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] for an absent or
/// blank `name`, an unknown pack, and an unknown shortcode;
/// [`AppError::Internal`] for a filesystem failure; [`AppError::Database`]
/// from the resync.
async fn delete_file(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiParams(params): ApiParams<DeleteFileParams>,
) -> AppResult<Json<BTreeMap<String, String>>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let name = trimmed_name(params.name.as_deref())?;
    let mut pack = load_for_file_write(&state, name).await?;
    let Some(filename) = pack.files.remove(&params.shortcode) else {
        return Err(emoji_missing(&params.shortcode));
    };
    if emoji_packs::file_name_valid(&filename) {
        emoji_packs::remove_pack_file(&state.config.emoji_dir, name, &filename).await?;
    }
    emoji_packs::save_pack(&state.config.emoji_dir, name, &pack).await?;
    resync(&state).await?;
    debug!(
        pack = name,
        shortcode = params.shortcode,
        "emoji pack file deleted"
    );
    Ok(Json(pack.files))
}

/// `GET /emoji/{pack}/{file}` — one image file of a pack, served with the
/// media safety headers. No authentication: the address is what
/// `custom_emojis.image_url` carries into client HTML.
///
/// # Errors
///
/// [`AppError::NotFound`] for an unusable name, a non-image file type or an
/// absent file — one answer for every miss, so the route cannot probe the
/// filesystem; other read failures arrive as [`AppError::Internal`].
async fn serve_emoji_file(
    State(state): State<AppState>,
    Path((pack, file)): Path<(String, String)>,
) -> AppResult<Response> {
    let content_type = emoji_content_type(&file)?;
    let read = emoji_packs::read_pack_file(&state.config.emoji_dir, &pack, &file).await;
    let bytes = match read {
        Ok(bytes) => bytes,
        Err(AppError::BadRequest(_)) => return Err(AppError::NotFound),
        Err(err) => return Err(err),
    };
    Ok(emoji_response(content_type, bytes))
}

/// The image content type to serve `file` as, from its extension alone
/// ([`uploads::content_type_for_key`]): a name the allowlist does not map
/// to `image/*` — HTML, SVG, the manifests, byte soup — answers the same
/// 404 as an absent file, so nothing stored in a pack can render in this
/// instance's origin.
fn emoji_content_type(file: &str) -> AppResult<&'static str> {
    let content_type = uploads::content_type_for_key(file);
    content_type
        .starts_with("image/")
        .then_some(content_type)
        .ok_or(AppError::NotFound)
}

/// The stored bytes of a pack image with the safety headers of the [module
/// docs].
///
/// [module docs]: self
fn emoji_response(content_type: &'static str, bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static(CACHE_CONTROL),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static(NOSNIFF),
            ),
            (CONTENT_DISPOSITION, HeaderValue::from_static(INLINE)),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::{Request, Uri};
    use serde_json::json;

    use super::*;

    /// Request parts carrying `uri`, the way the extractor receives them.
    fn parts_for(uri: &str) -> Parts {
        Request::get(uri.parse::<Uri>().unwrap())
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    /// The parameters the request line `uri` extracts.
    async fn params_for(uri: &str) -> PackParams {
        let mut parts = parts_for(uri);
        PackParams::from_request_parts(&mut parts, &())
            .await
            .unwrap()
    }

    /// A pack fixture with two files and full metadata.
    fn fixture_pack() -> emoji_packs::Pack {
        emoji_packs::Pack {
            pack: emoji_packs::PackMeta {
                license: Some("CC0-1.0".to_owned()),
                description: None,
                share_files: true,
                ..emoji_packs::PackMeta::default()
            },
            files: BTreeMap::from([
                ("cat".to_owned(), "cat.png".to_owned()),
                ("dog".to_owned(), "dog.png".to_owned()),
            ]),
        }
    }

    #[tokio::test]
    async fn page_params_default_and_clamp_like_pleromas_paginate() {
        let plain = params_for("/api/v1/pleroma/emoji/packs").await;
        assert_eq!(
            (plain.page, plain.page_size),
            (DEFAULT_PAGE, DEFAULT_PAGE_SIZE),
            "absent parameters take the defaults"
        );
        assert_eq!(plain.name, None, "no name by default");

        let big = params_for("/p?page=0&page_size=500&name=x").await;
        assert_eq!(big.page, 1, "a page below 1 reads as the first");
        assert_eq!(
            (big.page_size, big.name.as_deref()),
            (100, Some("x")),
            "page_size caps at MAX_PAGE_SIZE, name is kept raw"
        );

        for query in ["/p?page=abc&page_size=1.5", "/p?page=-2&page_size=0"] {
            let junk = params_for(query).await;
            assert_eq!(
                (junk.page, junk.page_size),
                (DEFAULT_PAGE, DEFAULT_PAGE_SIZE),
                "{query}: numbers that are not whole fall back to the defaults"
            );
        }

        let repeated = params_for("/p?page=2&page=3&page_size=5&page_size=7").await;
        assert_eq!(
            (repeated.page, repeated.page_size),
            (3, 7),
            "the last occurrence wins"
        );
        let encoded = params_for("/p?name=a%20b%2Fc").await;
        assert_eq!(
            encoded.name.as_deref(),
            Some("a b/c"),
            "percent-encoding is decoded before validation rejects it"
        );
    }

    #[tokio::test]
    async fn a_missing_or_blank_name_is_pleromas_empty_value_error() {
        for uri in ["/p", "/p?name=", "/p?name=%20%20"] {
            let params = params_for(uri).await;
            let err = params.required_name().unwrap_err();
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "{uri} is a 400, not {err}"
            );
            assert_eq!(err.to_string(), NAME_EMPTY, "message for {uri}");
        }
        let padded = params_for("/p?name=%20cats%20").await;
        assert_eq!(
            padded.required_name().unwrap(),
            "cats",
            "the name is trimmed like Pleroma's String.trim"
        );
    }

    #[test]
    fn take_pages_walks_ordered_items() {
        let items: Vec<usize> = (1..=7).collect();
        assert_eq!(
            take_page(items.iter().copied(), 1, 3),
            [1, 2, 3],
            "first page"
        );
        assert_eq!(
            take_page(items.iter().copied(), 3, 3),
            [7],
            "ragged last page"
        );
        assert!(
            take_page(items.iter().copied(), 4, 3).is_empty(),
            "past the end is an empty page"
        );
        assert_eq!(
            take_page(items.iter().copied(), usize::MAX, 30),
            Vec::<usize>::new(),
            "an absurd page cannot overflow"
        );
    }

    #[test]
    fn the_pack_view_serializes_pleromas_three_keys() {
        let value = serde_json::to_value(PackView::full(fixture_pack())).unwrap();
        assert_eq!(
            value,
            json!({
                "pack": {"license": "CC0-1.0", "share-files": true},
                "files": {"cat": "cat.png", "dog": "dog.png"},
                "files_count": 2,
            }),
            "metadata block, full shortcode map and count"
        );

        let paged = serde_json::to_value(PackView::paginated(fixture_pack(), 2, 1)).unwrap();
        assert_eq!(
            paged,
            json!({
                "pack": {"license": "CC0-1.0", "share-files": true},
                "files": {"dog": "dog.png"},
                "files_count": 2,
            }),
            "the second page carries one shortcode but still the full count"
        );
    }

    #[test]
    fn the_packs_response_pages_by_name_and_counts_everything() {
        let all = vec![
            ("a".to_owned(), fixture_pack()),
            ("b".to_owned(), fixture_pack()),
            ("c".to_owned(), fixture_pack()),
        ];
        let value = serde_json::to_value(PacksResponse::new(all, 2, 2)).unwrap();
        assert_eq!(value["count"], json!(3), "count is every pack on disk");
        assert_eq!(
            value["packs"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["c"],
            "the second page of two holds only the third name"
        );
    }

    #[test]
    fn the_zip_response_carries_the_download_headers() {
        let response = zip_response("cats", vec![1, 2, 3]).unwrap().into_response();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            ZIP_CONTENT_TYPE,
            "the archive is a zip"
        );
        assert_eq!(
            response.headers()[CONTENT_DISPOSITION],
            r#"attachment; filename="cats.zip""#,
            "the download filename follows the pack name"
        );
    }

    #[test]
    fn only_image_extensions_are_served_and_everything_else_404s() {
        for (file, want) in [
            ("cat.png", "image/png"),
            ("cat.gif", "image/gif"),
            ("sub/cat.webp", "image/webp"),
        ] {
            assert_eq!(
                emoji_content_type(file).unwrap(),
                want,
                "{file} is an image"
            );
        }
        for file in [
            "pack.json",
            "page.html",
            "icon.svg",
            "notes.txt",
            "noext",
            "script.js",
        ] {
            let err = emoji_content_type(file).unwrap_err();
            assert!(
                matches!(err, AppError::NotFound),
                "{file} answers the absent-file 404, not {err}"
            );
        }
    }

    #[test]
    fn served_images_carry_the_media_safety_headers() {
        let response = emoji_response("image/png", vec![9]).into_response();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "image/png",
            "the type comes from the extension"
        );
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            CACHE_CONTROL,
            "pack images cache for a day"
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            NOSNIFF,
            "the type may not be guessed over"
        );
        assert_eq!(
            response.headers()[CONTENT_DISPOSITION],
            INLINE,
            "images render"
        );
    }

    #[test]
    fn the_metadata_patch_reads_pleromas_dashed_names() {
        let patch: MetadataPatch = serde_json::from_value(json!({
            "license": "MIT",
            "homepage": "https://emoji.example",
            "description": "Cats",
            "fallback-src": "https://emoji.example/cats.zip",
            "fallback-src-sha256": "beef",
            "share-files": true,
        }))
        .unwrap();
        assert_eq!(patch.license.as_deref(), Some("MIT"));
        assert_eq!(patch.homepage.as_deref(), Some("https://emoji.example"));
        assert_eq!(patch.description.as_deref(), Some("Cats"));
        assert_eq!(
            patch.fallback_src.as_deref(),
            Some("https://emoji.example/cats.zip"),
            "the dashed key renamed"
        );
        assert_eq!(patch.fallback_src_sha256.as_deref(), Some("beef"));
        assert_eq!(patch.share_files, Some(true), "a real boolean");

        let form: MetadataPatch = serde_json::from_value(json!({"share-files": "off"})).unwrap();
        assert_eq!(
            form.share_files,
            Some(false),
            "the form spelling reads through flex"
        );

        let empty: MetadataPatch = serde_json::from_value(json!({})).unwrap();
        assert!(
            empty.license.is_none()
                && empty.homepage.is_none()
                && empty.description.is_none()
                && empty.fallback_src.is_none()
                && empty.fallback_src_sha256.is_none()
                && empty.share_files.is_none(),
            "an empty patch carries no key to merge"
        );
    }

    #[test]
    fn metadata_merge_replaces_present_keys_and_keeps_the_rest() {
        let mut stored = emoji_packs::PackMeta {
            license: Some("CC0-1.0".to_owned()),
            homepage: Some("https://old.example".to_owned()),
            description: Some("keep me".to_owned()),
            fallback_src: Some("https://old.example/old.zip".to_owned()),
            fallback_src_sha256: Some("dead".to_owned()),
            share_files: true,
        };
        let patch: MetadataPatch = serde_json::from_value(json!({
            "license": "MIT",
            "fallback-src": "https://new.example/new.zip",
            "share-files": false,
        }))
        .unwrap();
        merge_metadata(&mut stored, patch);
        assert_eq!(
            stored.license.as_deref(),
            Some("MIT"),
            "present key replaced"
        );
        assert_eq!(
            stored.fallback_src.as_deref(),
            Some("https://new.example/new.zip"),
            "the new fallback stored as given"
        );
        assert_eq!(
            stored.fallback_src_sha256.as_deref(),
            Some("dead"),
            "the sha the patch left out stands"
        );
        assert!(!stored.share_files, "present boolean replaced");
        assert_eq!(
            (stored.homepage.as_deref(), stored.description.as_deref()),
            (Some("https://old.example"), Some("keep me")),
            "untouched keys keep their values"
        );
    }

    #[test]
    fn a_fallback_src_without_its_sha_still_merges_alone() {
        let mut stored = emoji_packs::PackMeta::default();
        let patch: MetadataPatch =
            serde_json::from_value(json!({"fallback-src": "https://x.test/p.zip"})).unwrap();
        merge_metadata(&mut stored, patch);
        assert_eq!(
            stored.fallback_src.as_deref(),
            Some("https://x.test/p.zip"),
            "the url is stored without any fetch"
        );
        assert_eq!(
            stored.fallback_src_sha256, None,
            "and no sha appears on its own"
        );
    }

    #[test]
    fn write_names_are_trimmed_and_required_like_the_reads() {
        assert_eq!(
            trimmed_name(Some("  cofe  ")).unwrap(),
            "cofe",
            "the name is trimmed"
        );
        for raw in [None, Some(""), Some("   ")] {
            let err = trimmed_name(raw).unwrap_err();
            assert_eq!(err.to_string(), NAME_EMPTY, "absent or blank is a 400");
        }
    }

    #[test]
    fn the_write_404_names_the_missing_pack_and_absorbs_the_service_400s() {
        assert_eq!(
            pack_missing("ghost").to_string(),
            "Pack ghost does not exist",
            "Pleroma's wording, name included"
        );
        let err = write_miss("ghost", AppError::NotFound);
        assert!(
            matches!(err, AppError::NotFoundMessage(_)),
            "an absent pack is the named 404"
        );
        let bad_name = write_miss("../x", AppError::BadRequest("invalid pack name".to_owned()));
        assert_eq!(
            bad_name.to_string(),
            "Pack ../x does not exist",
            "an unusable name folds into the same 404"
        );
        let internal = write_miss("p", AppError::Internal("disk full".to_owned()));
        assert!(
            matches!(internal, AppError::Internal(_)),
            "a real server failure is not a 404"
        );
    }

    #[test]
    fn the_create_body_also_carries_the_name() {
        let params: CreateParams = serde_json::from_value(json!({"name": "from-body"})).unwrap();
        assert_eq!(params.name.as_deref(), Some("from-body"));
        let none: CreateParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(
            none.name, None,
            "no name in an empty body: the query has it"
        );
    }

    #[test]
    fn the_update_body_needs_a_metadata_object() {
        let with: UpdateParams = serde_json::from_value(json!({
            "name": "cats",
            "metadata": {"share-files": true},
        }))
        .unwrap();
        assert_eq!(with.name.as_deref(), Some("cats"));
        assert_eq!(with.metadata.share_files, Some(true));
        let err = serde_json::from_value::<UpdateParams>(json!({"name": "cats"}))
            .expect_err("metadata is required");
        assert!(
            err.to_string().contains("metadata"),
            "the error names it: {err}"
        );

        let not_a_bool = serde_json::from_value::<MetadataPatch>(json!({"share-files": "maybe"}))
            .expect_err("share-files must be a boolean");
        assert!(
            not_a_bool.to_string().contains("boolean"),
            "the refusal says why: {not_a_bool}"
        );
    }

    #[test]
    fn shortcodes_follow_the_pinned_pattern() {
        for code in ["cat", "a", "0", "dog_1", "well-known", "MiXeD42"] {
            assert!(shortcode_valid(code), "{code} is a shortcode");
        }
        for code in [
            "", " ", "cat dog", "cat.png", "a/b", ".hidden", "..", "cat!", "cat;rm", "é", "cat\n",
        ] {
            assert!(!shortcode_valid(code), "{code} is not a shortcode");
        }
    }

    #[test]
    fn only_the_four_magic_named_images_are_emoji_files() {
        for (bytes, want) in [
            (b"\x89PNG\r\n\x1a\nrest".as_slice(), "image/png"),
            (b"GIF89a....".as_slice(), "image/gif"),
            (b"\xFF\xD8\xFF\xE0rest".as_slice(), "image/jpeg"),
            (b"RIFF\x00\x00\x00\x00WEBPVP8 ".as_slice(), "image/webp"),
        ] {
            assert_eq!(detect_emoji_type(bytes).unwrap(), want, "type of {want}");
        }
        for bytes in [
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".as_slice(),
            b"PK\x03\x04zipzip".as_slice(),
            b"just text".as_slice(),
            b"".as_slice(),
        ] {
            let err = detect_emoji_type(bytes).unwrap_err();
            assert!(
                matches!(err, AppError::Unprocessable(_)),
                "no accepted image hides in these bytes: {err}"
            );
            assert_eq!(err.to_string(), UNSUPPORTED_FILE_TYPE, "pinned wording");
        }
    }

    #[test]
    fn the_default_filename_follows_the_detected_type() {
        for (content_type, extension) in [
            ("image/png", "png"),
            ("image/gif", "gif"),
            ("image/jpeg", "jpg"),
            ("image/webp", "webp"),
        ] {
            assert_eq!(
                target_filename(None, "cat", content_type).unwrap(),
                format!("cat.{extension}"),
                "<shortcode>.<ext> for {content_type}"
            );
            assert_eq!(
                target_filename(Some(String::new()), "cat", content_type).unwrap(),
                format!("cat.{extension}"),
                "an empty request falls back to the default"
            );
        }
        assert_eq!(
            target_filename(Some("sub/cat-picture".to_owned()), "cat", "image/png").unwrap(),
            "sub/cat-picture".to_owned(),
            "the request's own plain file name stands"
        );
        for bad in [
            "../escape.png",
            "/abs.png",
            ".hidden",
            "a/../../b",
            "two words.png",
        ] {
            let err = target_filename(Some(bad.to_owned()), "cat", "image/png").unwrap_err();
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "{bad} is no file name: {err}"
            );
            assert_eq!(err.to_string(), format!("invalid emoji file name: {bad}"));
        }
    }

    #[test]
    fn the_file_error_wordings_are_the_pinned_ones() {
        assert_eq!(
            shortcode_taken("cat").to_string(),
            "An emoji with the \"cat\" shortcode already exists",
            "the create's 409, quoted shortcode"
        );
        assert_eq!(
            shortcode_in_use("dog").to_string(),
            "New shortcode \"dog\" is already used. If you want to override emoji use 'force' option",
            "the rename's 409, Pleroma's wording"
        );
        assert_eq!(
            emoji_missing("ghost").to_string(),
            "Emoji \"ghost\" does not exist",
            "the rename and delete 400"
        );
        assert_eq!(
            file_pack_missing("nope").to_string(),
            "Pack nope does not exist",
            "the file routes' pack 400"
        );
        assert_eq!(
            file_too_large().to_string(),
            format!("Emoji file exceeds the limit of {MAX_EMOJI_FILE_BYTES} bytes"),
            "the size refusal names the cap"
        );
        assert_eq!(
            fetch_refused(&HttpError::InvalidUrl).to_string(),
            format!("Could not fetch the file URL: {}", HttpError::InvalidUrl),
            "the fetch refusal keeps the cause"
        );
    }

    #[test]
    fn an_unusable_pack_folds_into_the_file_routes_400() {
        let absent = file_write_miss("nope", AppError::NotFound);
        assert!(
            matches!(absent, AppError::BadRequest(_)),
            "an absent pack is the named 400, not {absent}"
        );
        assert_eq!(
            absent.to_string(),
            "Pack nope does not exist",
            "Pleroma's wording, name included"
        );
        let escaping =
            file_write_miss("../x", AppError::BadRequest("invalid pack name".to_owned()));
        assert_eq!(
            escaping.to_string(),
            "Pack ../x does not exist",
            "an unusable name folds into the same 400"
        );
        let broken = file_write_miss("p", AppError::Internal("disk full".to_owned()));
        assert!(
            matches!(broken, AppError::Internal(_)),
            "a real server failure is not a 400"
        );
        let gone = rename_miss("cat", AppError::NotFound);
        assert_eq!(
            gone.to_string(),
            "Emoji \"cat\" does not exist",
            "a file off disk is the emoji's own 400"
        );
    }

    #[test]
    fn the_file_bodies_read_their_shapes() {
        let create: CreateFileParams = serde_json::from_value(json!({
            "name": "cats",
            "shortcode": "cat",
            "filename": "cat.png",
            "file": "https://x.test/cat.png",
        }))
        .unwrap();
        assert_eq!(create.name.as_deref(), Some("cats"));
        assert_eq!(create.shortcode, "cat");
        assert_eq!(create.filename.as_deref(), Some("cat.png"));
        assert_eq!(
            create.file,
            Some(Value::String("https://x.test/cat.png".to_owned())),
            "the URL rides in as the string it is"
        );

        let minimal: CreateFileParams =
            serde_json::from_value(json!({"shortcode": "cat", "file": "https://x.test/c"}))
                .unwrap();
        assert_eq!(minimal.name, None, "the query has the pack name");
        assert_eq!(minimal.filename, None, "and the default names the file");

        let no_file: Result<CreateFileParams, _> =
            serde_json::from_value(json!({"shortcode": "cat"}));
        assert!(no_file.is_ok(), "file is only required on the wire here");

        let rename: UpdateFileParams = serde_json::from_value(json!({
            "name": "cats",
            "shortcode": "cat",
            "new_shortcode": "kitty",
            "force": "on",
        }))
        .unwrap();
        assert_eq!(rename.new_shortcode.as_deref(), Some("kitty"));
        assert_eq!(
            rename.new_filename, None,
            "a rename may touch only the shortcode"
        );
        assert_eq!(
            rename.force,
            Some(true),
            "the form spelling reads through flex"
        );

        let drop: DeleteFileParams = serde_json::from_value(json!({"shortcode": "cat"})).unwrap();
        assert_eq!(drop.shortcode, "cat");
    }

    #[test]
    fn an_empty_rename_value_is_pinned_to_the_422() {
        for params in [
            json!({"shortcode": "a", "new_shortcode": ""}),
            json!({"shortcode": "a", "new_filename": ""}),
        ] {
            let update: UpdateFileParams = serde_json::from_value(params).unwrap();
            let empty = update.new_shortcode.as_deref().is_some_and(str::is_empty)
                || update.new_filename.as_deref().is_some_and(str::is_empty);
            assert!(empty, "an explicit empty is not an absent rename");
        }
        let absent: UpdateFileParams = serde_json::from_value(json!({"shortcode": "a"})).unwrap();
        assert!(
            absent.new_shortcode.is_none() && absent.new_filename.is_none(),
            "absent keys keep the entry as it stands"
        );
    }

    #[test]
    fn the_file_source_prefers_the_part_and_names_a_missing_one() {
        let upload = UploadedFile {
            field: FILE_FIELD.to_owned(),
            filename: Some("whatever.png".to_owned()),
            content_type: Some("image/png".to_owned()),
            bytes: Bytes::from_static(b"\x89PNG\r\n\x1a\nrest"),
        };
        let mut files = vec![upload];
        let source = file_source(&mut files, Some(&json!("https://x.test/other")))
            .unwrap_or_else(|err| panic!("the part is the file: {err}"));
        assert!(
            matches!(source, EmojiFile::Upload(bytes) if bytes.len() == 12),
            "the part's bytes are the payload, its name is ignored"
        );
        assert!(files.is_empty(), "the part was taken, not peeked");

        let mut empty: Vec<UploadedFile> = Vec::new();
        let requested = file_source(&mut empty, Some(&json!(" https://x.test/cat.png ")))
            .unwrap_or_else(|err| panic!("a string is a URL to fetch: {err}"));
        assert_eq!(
            source_string(&requested).as_deref(),
            Some("https://x.test/cat.png"),
            "the URL is trimmed and read by the fetch"
        );
        for param in [None, Some(&Value::String("  ".to_owned()))] {
            let err = file_source(&mut empty, param).unwrap_err();
            assert_eq!(
                err.to_string(),
                MISSING_FILE,
                "no usable file parameter is the missing-file 422"
            );
        }
        let err = file_source(&mut empty, Some(&json!({"path": "x"}))).unwrap_err();
        assert_eq!(
            err.to_string(),
            UNSUPPORTED_FILE_TYPE,
            "an object is no file value"
        );
    }

    /// The URL of an [`EmojiFile::Url`] source, `None` for an upload.
    fn source_string(source: &EmojiFile) -> Option<String> {
        match source {
            EmojiFile::Url(url) => Some(url.clone()),
            EmojiFile::Upload(_) => None,
        }
    }

    #[test]
    fn an_oversize_upload_is_the_413_before_anything_else() {
        let upload = UploadedFile {
            field: FILE_FIELD.to_owned(),
            filename: Some("big.png".to_owned()),
            content_type: Some("image/png".to_owned()),
            bytes: Bytes::from(vec![0_u8; MAX_EMOJI_FILE_BYTES + 1]),
        };
        let mut files = vec![upload];
        let err = file_source(&mut files, None).unwrap_err();
        assert!(
            matches!(err, AppError::PayloadTooLarge(_)),
            "2 MiB and one byte is too large: {err}"
        );
    }
}
