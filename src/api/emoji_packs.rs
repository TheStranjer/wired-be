//! Pleroma's public emoji-pack endpoints, under `/api/v1/pleroma/emoji`
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! These are the read side of `Pleroma.Web.PleromaAPI.EmojiPackController`
//! (`lib/pleroma/web/pleroma_api/controllers/emoji_pack_controller.ex`)
//! plus Pleroma's util emoji map (`UtilController.emoji`,
//! `lib/pleroma/web/pleroma_api/controllers/util_controller.ex`).
//!
//! All four endpoints answer without authentication — the Pleroma
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
//! Unknown or unusable pack names are one and the same 404
//! `{"error":"Pack not found"}`: the endpoints cannot be used to probe the
//! filesystem, and `[A-Za-z0-9_.-]`-validated names keep every path inside
//! `EMOJI_DIR`. The archive's refusals follow the ticket exactly: 403
//! `{"error":"Pack is not shared"}` for a pack that does not share its
//! files, 400 for a missing `name` parameter (Pleroma's `"pack name cannot
//! be empty"`).
//!
//! [`files_router`] is the separate top-level `GET /emoji/{pack}/{file}`:
//! the address [`emoji_image_url`](crate::services::emoji_packs::emoji_image_url)
//! stores in `custom_emojis.image_url`. Like [`crate::api::media_files`] it
//! serves from the instance's own origin, so the type comes from the
//! extension and only image types are offered at all — anything else (the
//! `pack.json` manifests included) answers the same 404 as an absent file.

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
    routing::get,
};
use serde::Serialize;
use url::form_urlencoded;

use crate::{
    error::{AppError, AppResult},
    models::custom_emoji,
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
/// `content-type` of the archive download.
const ZIP_CONTENT_TYPE: &str = "application/zip";
/// `cache-control` for pack images: packs are edited in place (unlike the
/// random upload keys), so images are cacheable for a day, not `immutable`.
const CACHE_CONTROL: &str = "public, max-age=86400";
/// `x-content-type-options`: the type sent here is the type to use.
const NOSNIFF: &str = "nosniff";
/// Pack images render inline; nothing else this route could name is served.
const INLINE: &str = "inline";

/// The `/api/v1/pleroma/emoji` router, nested by [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(emoji_map))
        .route("/pack", get(show))
        .route("/packs", get(index))
        .route("/packs/archive", get(archive))
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
}
