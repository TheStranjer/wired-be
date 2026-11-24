//! The unauthenticated read endpoints: the util emoji map, the pack
//! listing, one pack and the pack archive download.

use std::{collections::BTreeMap, path::Path};

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
    util::token::sha256_hex_bytes,
};

/// The metadata block a listing serializes: the stored
/// [`PackMeta`](emoji_packs::PackMeta) plus the two keys Pleroma computes
/// when it loads a pack (`validate_pack`), which a remote instance needs to
/// install a shared pack.
#[derive(Debug, Serialize)]
pub(super) struct PackMetaView {
    /// The stored metadata, its `download-sha256` replaced by the digest
    /// of the archive this instance serves right now.
    #[serde(flatten)]
    meta: emoji_packs::PackMeta,
    /// Whether the archive endpoint can serve this pack.
    #[serde(rename = "can-download")]
    can_download: bool,
}

impl PackMetaView {
    /// The block for `meta` with `digest`, the uppercase hex SHA-256 of
    /// the pack's archive, or `None` for a pack that cannot be downloaded
    /// (whatever digest the manifest carried is dropped).
    fn new(mut meta: emoji_packs::PackMeta, digest: Option<String>) -> Self {
        meta.download_sha256 = digest;
        Self {
            can_download: meta.download_sha256.is_some(),
            meta,
        }
    }
}

/// One pack as the endpoints serialize it: Pleroma's `@derive
/// {Jason.Encoder, only: [:files, :pack, :files_count]}` — `files` may be
/// the paginated window while `files_count` always counts the whole pack.
#[derive(Debug, Serialize)]
pub(super) struct PackView {
    /// The manifest metadata block with the download advertisement.
    pack: PackMetaView,
    /// The pack's files (full list on `/packs`, one page on `/pack`).
    files: BTreeMap<String, String>,
    /// Total files in the pack, before pagination.
    files_count: usize,
}

impl PackView {
    /// The view carrying all of `pack`'s files, advertising `digest`.
    fn full(pack: emoji_packs::Pack, digest: Option<String>) -> Self {
        let files_count = pack.files.len();
        Self {
            pack: PackMetaView::new(pack.pack, digest),
            files: pack.files,
            files_count,
        }
    }

    /// The view carrying `pack`'s files paginated by shortcode (the map is
    /// ordered, matching Pleroma's `Enum.sort` of the keys), advertising
    /// `digest`.
    fn paginated(
        pack: emoji_packs::Pack,
        digest: Option<String>,
        page: usize,
        page_size: usize,
    ) -> Self {
        let files_count = pack.files.len();
        Self {
            pack: PackMetaView::new(pack.pack, digest),
            files: take_page(pack.files.into_iter(), page, page_size)
                .into_iter()
                .collect(),
            files_count,
        }
    }
}

/// The uppercase hex SHA-256 of pack `name`'s archive (Pleroma's
/// `Base.encode16`), the digest of the bytes `/packs/archive` serves.
/// `None` when `pack` does not share its files or its archive cannot be
/// built: the pack is then not downloadable.
/// [`emoji_packs::pack_archive`] writes entries in a fixed order with a
/// fixed timestamp, so the digest matches the download byte for byte.
async fn download_digest(dir: &Path, name: &str, pack: &emoji_packs::Pack) -> Option<String> {
    if !pack.pack.share_files {
        return None;
    }
    let bytes = emoji_packs::pack_archive(dir, name).await.ok()?;
    Some(sha256_hex_bytes(&bytes).to_ascii_uppercase())
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
    /// by [`list_packs`](emoji_packs::list_packs)); only the packs of the
    /// requested page have their archives hashed.
    async fn load(
        dir: &Path,
        all: Vec<(String, emoji_packs::Pack)>,
        page: usize,
        page_size: usize,
    ) -> Self {
        let count = all.len();
        let mut packs = BTreeMap::new();
        for (name, pack) in take_page(all.into_iter(), page, page_size) {
            let digest = download_digest(dir, &name, &pack).await;
            let _previous: Option<PackView> = packs.insert(name, PackView::full(pack, digest));
        }
        Self { count, packs }
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
    Ok(Json(
        PacksResponse::load(&state.config.emoji_dir, all, params.page, params.page_size).await,
    ))
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
    let name = params.required_name()?;
    let pack = load_or_404(&state, name).await?;
    let digest = download_digest(&state.config.emoji_dir, name, &pack).await;
    Ok(Json(PackView::paginated(
        pack,
        digest,
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
        let digest = Some("ABCDEF".to_owned());
        let value = serde_json::to_value(PackView::full(fixture_pack(), digest.clone())).unwrap();
        assert_eq!(
            value,
            json!({
                "pack": {
                    "license": "CC0-1.0",
                    "share-files": true,
                    "can-download": true,
                    "download-sha256": "ABCDEF",
                },
                "files": {"cat": "cat.png", "dog": "dog.png"},
                "files_count": 2,
            }),
            "metadata block with the advertisement, full shortcode map and count"
        );

        let paged =
            serde_json::to_value(PackView::paginated(fixture_pack(), digest, 2, 1)).unwrap();
        assert_eq!(
            paged,
            json!({
                "pack": {
                    "license": "CC0-1.0",
                    "share-files": true,
                    "can-download": true,
                    "download-sha256": "ABCDEF",
                },
                "files": {"dog": "dog.png"},
                "files_count": 2,
            }),
            "the second page carries one shortcode but still the full count"
        );
    }

    #[test]
    fn a_pack_without_a_digest_is_not_downloadable_and_drops_a_stored_hash() {
        let mut pack = fixture_pack();
        pack.pack.download_sha256 = Some("STALE".to_owned());
        let value = serde_json::to_value(PackView::full(pack, None)).unwrap();
        assert_eq!(
            value["pack"],
            json!({"license": "CC0-1.0", "share-files": true, "can-download": false}),
            "can-download is false and the stored digest is not advertised"
        );
    }

    #[tokio::test]
    async fn the_packs_response_pages_by_name_and_counts_everything() {
        let dir = tempfile::TempDir::new().unwrap();
        let all = vec![
            ("a".to_owned(), fixture_pack()),
            ("b".to_owned(), fixture_pack()),
            ("c".to_owned(), fixture_pack()),
        ];
        let value = serde_json::to_value(PacksResponse::load(dir.path(), all, 2, 2).await).unwrap();
        assert_eq!(value["count"], json!(3), "count is every pack on disk");
        assert_eq!(
            value["packs"]["c"]["pack"]["can-download"],
            json!(false),
            "a shared pack whose files are off disk cannot be downloaded"
        );
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
