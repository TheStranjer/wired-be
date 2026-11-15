//! Emoji packs: Pleroma's on-disk custom emoji format, and the sync that
//! mirrors it into the `custom_emojis` table.
//!
//! Pleroma stores custom emoji as pack directories in its static dir's
//! `emoji` subdirectory (`lib/pleroma/emoji/pack.ex`): one directory per
//! pack, each holding a `pack.json` manifest plus the image files it names.
//! Here the root is `EMOJI_DIR`
//! ([`Config::emoji_dir`](crate::config::Config::emoji_dir)),
//! the pack's category name is its directory name, and a manifest looks like
//!
//! ```json
//! {
//!   "pack": {
//!     "license": "...", "homepage": "...", "description": "...",
//!     "fallback-src": "...", "fallback-src-sha256": "...", "share-files": true
//!   },
//!   "files": {"shortcode": "filename.png"}
//! }
//! ```
//!
//! exactly [`Pack`]/[`PackMeta`] read and write (unknown manifest keys such
//! as Pleroma's `files_extra` are ignored). Every operation takes the packs
//! root as its first argument: app state travels explicitly, never as a
//! global.
//!
//! Names are the safety boundary. A pack [`pack_name_valid`] or file
//! [`file_name_valid`] would reject — anything but `[A-Za-z0-9_.-]` runs
//! without leading dots, and for files those runs joined by `/` — never
//! reaches the filesystem, so no client name can traverse out of
//! `EMOJI_DIR` (the same rule Pleroma's `path_join_name_safe` enforces by
//! raising).
//!
//! [`sync_custom_emojis`] is the DB side: every pack file becomes a local
//! `custom_emojis` row (`category` = pack name, `image_url` =
//! `{BASE_URL}/emoji/{pack}/{file}`, the address the static route of
//! [`crate::api::emoji_packs`] serves), and local emojis under that prefix
//! that no pack names any more are deleted. Call it at startup and after
//! any change to the packs directory.

use std::{
    collections::BTreeMap,
    io::{self, Cursor, Write as _},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tracing::debug;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{
    error::{AppError, AppResult},
    models::custom_emoji::{self, NewCustomEmoji},
};

/// Name of the manifest file every pack directory holds (Pleroma's
/// `Path.join(dir, "pack.json")`, `lib/pleroma/emoji/pack.ex`).
pub const PACK_FILE: &str = "pack.json";
/// Body of the 403 the archive endpoint answers for a pack whose files are
/// not shared.
pub const PACK_NOT_SHARED: &str = "Pack is not shared";
/// Body of the 400 [`create_pack`] answers when the pack directory already
/// exists (Pleroma's `File.mkdir` `:eexist`; the admin ticket pins the
/// status at 400, not Pleroma's 409).
pub const PACK_EXISTS: &str = "Pack already exists";
/// The manifest [`create_pack`] writes: empty metadata and no files —
/// Pleroma's `save_pack(%Pack{})` of a freshly `mkdir`ed directory.
pub const NEW_PACK_MANIFEST: &str = r#"{"pack": {}, "files": {}}"#;

/// The metadata block of a `pack.json`: all of it optional on read (an
/// empty manifest `{"files": {}}` is what Pleroma's `create` writes), with
/// `share-files` defaulting to `false`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackMeta {
    /// The pack's license identifier, e.g. an SPDX expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// The pack's home page URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Free-form description of the pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// URL of an external archive holding the pack's files.
    #[serde(
        default,
        rename = "fallback-src",
        skip_serializing_if = "Option::is_none"
    )]
    pub fallback_src: Option<String>,
    /// SHA-256 of the fallback archive, lowercase hex.
    #[serde(
        default,
        rename = "fallback-src-sha256",
        skip_serializing_if = "Option::is_none"
    )]
    pub fallback_src_sha256: Option<String>,
    /// Whether this instance may hand the pack's files to others: the
    /// archive endpoint's gate, Pleroma's `pack.pack["share-files"]`.
    #[serde(default, rename = "share-files")]
    pub share_files: bool,
}

/// One emoji pack as its `pack.json` stores it: the metadata block plus the
/// shortcode map. The pack's name (and category) is its directory name, so
/// it is deliberately not part of this value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pack {
    /// The manifest's metadata block.
    #[serde(default)]
    pub pack: PackMeta,
    /// Shortcode to file name (relative to the pack directory); ordered for
    /// stable pagination and byte-identical saves.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

/// Whether `segment` may name a pack directory or one path segment of a
/// file: a non-empty run of `[A-Za-z0-9_.-]` that does not start with a dot
/// (so `.`, `..`, hidden entries and every traversal spelling fail).
fn segment_valid(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.starts_with('.')
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Whether `name` may be the name of a pack directory: a single valid
/// segment, so it can never escape `EMOJI_DIR` or hide a file.
#[must_use]
pub fn pack_name_valid(name: &str) -> bool {
    segment_valid(name)
}

/// Whether `file` may name a file inside a pack: one or more valid segments
/// joined by `/` (Pleroma's packs keep subdirectories in `files` values).
/// Absolute paths, empty segments and `..` are rejected.
#[must_use]
pub fn file_name_valid(file: &str) -> bool {
    !file.is_empty() && file.split('/').all(segment_valid)
}

/// Rejects a pack name that [`pack_name_valid`] would refuse, for the
/// service entry points that take a client-supplied name.
fn check_pack_name(name: &str) -> AppResult<()> {
    if pack_name_valid(name) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("invalid pack name: {name}")))
    }
}

/// Wraps a filesystem failure, keeping the operation and path for the log:
/// [`AppError::Internal`] never sends its detail to a client.
fn io_error(operation: &str, path: &Path, err: &io::Error) -> AppError {
    AppError::Internal(format!("{operation} {} failed: {err}", path.display()))
}

/// The 403 for anything an archive may not include: a pack that is not
/// shared, and Pleroma's `downloadable?` condition — a pack whose files are
/// all present — that a broken pack fails.
fn not_shared() -> AppError {
    AppError::Forbidden(PACK_NOT_SHARED.to_owned())
}

/// The directory of pack `name` under the packs root `dir`.
fn pack_dir(dir: &Path, name: &str) -> AppResult<PathBuf> {
    check_pack_name(name)?;
    Ok(dir.join(name))
}

/// The public URL of `file` in pack `name`, as [`sync_custom_emojis`]
/// stores it: `{base_url}/emoji/{name}/{file}`, the address the static
/// emoji route of [`crate::api::emoji_packs`] serves.
#[must_use]
pub fn emoji_image_url(base_url: &str, name: &str, file: &str) -> String {
    format!("{base_url}/emoji/{name}/{file}")
}

/// Reads pack `name` from the packs root `dir`, parsing its `pack.json`.
///
/// # Errors
///
/// [`AppError::BadRequest`] if `name` is not [`pack_name_valid`],
/// [`AppError::NotFound`] if the pack or its manifest does not exist, and
/// [`AppError::Internal`] for an unreadable or unparsable manifest or any
/// other filesystem failure.
pub async fn load_pack(dir: &Path, name: &str) -> AppResult<Pack> {
    let path = pack_dir(dir, name)?.join(PACK_FILE);
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(AppError::NotFound),
        Err(err) => return Err(io_error("read pack manifest", &path, &err)),
    };
    serde_json::from_str(&text).map_err(|err| {
        AppError::Internal(format!(
            "parse pack manifest {} failed: {err}",
            path.display()
        ))
    })
}

/// Writes `pack` as the `pack.json` of pack `name` under `dir`.
///
/// Creates the pack directory if it does not exist; the pack's files are
/// not touched — this persists the manifest only.
///
/// # Errors
///
/// [`AppError::BadRequest`] if `name` is not [`pack_name_valid`] and
/// [`AppError::Internal`] for any filesystem or encoding failure.
pub async fn save_pack(dir: &Path, name: &str, pack: &Pack) -> AppResult<()> {
    let path = pack_dir(dir, name)?;
    tokio::fs::create_dir_all(&path)
        .await
        .map_err(|err| io_error("create pack directory", &path, &err))?;
    let json = serde_json::to_string_pretty(pack)
        .map_err(|err| AppError::Internal(format!("encode pack {name} failed: {err}")))?;
    let manifest = path.join(PACK_FILE);
    tokio::fs::write(&manifest, json)
        .await
        .map_err(|err| io_error("write pack manifest", &manifest, &err))?;
    debug!(pack = name, files = pack.files.len(), "saved emoji pack");
    Ok(())
}

/// Creates pack `name` under the packs root `dir`: a fresh directory and
/// the default [`NEW_PACK_MANIFEST`], Pleroma's `create` (`File.mkdir`
/// followed by `save_pack(%Pack{})`).
///
/// The directory is the existence boundary, as in Pleroma: a name whose
/// directory is there already exists whether or not it holds a manifest.
/// Existing files are never touched.
///
/// # Errors
///
/// [`AppError::BadRequest`] if `name` is not [`pack_name_valid`] or the
/// pack already exists ([`PACK_EXISTS`]), [`AppError::Internal`] for any
/// other filesystem failure.
pub async fn create_pack(dir: &Path, name: &str) -> AppResult<()> {
    let path = pack_dir(dir, name)?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|err| io_error("create emoji packs root", dir, &err))?;
    match tokio::fs::create_dir(&path).await {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return Err(AppError::BadRequest(PACK_EXISTS.to_owned()));
        }
        Err(err) => return Err(io_error("create pack directory", &path, &err)),
    }
    let manifest = path.join(PACK_FILE);
    tokio::fs::write(&manifest, NEW_PACK_MANIFEST)
        .await
        .map_err(|err| io_error("write pack manifest", &manifest, &err))?;
    debug!(pack = name, "created emoji pack");
    Ok(())
}

/// Removes pack `name` — the whole directory with its files — from the
/// packs root `dir`, Pleroma's `delete` (`File.rm_rf`).
///
/// # Errors
///
/// [`AppError::BadRequest`] if `name` is not [`pack_name_valid`],
/// [`AppError::NotFound`] if the pack directory does not exist, and
/// [`AppError::Internal`] for any other filesystem failure.
pub async fn delete_pack(dir: &Path, name: &str) -> AppResult<()> {
    let path = pack_dir(dir, name)?;
    match tokio::fs::remove_dir_all(&path).await {
        Ok(()) => {
            debug!(pack = name, "deleted emoji pack");
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(AppError::NotFound),
        Err(err) => Err(io_error("delete pack directory", &path, &err)),
    }
}

/// Every pack under the packs root `dir`, ordered by name.
///
/// Entries without a readable, parsable `pack.json` (plain files,
/// half-installed or broken packs) are skipped, as in Pleroma's
/// `list_local`; a missing root lists as no packs.
///
/// # Errors
///
/// [`AppError::Internal`] if the directory exists but cannot be listed.
pub async fn list_packs(dir: &Path) -> AppResult<Vec<(String, Pack)>> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Ok(Vec::new());
    };
    let mut packs = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|err| io_error("list emoji packs", dir, &err))?
    {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Ok(pack) = load_pack(dir, &name).await {
            packs.push((name, pack));
        }
    }
    packs.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
    Ok(packs)
}

/// Reads `file` of pack `name` from the packs root `dir`.
///
/// This is the storage behind the static emoji route of
/// [`crate::api::emoji_packs`].
///
/// # Errors
///
/// [`AppError::BadRequest`] if either name is unusable,
/// [`AppError::NotFound`] if the file does not exist (including a path that
/// names a directory), and [`AppError::Internal`] for other read failures.
pub async fn read_pack_file(dir: &Path, name: &str, file: &str) -> AppResult<Vec<u8>> {
    if !file_name_valid(file) {
        return Err(AppError::BadRequest(format!(
            "invalid emoji file name: {file}"
        )));
    }
    let path = pack_dir(dir, name)?.join(file);
    match tokio::fs::read(&path).await {
        Ok(bytes) => Ok(bytes),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(AppError::NotFound),
        Err(err) => Err(io_error("read emoji file", &path, &err)),
    }
}

/// The whole pack as a zip archive, ready for the download endpoint.
///
/// The archive holds `pack.json` first, then every file listed in it under
/// its relative name — Pleroma's `["pack.json" | pack_file_list]` in
/// `create_archive_and_cache`.
///
/// # Errors
///
/// Whatever [`load_pack`] answers, [`AppError::Forbidden`]
/// ([`PACK_NOT_SHARED`]) if the pack does not share its files, if one of
/// its file names is unusable, or if a listed file is missing from disk —
/// Pleroma's `downloadable?` condition; [`AppError::Internal`] for any
/// other read or archive failure.
pub async fn pack_archive(dir: &Path, name: &str) -> AppResult<Vec<u8>> {
    let pack = load_pack(dir, name).await?;
    if !pack.pack.share_files {
        return Err(not_shared());
    }
    let pack_dir = pack_dir(dir, name)?;
    let manifest = tokio::fs::read(pack_dir.join(PACK_FILE))
        .await
        .map_err(|err| io_error("read pack manifest", &pack_dir.join(PACK_FILE), &err))?;
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip_write(&mut writer, PACK_FILE, options, &manifest)?;
    for file in pack.files.values() {
        let bytes = read_archive_entry(&pack_dir, file).await?;
        zip_write(&mut writer, file, options, &bytes)?;
    }
    let cursor = writer
        .finish()
        .map_err(|err| AppError::Internal(format!("finish emoji pack archive failed: {err}")))?;
    debug!(
        pack = name,
        files = pack.files.len(),
        "built emoji pack archive"
    );
    Ok(cursor.into_inner())
}

/// The bytes of one archive entry, missing or unsafe files answered as
/// [`not_shared`] rather than served or half-packaged.
async fn read_archive_entry(pack_dir: &Path, file: &str) -> AppResult<Vec<u8>> {
    if !file_name_valid(file) {
        return Err(not_shared());
    }
    let path = pack_dir.join(file);
    match tokio::fs::read(&path).await {
        Ok(bytes) => Ok(bytes),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(not_shared()),
        Err(err) => Err(io_error("read pack file", &path, &err)),
    }
}

/// One zip entry: `name` with `options`, holding `bytes`.
fn zip_write<W: io::Write + io::Seek>(
    writer: &mut ZipWriter<W>,
    name: &str,
    options: SimpleFileOptions,
    bytes: &[u8],
) -> AppResult<()> {
    const FAIL: &str = "write emoji pack archive failed";
    writer
        .start_file(name, options)
        .map_err(|err| AppError::Internal(format!("{FAIL}: {err}")))?;
    writer
        .write_all(bytes)
        .map_err(|err| AppError::Internal(format!("{FAIL}: {err}")))?;
    Ok(())
}

/// Reconciles the `custom_emojis` table with the packs under `dir`.
///
/// Every pack file is upserted as a local emoji (`category` and sole tag =
/// the pack name, `image_url` = [`emoji_image_url`]), and local emojis
/// under the `{base_url}/emoji/` prefix that no pack names any more are
/// deleted. Remote emojis and rows outside the prefix are never touched.
/// Call this at startup and after every change to the packs directory.
///
/// Returns the number of pack files stored.
///
/// # Errors
///
/// [`AppError::Internal`] for a packs directory that cannot be listed, and
/// any database failure as [`AppError::Database`](crate::error::AppError).
pub async fn sync_custom_emojis(pool: &PgPool, base_url: &str, dir: &Path) -> AppResult<usize> {
    let packs = list_packs(dir).await?;
    let mut stored = Vec::new();
    for (name, pack) in &packs {
        for (shortcode, file) in &pack.files {
            let new = NewCustomEmoji {
                category: Some(name.clone()),
                tags: vec![name.clone()],
                ..NewCustomEmoji::local(shortcode, &emoji_image_url(base_url, name, file))
            };
            let _row: custom_emoji::CustomEmoji = custom_emoji::upsert(pool, &new).await?;
            stored.push(shortcode.clone());
        }
    }
    let prefix = format!("{base_url}/emoji/");
    let removed = custom_emoji::delete_local_under(pool, &prefix, &stored).await?;
    debug!(
        packs = packs.len(),
        emojis = stored.len(),
        removed,
        "synced emoji packs into custom_emojis"
    );
    Ok(stored.len())
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use sqlx::PgPool;
    use tempfile::TempDir;
    use zip::ZipArchive;

    use super::*;

    /// The instance origin the sync builds URLs from.
    const BASE: &str = "https://test.example";
    /// Bytes for a fake image file.
    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfake";

    /// A throwaway packs root; removed when the guard drops.
    fn temp_dir() -> TempDir {
        TempDir::new().unwrap()
    }

    /// A pack manifest naming `files` (shortcode, file name) pairs, sharing
    /// its files when `shared`.
    fn pack_with(shared: bool, files: &[(&str, &str)]) -> Pack {
        Pack {
            pack: PackMeta {
                share_files: shared,
                ..PackMeta::default()
            },
            files: files
                .iter()
                .map(|(shortcode, file)| ((*shortcode).to_owned(), (*file).to_owned()))
                .collect(),
        }
    }

    /// Writes `contents` to `path`, creating parent directories.
    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    /// Installs pack `name` under `dir`: the shared manifest plus real
    /// files for every entry of `files`.
    async fn seed_pack(dir: &Path, name: &str, files: &[(&str, &str)]) {
        save_pack(dir, name, &pack_with(true, files)).await.unwrap();
        let pack_dir = dir.join(name);
        for (_, file) in files {
            write_file(&pack_dir.join(file), PNG_BYTES);
        }
    }

    #[tokio::test]
    async fn save_then_load_round_trips_through_pack_json() {
        let dir = temp_dir();
        let pack = Pack {
            pack: PackMeta {
                license: Some("CC0-1.0".to_owned()),
                homepage: Some("https://emoji.example".to_owned()),
                description: Some("Test pack".to_owned()),
                fallback_src: Some("https://emoji.example/pack.zip".to_owned()),
                fallback_src_sha256: Some("beef".to_owned()),
                share_files: true,
            },
            files: BTreeMap::from([("cat".to_owned(), "cat.png".to_owned())]),
        };
        save_pack(dir.path(), "testpack", &pack).await.unwrap();
        let raw = std::fs::read_to_string(dir.path().join("testpack").join(PACK_FILE)).unwrap();
        assert!(
            raw.contains(r#""share-files": true"#) && raw.contains(r#""cat": "cat.png""#),
            "the manifest is Pleroma's shape: {raw}"
        );
        assert_eq!(load_pack(dir.path(), "testpack").await.unwrap(), pack);
    }

    #[tokio::test]
    async fn loads_a_pleroma_manifest_with_extra_keys() {
        let dir = temp_dir();
        write_file(
            &dir.path().join("pleroma").join(PACK_FILE),
            br#"{
              "files_count": 1,
              "files": {"cat": "cat.png"},
              "files_extra": {"cat": {"tags": ["pack:pleroma"]}},
              "pack": {
                "license": "MIT",
                "share-files": true,
                "fallback-src": "https://x.test/a.zip",
                "whatever": "ignored"
              }
            }"#,
        );
        let pack = load_pack(dir.path(), "pleroma").await.unwrap();
        assert!(pack.pack.share_files, "the share flag parsed");
        assert_eq!(
            pack.pack.fallback_src.as_deref(),
            Some("https://x.test/a.zip"),
            "the dashed keys renamed"
        );
        assert_eq!(pack.pack.license.as_deref(), Some("MIT"), "license parsed");
        assert_eq!(
            pack.files.get("cat").map(String::as_str),
            Some("cat.png"),
            "the shortcode map parsed"
        );
    }

    #[tokio::test]
    async fn an_empty_manifest_defaults_to_unshared_and_no_files() {
        let dir = temp_dir();
        write_file(&dir.path().join("bare").join(PACK_FILE), br#"{"pack": {}}"#);
        let pack = load_pack(dir.path(), "bare").await.unwrap();
        assert!(!pack.pack.share_files, "missing share-files means false");
        assert!(pack.files.is_empty(), "missing files means none");
    }

    #[tokio::test]
    async fn a_missing_pack_is_not_found_and_a_broken_one_is_internal() {
        let dir = temp_dir();
        assert!(
            matches!(
                load_pack(dir.path(), "nope").await.unwrap_err(),
                AppError::NotFound
            ),
            "an absent pack is a 404"
        );
        write_file(&dir.path().join("broken").join(PACK_FILE), b"{ not json");
        assert!(
            matches!(
                load_pack(dir.path(), "broken").await.unwrap_err(),
                AppError::Internal(_)
            ),
            "an unparsable manifest is a server-side failure"
        );
    }

    #[test]
    fn pack_names_are_validated() {
        for name in ["a", "pack-1", "pack_1", "Pack.png", "0", "dots..in..name"] {
            assert!(pack_name_valid(name), "{name} is a pack name");
        }
        for name in [
            "",
            ".",
            "..",
            ".hidden",
            "../escape",
            "a/b",
            "a b",
            "uni é",
            "nul\0",
            "x;",
        ] {
            assert!(!pack_name_valid(name), "{name} is not a pack name");
        }
    }

    #[test]
    fn file_names_are_validated() {
        for file in ["a.png", "sub/deep.gif", "dots..in.name", "a/b/c.webp"] {
            assert!(file_name_valid(file), "{file} is a file name");
        }
        for file in [
            "",
            "/abs.png",
            "a/../b.png",
            "..",
            "a/..",
            "./rel.png",
            ".hidden",
            "a//b",
            "trailing/",
            "space name.png",
            "back\\slash.png",
            "é.png",
        ] {
            assert!(!file_name_valid(file), "{file} is not a file name");
        }
    }

    #[tokio::test]
    async fn unusable_names_never_reach_the_filesystem() {
        let dir = temp_dir();
        write_file(&dir.path().join("outside.txt"), b"secret");
        for name in ["../outside.txt", ".", "", "a/b"] {
            let load_err = load_pack(dir.path(), name).await.unwrap_err();
            assert!(
                matches!(load_err, AppError::BadRequest(_)),
                "load rejects {name:?} with a 400: {load_err}"
            );
            let save_err = save_pack(dir.path(), name, &Pack::default())
                .await
                .unwrap_err();
            assert!(
                matches!(save_err, AppError::BadRequest(_)),
                "save rejects {name:?} with a 400: {save_err}"
            );
        }
        for file in ["../outside.txt", "/etc/passwd", "a/../../b"] {
            let err = read_pack_file(dir.path(), "whatever", file)
                .await
                .unwrap_err();
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "read rejects {file:?} with a 400: {err}"
            );
        }
        // The rejected save created nothing beside the root, and the bytes
        // of the outside file are untouched.
        let siblings = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(siblings, ["outside.txt"], "no pack directory was created");
        assert_eq!(
            std::fs::read(dir.path().join("outside.txt")).unwrap(),
            b"secret",
            "the file outside any pack stayed as it was"
        );
    }

    /// The error a refused [`create_pack`] of an occupied name carries.
    fn exists_error(err: &AppError) -> bool {
        matches!(err, AppError::BadRequest(_)) && err.to_string() == PACK_EXISTS
    }

    #[tokio::test]
    async fn create_pack_makes_the_directory_and_the_default_manifest() {
        let dir = temp_dir();
        create_pack(dir.path(), "fresh").await.unwrap();
        let raw = std::fs::read_to_string(dir.path().join("fresh").join(PACK_FILE)).unwrap();
        assert_eq!(raw, NEW_PACK_MANIFEST, "the manifest is the pinned default");
        assert_eq!(
            load_pack(dir.path(), "fresh").await.unwrap(),
            Pack::default(),
            "and it parses back as the empty pack"
        );
    }

    #[tokio::test]
    async fn create_pack_makes_a_missing_root_but_never_leaves_it() {
        let dir = temp_dir();
        let root = dir.path().join("emoji");
        create_pack(&root, "late").await.unwrap();
        assert!(
            root.join("late").join(PACK_FILE).exists(),
            "root and pack created"
        );
    }

    #[tokio::test]
    async fn creating_an_existing_pack_reports_exists_and_changes_nothing() {
        let dir = temp_dir();
        seed_pack(dir.path(), "cats", &[("cat", "cat.png")]).await;
        let err = create_pack(dir.path(), "cats").await.unwrap_err();
        assert!(exists_error(&err), "an occupied name is PACK_EXISTS: {err}");
        let kept = load_pack(dir.path(), "cats").await.unwrap();
        assert_eq!(
            kept.files.get("cat").map(String::as_str),
            Some("cat.png"),
            "the manifest survived the refused create"
        );
        assert_eq!(
            std::fs::read(dir.path().join("cats").join("cat.png")).unwrap(),
            PNG_BYTES,
            "and so did the files"
        );
        // A half-installed pack — a directory without a manifest — also
        // exists as far as Pleroma's `File.mkdir` is concerned.
        std::fs::create_dir_all(dir.path().join("bare")).unwrap();
        let bare_err = create_pack(dir.path(), "bare").await.unwrap_err();
        assert!(
            exists_error(&bare_err),
            "a bare directory exists too: {bare_err}"
        );
    }

    #[tokio::test]
    async fn delete_pack_removes_the_directory_and_reports_absent_packs() {
        let dir = temp_dir();
        seed_pack(
            dir.path(),
            "cats",
            &[("cat", "cat.png"), ("kitten", "sub/kitten.png")],
        )
        .await;
        seed_pack(dir.path(), "dogs", &[("dog", "dog.png")]).await;
        delete_pack(dir.path(), "cats").await.unwrap();
        assert!(
            !dir.path().join("cats").exists(),
            "the whole pack directory is gone"
        );
        assert!(
            dir.path().join("dogs").join(PACK_FILE).exists(),
            "sibling packs stay"
        );
        let err = delete_pack(dir.path(), "cats").await.unwrap_err();
        assert!(matches!(err, AppError::NotFound), "twice is a 404: {err}");
    }

    #[tokio::test]
    async fn create_and_delete_reject_unusable_names_before_the_filesystem() {
        let dir = temp_dir();
        write_file(&dir.path().join("outside.txt"), b"secret");
        for name in [
            "../outside.txt",
            "..",
            ".",
            "",
            ".hidden",
            "a/b",
            "/abs",
            "é",
        ] {
            let create_err = create_pack(dir.path(), name).await.unwrap_err();
            assert!(
                matches!(create_err, AppError::BadRequest(_)),
                "create rejects {name:?} with a 400: {create_err}"
            );
            let delete_err = delete_pack(dir.path(), name).await.unwrap_err();
            assert!(
                matches!(delete_err, AppError::BadRequest(_)),
                "delete rejects {name:?} with a 400: {delete_err}"
            );
        }
        assert_eq!(
            std::fs::read(dir.path().join("outside.txt")).unwrap(),
            b"secret",
            "the file outside any pack is neither touched nor removed"
        );
    }

    #[tokio::test]
    async fn list_packs_sorts_by_name_and_skips_unreadable_entries() {
        let dir = temp_dir();
        save_pack(dir.path(), "b-pack", &pack_with(true, &[("b", "b.png")]))
            .await
            .unwrap();
        save_pack(dir.path(), "a-pack", &pack_with(false, &[("a", "a.png")]))
            .await
            .unwrap();
        std::fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        write_file(&dir.path().join(".hidden").join(PACK_FILE), b"{}");
        std::fs::create_dir_all(dir.path().join("no-manifest")).unwrap();
        write_file(&dir.path().join("stray.png"), PNG_BYTES);
        let listed = list_packs(dir.path()).await.unwrap();
        let names: Vec<&str> = listed.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["a-pack", "b-pack"], "only loadable packs, sorted");
    }

    #[tokio::test]
    async fn listing_a_missing_root_is_no_packs() {
        let dir = temp_dir();
        let listed = list_packs(&dir.path().join("nowhere")).await.unwrap();
        assert!(listed.is_empty(), "a missing emoji dir lists empty");
    }

    #[tokio::test]
    async fn the_archive_holds_the_manifest_and_every_file() {
        let dir = temp_dir();
        seed_pack(
            dir.path(),
            "shared",
            &[("cat", "cat.png"), ("dog", "sub/dog.png")],
        )
        .await;
        let bytes = pack_archive(dir.path(), "shared").await.unwrap();
        let mut archive = ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut names: Vec<String> = archive
            .file_names()
            .map(|name| name.unwrap().into_owned())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["cat.png", "pack.json", "sub/dog.png"],
            "zip entries"
        );
        let manifest = {
            let mut entry = archive.by_name(PACK_FILE).unwrap();
            let mut text = String::new();
            let written: usize = entry.read_to_string(&mut text).unwrap();
            assert!(!text.is_empty(), "the manifest entry holds {written} bytes");
            text
        };
        assert!(
            manifest.contains(r#""cat": "cat.png""#),
            "the manifest rode along: {manifest}"
        );
        let image = {
            let mut entry = archive.by_name("sub/dog.png").unwrap();
            let mut payload = Vec::new();
            let stored: usize = entry.read_to_end(&mut payload).unwrap();
            assert_eq!(stored, PNG_BYTES.len(), "the nested entry read back fully");
            payload
        };
        assert_eq!(image, PNG_BYTES, "nested files keep their path and bytes");
    }

    #[tokio::test]
    async fn unshared_broken_and_unsafe_packs_are_not_archived() {
        let dir = temp_dir();
        save_pack(dir.path(), "private", &pack_with(false, &[("c", "c.png")]))
            .await
            .unwrap();
        write_file(&dir.path().join("private").join("c.png"), PNG_BYTES);
        let private_err = pack_archive(dir.path(), "private").await.unwrap_err();
        assert!(
            matches!(private_err, AppError::Forbidden(_))
                && private_err.to_string() == PACK_NOT_SHARED,
            "share-files false is a 403: {private_err}"
        );

        save_pack(
            dir.path(),
            "missing",
            &pack_with(true, &[("c", "gone.png")]),
        )
        .await
        .unwrap();
        let missing_err = pack_archive(dir.path(), "missing").await.unwrap_err();
        assert!(
            matches!(missing_err, AppError::Forbidden(_)),
            "a listed file off disk is not shareable: {missing_err}"
        );

        write_file(
            &dir.path().join("evil").join(PACK_FILE),
            br#"{"pack": {"share-files": true}, "files": {"x": "../outside.txt"}}"#,
        );
        write_file(&dir.path().join("outside.txt"), b"secret");
        let evil_err = pack_archive(dir.path(), "evil").await.unwrap_err();
        assert!(
            matches!(evil_err, AppError::Forbidden(_)),
            "an escaping entry refuses the pack: {evil_err}"
        );
    }

    #[test]
    fn the_emoji_url_follows_the_route() {
        assert_eq!(
            emoji_image_url(BASE, "cats", "cat.png"),
            "https://test.example/emoji/cats/cat.png",
            "sync stores the address the static route serves"
        );
        assert_eq!(
            emoji_image_url(BASE, "cats", "sub/deep.png"),
            "https://test.example/emoji/cats/sub/deep.png",
            "nested files keep their relative path in the url"
        );
    }

    /// The `(shortcode, id)` pairs of every local row, for id stability.
    async fn local_ids(pool: &PgPool) -> Vec<(String, uuid::Uuid)> {
        custom_emoji::list_local(pool)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.shortcode, row.id))
            .collect()
    }

    /// One synced row as `(shortcode, image_url, first tag, category)`.
    type RowFacts<'a> = (&'a str, &'a str, &'a str, Option<&'a str>);

    #[tokio::test]
    async fn sync_stores_every_pack_file_as_a_local_emoji() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dir = temp_dir();
        seed_pack(dir.path(), "cats", &[("cat", "cat.png")]).await;
        save_pack(
            dir.path(),
            "dogs",
            &pack_with(false, &[("dog", "sub/dog.png")]),
        )
        .await
        .unwrap();
        write_file(&dir.path().join("dogs").join("sub/dog.png"), PNG_BYTES);

        let stored = sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap();
        assert_eq!(stored, 2, "both pack files counted");
        let rows = custom_emoji::list_local(&pool).await.unwrap();
        let got: Vec<RowFacts<'_>> = rows
            .iter()
            .map(|row| {
                (
                    row.shortcode.as_str(),
                    row.image_url.as_str(),
                    row.tags.first().map_or_default(String::as_str),
                    row.category.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    "cat",
                    "https://test.example/emoji/cats/cat.png",
                    "cats",
                    Some("cats")
                ),
                (
                    "dog",
                    "https://test.example/emoji/dogs/sub/dog.png",
                    "dogs",
                    Some("dogs")
                ),
            ],
            "category = pack name, tag = pack name, url = the served address"
        );
    }

    #[tokio::test]
    async fn sync_is_idempotent_and_keeps_row_ids() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dir = temp_dir();
        seed_pack(dir.path(), "cats", &[("cat", "cat.png")]).await;
        let first = sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap();
        let first_ids = local_ids(&pool).await;
        let second = sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap();
        let second_ids = local_ids(&pool).await;
        assert_eq!(first, second, "the same count every run");
        assert_eq!(
            first_ids, second_ids,
            "rows are updated in place (decision 4)"
        );
    }

    #[tokio::test]
    async fn sync_forgets_pack_files_that_are_gone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dir = temp_dir();
        seed_pack(
            dir.path(),
            "cats",
            &[("cat", "cat.png"), ("gone", "gone.png")],
        )
        .await;
        let seeded: usize = sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap();
        assert_eq!(seeded, 2, "both files synced before the pack shrinks");
        let kept_ids = local_ids(&pool).await;
        // An emoji that is not pack-served, and a remote one under the same
        // kind of url, must survive the sweep.
        let _upload: custom_emoji::CustomEmoji = custom_emoji::upsert(
            &pool,
            &NewCustomEmoji::local("upload", "https://cdn.example/custom/upload.png"),
        )
        .await
        .unwrap();
        let _remote: custom_emoji::CustomEmoji = custom_emoji::upsert(
            &pool,
            &NewCustomEmoji {
                domain: Some("remote.example".to_owned()),
                ..NewCustomEmoji::local("remote-cat", "https://test.example/emoji/remote/cat.png")
            },
        )
        .await
        .unwrap();

        save_pack(dir.path(), "cats", &pack_with(true, &[("cat", "cat.png")]))
            .await
            .unwrap();
        let stored = sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap();
        assert_eq!(stored, 1, "only the remaining file counts");
        let rows = custom_emoji::list_local(&pool).await.unwrap();
        let codes: Vec<&str> = rows.iter().map(|row| row.shortcode.as_str()).collect();
        assert_eq!(
            codes,
            ["cat", "upload"],
            "the stale pack emoji is gone; non-prefix and remote rows stay"
        );
        let after_ids = local_ids(&pool).await;
        assert!(
            after_ids.iter().all(|(code, id)| kept_ids
                .iter()
                .find(|(kept_code, _)| kept_code == code)
                .is_none_or(|(_, kept_id)| kept_id == id)),
            "surviving pack emojis keep their ids"
        );
    }

    #[tokio::test]
    async fn sync_of_an_empty_or_missing_root_stores_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dir = temp_dir();
        assert_eq!(
            sync_custom_emojis(&pool, BASE, &dir.path().join("nowhere"))
                .await
                .unwrap(),
            0,
            "a missing root syncs to zero"
        );
        assert_eq!(
            sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap(),
            0,
            "an empty root syncs to zero"
        );
        assert!(
            custom_emoji::list_local(&pool).await.unwrap().is_empty(),
            "and stores no rows"
        );
    }
}
