//! The unauthenticated read endpoints: the util emoji map, the pack
//! listing, one pack and the pack archive download.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::State,
    http::{
        HeaderValue,
        header::{self, CONTENT_DISPOSITION},
    },
    response::{IntoResponse as _, Response},
};
use serde::Serialize;

use super::{
    PACK_NOT_FOUND, ZIP_CONTENT_TYPE,
    params::{PackParams, take_page},
};
use crate::{
    error::{AppError, AppResult},
    models::custom_emoji,
    services::emoji_packs,
    state::AppState,
};

/// One pack as the endpoints serialize it: Pleroma's `@derive
/// {Jason.Encoder, only: [:files, :pack, :files_count]}` — `files` may be
/// the paginated window while `files_count` always counts the whole pack.
#[derive(Debug, Serialize)]
pub(super) struct PackView {
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
pub(super) struct PacksResponse {
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
pub(super) struct EmojiMapEntry {
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
pub(super) async fn emoji_map(
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
pub(super) async fn index(
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
pub(super) async fn show(
    State(state): State<AppState>,
    params: PackParams,
) -> AppResult<Json<PackView>> {
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
pub(super) async fn archive(
    State(state): State<AppState>,
    params: PackParams,
) -> AppResult<Response> {
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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
}
