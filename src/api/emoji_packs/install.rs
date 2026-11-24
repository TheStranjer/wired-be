//! The pack install routes: downloading a pack from another instance,
//! installing an uploaded zip and importing manifests from the
//! filesystem, with the manifest rules an installed archive follows.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
};

use axum::{Json, extract::State};
use tracing::debug;

use super::{
    AREA_EMOJI, CLASS_READ, CLASS_WRITE, COULD_NOT_DOWNLOAD, FILE_FIELD, INVALID_CHECKSUM,
    MISSING_FILE, OK_BODY, PACK_NOT_SHAREABLE, REMOTE_ARCHIVE_PATH,
    pack_writes::{resync, trimmed_name},
    params::PackParams,
    remote::{
        DownloadParams, DownloadRequest, DownloadZipParams, RemotePackListing, fetch_pack_listing,
        fetch_remote, instance_api_url,
    },
};
use crate::{
    auth::admin::AdminUser,
    error::{AppError, AppResult},
    extract::{
        ApiParams,
        multipart::{MultipartForm, UploadedFiles as _},
    },
    services::emoji_packs,
    state::AppState,
    util::token::sha256_hex_bytes,
};

/// Where the archive of `request`'s remote pack comes from, Pleroma's
/// `fetch_pack_info` decision: a pack that shares its files downloads
/// from its instance's archive endpoint; an unshared one only through a
/// `fallback-src` it names; a pack with neither is refused with the
/// ticket's pinned 400. Returns the archive URL with the SHA-256 the
/// download must match — the listing's `download-sha256` for a shared
/// pack, `fallback-src-sha256` for a fallback; like Pleroma, a source
/// whose digest is missing or blank is refused with the same 400.
fn archive_source(
    request: &DownloadRequest<'_>,
    listing: &RemotePackListing,
) -> AppResult<(String, String)> {
    let not_shareable = || AppError::BadRequest(PACK_NOT_SHAREABLE.to_owned());
    if listing.pack.share_files {
        let url = instance_api_url(
            request.base,
            REMOTE_ARCHIVE_PATH,
            &[("name", request.remote_name)],
        )?;
        let digest =
            non_blank(listing.pack.download_sha256.as_deref()).ok_or_else(not_shareable)?;
        return Ok((url, digest));
    }
    let url = non_blank(listing.pack.fallback_src.as_deref()).ok_or_else(not_shareable)?;
    let digest =
        non_blank(listing.pack.fallback_src_sha256.as_deref()).ok_or_else(not_shareable)?;
    Ok((url, digest))
}

/// `value` trimmed and owned, `None` when absent or blank.
fn non_blank(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

/// Whether `bytes` hash to the hex SHA-256 `expected`, compared
/// case-insensitively (Pleroma writes uppercase hex); a value that is
/// not a SHA-256 hex digest never matches.
fn checksum_matches(bytes: &[u8], expected: &str) -> bool {
    sha256_hex_bytes(bytes).eq_ignore_ascii_case(expected.trim())
}

/// Extracts a pack archive into the freshly created pack `name` and
/// writes the manifest that stands: the one the archive carries itself
/// when it parses and names files that landed, else `listing`'s map (a
/// remote pack's own shortcode map, without the archive digest that
/// described only the download) or the stems of the extracted
/// images (an upload without a manifest). Every entry of the stored
/// manifest names a file that actually arrived, so an installed pack
/// always lists, serves and archives. Returns the stored entry count.
///
/// # Errors
///
/// [`emoji_packs::extract_pack_archive`]'s refusals
/// ([`emoji_packs::ARCHIVE_UNSAFE_PATH`] and friends), whatever
/// [`emoji_packs::save_pack`] fails with; a directory the failed install
/// leaves behind is the caller's to [`remove_half_pack`].
async fn install_pack_archive(
    state: &AppState,
    name: &str,
    archive: &[u8],
    listing: Option<RemotePackListing>,
) -> AppResult<usize> {
    let extracted =
        emoji_packs::extract_pack_archive(&state.config.emoji_dir, name, archive).await?;
    let manifest = stored_manifest(state, name, &extracted)
        .await
        .unwrap_or_else(|| {
            listing.map_or_else(
                || manifest_from_images(&extracted),
                |listing| manifest_from_listing(&listing, &extracted),
            )
        });
    emoji_packs::save_pack(&state.config.emoji_dir, name, &manifest).await?;
    Ok(manifest.files.len())
}

/// The manifest the extracted archive brought with it: its `pack.json`,
/// loaded from the pack directory where the extraction wrote it and
/// reduced to the entries that are a valid shortcode naming a file that
/// landed. `None` when the archive shipped no manifest, none that
/// parses, or none that names a single installed file.
async fn stored_manifest(
    state: &AppState,
    name: &str,
    extracted: &BTreeSet<String>,
) -> Option<emoji_packs::Pack> {
    if !extracted.contains(emoji_packs::PACK_FILE) {
        return None;
    }
    let stored = emoji_packs::load_pack(&state.config.emoji_dir, name)
        .await
        .ok()?;
    let files: BTreeMap<String, String> = stored
        .files
        .into_iter()
        .filter(|(code, file)| emoji_packs::shortcode_valid(code) && extracted.contains(file))
        .collect();
    (!files.is_empty()).then_some(emoji_packs::Pack {
        pack: stored.pack,
        files,
    })
}

/// The manifest a remote pack gets from its listing: the metadata block
/// as the peer describes it, keeping only the files the extraction
/// landed (an archive may carry less than the listing claims, and never
/// more than this instance stores) under a valid shortcode: the peer
/// controls the keys, so any it names that fail
/// [`emoji_packs::shortcode_valid`] are dropped.
fn manifest_from_listing(
    listing: &RemotePackListing,
    extracted: &BTreeSet<String>,
) -> emoji_packs::Pack {
    emoji_packs::Pack {
        pack: emoji_packs::PackMeta {
            download_sha256: None,
            ..listing.pack.clone()
        },
        files: listing
            .files
            .iter()
            .filter(|(code, file)| emoji_packs::shortcode_valid(code) && extracted.contains(*file))
            .map(|(code, file)| (code.clone(), file.clone()))
            .collect(),
    }
}

/// The manifest an upload without one gets: every extracted image is an
/// emoji named after its file stem, nested names keeping their relative
/// path — Pleroma's `make_shortcode_to_file_map`.
pub(super) fn manifest_from_images(extracted: &BTreeSet<String>) -> emoji_packs::Pack {
    let mut pack = emoji_packs::Pack::default();
    for file in extracted {
        if file == emoji_packs::PACK_FILE {
            continue;
        }
        let Some(stem) = std::path::Path::new(file)
            .file_stem()
            .and_then(OsStr::to_str)
        else {
            continue;
        };
        if emoji_packs::shortcode_valid(stem) {
            let _entry: Option<String> = pack.files.insert(stem.to_owned(), file.clone());
        }
    }
    pack
}

/// Removes a pack directory an install left half-written, so a refused
/// download or upload stores nothing that cannot be served; a directory
/// that resists removal is logged — the install failure is the answer.
async fn remove_half_pack(state: &AppState, name: &str) {
    if let Err(err) = emoji_packs::delete_pack(&state.config.emoji_dir, name).await {
        debug!(
            pack = name,
            %err,
            "could not remove the half-installed emoji pack"
        );
    }
}

/// `POST /api/v1/pleroma/emoji/packs/download` — Pleroma's `download`:
/// install the pack `name` of the instance `url` under `as` (or the
/// remote name). The remote pack listing decides the source, per
/// [`archive_source`]: a shared pack downloads from its instance's
/// `packs/archive` endpoint, an unshared one only through a
/// `fallback-src`, and a pack with neither is the 400
/// `{"error":"The pack was not set as shared and there is no fallback
/// src to download from"}`. The zip — at most
/// [`MAX_PACK_ARCHIVE_BYTES`](emoji_packs::MAX_PACK_ARCHIVE_BYTES) —
/// is extracted safely into a fresh pack directory, so an occupied
/// local name is the 400 `{"error":"Pack already exists"}` and a
/// traversal or over-cap archive is refused with nothing left behind.
/// On success the custom emojis resync and the answer is the JSON
/// string `"ok"`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] from the name
/// and `url` checks, the pack-exists refusal and the extraction rules
/// ([`emoji_packs::ARCHIVE_UNSAFE_PATH`] and friends);
/// [`AppError::Unprocessable`] for a body without `url` or `name`;
/// [`AppError::InternalMessage`] for a failed remote fetch;
/// [`AppError::Internal`] for a filesystem failure; [`AppError::Database`]
/// from the resync.
pub(super) async fn download(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiParams(params): ApiParams<DownloadParams>,
) -> AppResult<Json<&'static str>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let request = DownloadRequest::from_params(&params)?;
    let mut listing = fetch_pack_listing(&state, &request, "0").await?;
    if listing.files.len() < listing.files_count {
        let full = listing.files_count.to_string();
        listing = fetch_pack_listing(&state, &request, &full).await?;
    }
    let (source, expected_sha256) = archive_source(&request, &listing)?;
    let archive = fetch_remote(
        &state,
        &source,
        emoji_packs::MAX_PACK_ARCHIVE_BYTES,
        COULD_NOT_DOWNLOAD,
    )
    .await?;
    if !checksum_matches(&archive.body, &expected_sha256) {
        return Err(AppError::BadRequest(INVALID_CHECKSUM.to_owned()));
    }
    emoji_packs::create_pack(&state.config.emoji_dir, request.local_name).await?;
    let installed =
        install_pack_archive(&state, request.local_name, &archive.body, Some(listing)).await;
    if let Err(err) = installed {
        remove_half_pack(&state, request.local_name).await;
        return Err(err);
    }
    resync(&state).await?;
    debug!(
        pack = request.local_name,
        "emoji pack downloaded from a remote instance"
    );
    Ok(Json(OK_BODY))
}

/// `POST /api/v1/pleroma/emoji/packs/download_zip` — Pleroma's
/// `download_zip`: create the pack `name` from an uploaded zip. The
/// same safe extraction as [`download`]; a `pack.json` inside the
/// archive is the manifest, and without one every extracted image
/// becomes an emoji named after its file stem. An occupied name is the
/// 400 `{"error":"Pack already exists"}`, a body without a `file` part
/// the 422, unusable archive contents the 400 of
/// [`emoji_packs::ARCHIVE_UNSAFE_PATH`] and friends. On success the
/// custom emojis resync and the answer is the JSON string `"ok"`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] for a blank or
/// unusable `name` and the extraction refusals;
/// [`AppError::Unprocessable`] for a body without any `file` part;
/// [`AppError::PayloadTooLarge`] for an upload past the configured
/// limits; [`AppError::Internal`] for a filesystem failure;
/// [`AppError::Database`] from the resync.
pub(super) async fn download_zip(
    State(state): State<AppState>,
    admin: AdminUser,
    params: PackParams,
    form: MultipartForm<DownloadZipParams>,
) -> AppResult<Json<&'static str>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let MultipartForm { fields, mut files } = form;
    let name = trimmed_name(fields.name.as_deref().or(params.name.as_deref()))?;
    let Some(upload) = files.take(FILE_FIELD) else {
        return Err(AppError::Unprocessable(MISSING_FILE.to_owned()));
    };
    emoji_packs::create_pack(&state.config.emoji_dir, name).await?;
    let installed = install_pack_archive(&state, name, &upload.bytes, None).await;
    if let Err(err) = installed {
        remove_half_pack(&state, name).await;
        return Err(err);
    }
    resync(&state).await?;
    debug!(pack = name, "emoji pack installed from an uploaded archive");
    Ok(Json(OK_BODY))
}

/// `GET /api/v1/pleroma/emoji/packs/import` — Pleroma's
/// `import_from_filesystem`: every pack directory of `EMOJI_DIR` that
/// lacks a valid `pack.json` gets one written from the image files it
/// holds ([`import_packs`](emoji_packs::import_packs)), and the names
/// imported answer as a JSON array; already-valid packs, hidden names
/// and loose files under the root are left alone. The custom emojis
/// resync afterwards, so an imported pack serves at once. An admin with
/// `admin:read` (the ticket's grant; Pleroma asks `admin:write`).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::Internal`] for a filesystem
/// failure; [`AppError::Database`] from the resync.
pub(super) async fn import(
    State(state): State<AppState>,
    admin: AdminUser,
) -> AppResult<Json<Vec<String>>> {
    admin.require(CLASS_READ, AREA_EMOJI)?;
    let names = emoji_packs::import_packs(&state.config.emoji_dir).await?;
    resync(&state).await?;
    debug!(packs = names.len(), "emoji pack manifests imported");
    Ok(Json(names))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::emoji_packs::test_support::download_body;

    /// A shared listing for the source-decision tests.
    fn listing_with(meta: emoji_packs::PackMeta) -> RemotePackListing {
        RemotePackListing {
            pack: meta,
            files: BTreeMap::new(),
            files_count: 0,
        }
    }

    #[test]
    fn a_shared_pack_downloads_from_its_archive_endpoint() {
        let body = download_body(
            "https://remote.example",
            "cats and dogs",
            Some("packed-cats"),
        );
        let request = DownloadRequest::from_params(&body).unwrap();
        assert_eq!(request.local_name, "packed-cats");
        let shared = listing_with(emoji_packs::PackMeta {
            share_files: true,
            download_sha256: Some(" ABCD ".to_owned()),
            fallback_src: Some("https://files.example/ignored.zip".to_owned()),
            fallback_src_sha256: Some("ffff".to_owned()),
            ..emoji_packs::PackMeta::default()
        });
        assert_eq!(
            archive_source(&request, &shared).unwrap(),
            (
                "https://remote.example/api/v1/pleroma/emoji/packs/archive?name=cats+and+dogs"
                    .to_owned(),
                "ABCD".to_owned()
            ),
            "the instance's own archive endpoint, name encoded, with download-sha256"
        );
        for digest in [None, Some("  ".to_owned())] {
            let undeclared = listing_with(emoji_packs::PackMeta {
                share_files: true,
                download_sha256: digest,
                fallback_src_sha256: Some("ffff".to_owned()),
                ..emoji_packs::PackMeta::default()
            });
            let err = archive_source(&request, &undeclared).unwrap_err();
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "a shared pack without download-sha256 is a 400, not {err}"
            );
            assert_eq!(err.to_string(), PACK_NOT_SHAREABLE, "the pinned wording");
        }
    }

    #[test]
    fn an_unshared_pack_follows_only_its_fallback_src() {
        let body = download_body("https://x.test", "cats", None);
        let request = DownloadRequest::from_params(&body).unwrap();
        let with_src = listing_with(emoji_packs::PackMeta {
            fallback_src: Some("  https://files.example/cats.zip  ".to_owned()),
            fallback_src_sha256: Some("beef".to_owned()),
            download_sha256: Some("ignored".to_owned()),
            ..emoji_packs::PackMeta::default()
        });
        assert_eq!(
            archive_source(&request, &with_src).unwrap(),
            (
                "https://files.example/cats.zip".to_owned(),
                "beef".to_owned()
            ),
            "the declared archive, as given, with fallback-src-sha256"
        );
        for meta in [
            emoji_packs::PackMeta::default(),
            emoji_packs::PackMeta {
                fallback_src: Some("   ".to_owned()),
                fallback_src_sha256: Some("beef".to_owned()),
                ..emoji_packs::PackMeta::default()
            },
            emoji_packs::PackMeta {
                fallback_src: Some("https://files.example/cats.zip".to_owned()),
                ..emoji_packs::PackMeta::default()
            },
            emoji_packs::PackMeta {
                fallback_src: Some("https://files.example/cats.zip".to_owned()),
                fallback_src_sha256: Some(" ".to_owned()),
                ..emoji_packs::PackMeta::default()
            },
        ] {
            let err = archive_source(&request, &listing_with(meta)).unwrap_err();
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "nothing to download from is a 400, not {err}"
            );
            assert_eq!(err.to_string(), PACK_NOT_SHAREABLE, "the pinned wording");
        }
    }

    #[test]
    fn the_checksum_comparison_ignores_case_and_rejects_everything_else() {
        let bytes = b"an archive";
        let lower = sha256_hex_bytes(bytes);
        assert_eq!(lower.len(), 64, "a SHA-256 is 64 hex digits");
        assert!(checksum_matches(bytes, &lower), "lowercase hex matches");
        assert!(
            checksum_matches(bytes, &lower.to_uppercase()),
            "uppercase hex, as Pleroma writes it, matches"
        );
        assert!(
            checksum_matches(bytes, &format!(" {lower}\n")),
            "surrounding whitespace is not part of the digest"
        );
        assert!(
            !checksum_matches(b"another archive", &lower),
            "other bytes do not match"
        );
        assert!(
            !checksum_matches(bytes, &sha256_hex_bytes(b"other")),
            "a wrong digest is refused"
        );
        let truncated: String = lower.chars().take(63).collect();
        for bad in ["", "beef", "not hex at all", truncated.as_str()] {
            assert!(!checksum_matches(bytes, bad), "{bad:?} is no digest of it");
        }
    }

    #[test]
    fn the_listing_manifest_keeps_only_files_that_landed() {
        let listing = RemotePackListing {
            pack: emoji_packs::PackMeta {
                license: Some("MIT".to_owned()),
                share_files: true,
                download_sha256: Some("beef".to_owned()),
                ..emoji_packs::PackMeta::default()
            },
            files: BTreeMap::from([
                ("cat".to_owned(), "cat.png".to_owned()),
                ("gone".to_owned(), "gone.png".to_owned()),
                ("nested".to_owned(), "sub/nested.gif".to_owned()),
            ]),
            files_count: 3,
        };
        let extracted = BTreeSet::from(["cat.png".to_owned(), "sub/nested.gif".to_owned()]);
        let pack = manifest_from_listing(&listing, &extracted);
        assert_eq!(
            pack.pack.license.as_deref(),
            Some("MIT"),
            "the remote metadata rides into the stored manifest"
        );
        assert!(pack.pack.share_files, "including its share flag");
        assert_eq!(
            pack.pack.download_sha256, None,
            "but not the digest of the download"
        );
        assert_eq!(
            pack.files
                .iter()
                .map(|(code, file)| (code.as_str(), file.as_str()))
                .collect::<Vec<_>>(),
            [("cat", "cat.png"), ("nested", "sub/nested.gif")],
            "the announced file that never landed is not entered"
        );
    }

    #[test]
    fn the_listing_manifest_drops_invalid_shortcodes() {
        let listing = RemotePackListing {
            pack: emoji_packs::PackMeta::default(),
            files: BTreeMap::from([
                ("ok".to_owned(), "a.png".to_owned()),
                ("bad code".to_owned(), "a.png".to_owned()),
                ("<x>".to_owned(), "a.png".to_owned()),
                (":colon:".to_owned(), "a.png".to_owned()),
                (format!("{}\"", "a".repeat(10)), "a.png".to_owned()),
                (String::new(), "a.png".to_owned()),
            ]),
            files_count: 6,
        };
        let extracted = BTreeSet::from(["a.png".to_owned()]);
        let pack = manifest_from_listing(&listing, &extracted);
        assert_eq!(
            pack.files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["ok"],
            "only the valid shortcode is entered, though every file landed"
        );
    }

    #[test]
    fn an_upload_manifest_names_emojis_after_the_file_stems() {
        let extracted = BTreeSet::from([
            emoji_packs::PACK_FILE.to_owned(),
            "cat.png".to_owned(),
            "cat.webp".to_owned(),
            "sub/kitten.gif".to_owned(),
            "sub/kitten.jpg".to_owned(),
            "no stem.png".to_owned(),
        ]);
        let pack = manifest_from_images(&extracted);
        assert_eq!(
            pack.files
                .iter()
                .map(|(code, file)| (code.as_str(), file.as_str()))
                .collect::<Vec<_>>(),
            [("cat", "cat.webp"), ("kitten", "sub/kitten.jpg")],
            "one emoji per stem, the last extracted name wins, the manifest itself is no emoji, and a stem the pattern refuses is skipped"
        );
        assert!(
            !pack.pack.share_files,
            "a stem-built pack starts unshared, its provenance unknown"
        );
    }
}
