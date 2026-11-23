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
//! The four install routes (WBE-216) are the controller's `remote`,
//! `download`, `download_zip` and `import_from_filesystem` actions.
//! [`AdminUser`] gates them all; the ticket grants the two GETs the
//! `admin:read` class and the two POSTs `admin:write` (Pleroma itself
//! demands `admin:write` for all four).
//!
//! - `GET /packs/remote?url=…&page=…&page_size=…` — relay another
//!   instance's `{"count","packs"}` listing, fetched from
//!   `{url}/api/v1/pleroma/emoji/packs` through the SSRF-guarded
//!   [`HttpClient`](crate::http_client::HttpClient) at a 5 MiB cap.
//!   A `url` no client may call is the 400, any remote failure the 500
//!   `{"error":"Could not fetch the pack list"}`.
//! - `POST /packs/download` — body `url`, `name` and the optional local
//!   name `as`: the remote pack listing decides the source (a shared
//!   pack downloads from its `packs/archive` endpoint, an unshared one
//!   only through a `fallback-src`, else the 400 `{"error":"The pack
//!   was not set as shared and there is no fallback src to download
//!   from"}`); the zip — at most 20 MiB — is extracted safely into a
//!   fresh pack directory, and an occupied local name is the 400
//!   `{"error":"Pack already exists"}`. Then the emojis resync and the
//!   answer is `"ok"`.
//! - `POST /packs/download_zip` — the same install from an uploaded zip
//!   under `file`; with no `pack.json` inside, every image becomes an
//!   emoji named after its file stem.
//! - `GET /packs/import` — write `pack.json` for every pack directory of
//!   `EMOJI_DIR` that lacks a valid one
//!   ([`import_packs`](crate::services::emoji_packs::import_packs)) and
//!   answer the imported names as a JSON array.
//!
//! [`AdminUser`]: crate::auth::admin::AdminUser
//! [`moderation_log`]: crate::models::moderation_log

mod file_checks;
mod file_writes;
mod install;
mod pack_writes;
mod params;
mod reads;
mod remote;
mod serve;
#[cfg(test)]
mod test_support;

use axum::{
    Router,
    routing::{get, post},
};

use self::{
    file_writes::{create_file, delete_file, update_file},
    install::{download, download_zip, import},
    pack_writes::{create, destroy, update},
    reads::{archive, emoji_map, index, show},
    remote::remote,
    serve::serve_emoji_file,
};
use crate::{
    config::Config, extract::multipart::upload_body_limit, services::emoji_packs, state::AppState,
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
/// The image types magic bytes can name for an emoji file: the service's
/// allowlist, shared with the pack-archive extraction of
/// [`emoji_packs::extract_pack_archive`]. SVG is deliberately absent: it
/// has no magic bytes to read, and the static pack route refuses to
/// serve it either, so accepting it here could only store a file that
/// never renders.
const ACCEPTED_EMOJI_TYPES: [&str; 4] = emoji_packs::ACCEPTED_EMOJI_TYPES;
/// 422 body for bytes [`detect_emoji_type`](file_checks::detect_emoji_type)
/// cannot name an accepted image type for (the wording the media and
/// profile uploads use alike).
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

/// The read class of [`AdminUser::require`]: the ticket grants the two
/// GET install routes `admin:read` (and its narrow `admin:read:emoji`
/// alias), where Pleroma demands `admin:write`.
const CLASS_READ: &str = "read";
/// Largest remote listing response the install routes read: the
/// ticket's 5 MiB.
const MAX_REMOTE_LIST_BYTES: usize = 5 * 1024 * 1024;
/// 400 body for a `url` this instance's client could never call.
const INVALID_URL: &str = "url must be a valid http(s) URL";
/// 500 body when the remote pack listing could not be fetched or read —
/// the ticket's pinned wording for the relay route, kept for the
/// download's own listing fetch.
const COULD_NOT_FETCH: &str = "Could not fetch the pack list";
/// 500 body when the pack archive (shared or `fallback-src`) could not
/// be fetched, read or extracted within
/// [`MAX_PACK_ARCHIVE_BYTES`](emoji_packs::MAX_PACK_ARCHIVE_BYTES).
const COULD_NOT_DOWNLOAD: &str = "Could not download the pack archive";
/// The 400 for an archive whose bytes do not hash to the SHA-256 its
/// listing declared (Pleroma's `:invalid_checksum`).
const INVALID_CHECKSUM: &str = "Invalid pack checksum";
/// 400 body for a remote pack that shares no files and names no
/// fallback archive, or declares no SHA-256 for the one it has (Pleroma's
/// `fetch_pack_info` refusal, the ticket's exact wording).
const PACK_NOT_SHAREABLE: &str =
    "The pack was not set as shared and there is no fallback src to download from";
/// The remote path of an instance's pack listing.
const REMOTE_PACKS_PATH: &str = "/api/v1/pleroma/emoji/packs";
/// The remote path of one pack's listing.
const REMOTE_PACK_PATH: &str = "/api/v1/pleroma/emoji/pack";
/// The remote path of one pack's zip archive.
const REMOTE_ARCHIVE_PATH: &str = "/api/v1/pleroma/emoji/packs/archive";

/// The `/api/v1/pleroma/emoji` router, nested by [`crate::api::v1`].
///
/// `config` is what [`upload_body_limit`] sizes the `/packs/files` create
/// and `/packs/download_zip` routes' body limits from: axum's multipart
/// extractor refuses bodies over 2 MiB unless the route raises that
/// first, and both endpoints take uploaded files.
pub fn router(config: &Config) -> Router<AppState> {
    Router::new()
        .route("/", get(emoji_map))
        .route(
            "/pack",
            get(show).post(create).patch(update).delete(destroy),
        )
        .route("/packs", get(index))
        .route("/packs/archive", get(archive))
        .route("/packs/download", post(download))
        .route(
            "/packs/download_zip",
            post(download_zip).layer(upload_body_limit(config)),
        )
        .route("/packs/import", get(import))
        .route("/packs/remote", get(remote))
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
