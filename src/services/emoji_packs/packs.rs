//! The pack-level filesystem operations: reading and writing a `pack.json`,
//! creating and deleting a pack directory, and listing the packs under a
//! root — Pleroma's `Emoji.Pack` `load`/`save`/`create`/`delete`/
//! `list_local` set.

use std::{io, path::Path};

use tracing::debug;

use crate::error::{AppError, AppResult};

use super::{
    NEW_PACK_MANIFEST, PACK_EXISTS, PACK_FILE, Pack,
    names::{io_error, pack_dir},
};

/// Reads pack `name` from the packs root `dir`, parsing its `pack.json`.
///
/// # Errors
///
/// [`AppError::BadRequest`] if `name` is not [`pack_name_valid`](super::pack_name_valid),
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
/// [`AppError::BadRequest`] if `name` is not [`pack_name_valid`](super::pack_name_valid) and
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
/// [`AppError::BadRequest`] if `name` is not [`pack_name_valid`](super::pack_name_valid) or the
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
/// [`AppError::BadRequest`] if `name` is not [`pack_name_valid`](super::pack_name_valid),
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::services::emoji_packs::PackMeta;
    use crate::services::emoji_packs::test_support::{
        PNG_BYTES, pack_with, seed_pack, temp_dir, write_file,
    };

    use super::*;

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
                download_sha256: None,
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
}
