//! The file-level operations on a pack directory: reading a pack file for
//! the static route, writing, renaming and removing the images the
//! manifest names — Pleroma's `save_file`, `rename_file` and `remove_file`
//! pair, subdirectories and the pruning of emptied ones included.

use std::{io, path::Path};

use tracing::debug;

use crate::error::{AppError, AppResult};

use super::names::{check_file_name, check_pack_name, io_error, pack_dir};

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
    check_file_name(file)?;
    let path = pack_dir(dir, name)?.join(file);
    match tokio::fs::read(&path).await {
        Ok(bytes) => Ok(bytes),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(AppError::NotFound),
        Err(err) => Err(io_error("read emoji file", &path, &err)),
    }
}

/// Creates the parent directory of a pack file path (the subdirectory a
/// nested name names, Pleroma's `create_subdirs`); the plain pack
/// directory itself is already there.
pub(super) async fn create_parent(path: &Path) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| io_error("create pack directory", parent, &err))?;
    }
    Ok(())
}

/// Removes the directory a moved-away or deleted pack file leaves behind,
/// when it is now empty — Pleroma's `remove_dir_if_empty`. The pack
/// directory itself is never a candidate, and a directory that still
/// holds files (or vanished meanwhile) simply stays: a refusal to prune
/// is never an error worth reporting.
async fn prune_empty_dir(pack_dir: &Path, former_file: &Path) {
    let Some(parent) = former_file.parent() else {
        return;
    };
    if parent == pack_dir {
        return;
    }
    if let Err(err) = tokio::fs::remove_dir(parent).await {
        debug!(
            dir = %parent.display(),
            %err,
            "kept the pack directory: not empty or already gone"
        );
    }
}

/// Writes `bytes` as `file` inside pack `name`, creating the
/// subdirectories a nested name names.
///
/// This is the file half of Pleroma's `add_file` (`save_file` with its
/// `create_subdirs`); the manifest entry is the caller's to record with
/// [`save_pack`](super::save_pack).
///
/// # Errors
///
/// [`AppError::BadRequest`] if either name is unusable, and
/// [`AppError::Internal`] for any filesystem failure.
pub async fn write_pack_file(dir: &Path, name: &str, file: &str, bytes: &[u8]) -> AppResult<()> {
    check_pack_name(name)?;
    check_file_name(file)?;
    let path = pack_dir(dir, name)?.join(file);
    create_parent(&path).await?;
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|err| io_error("write emoji file", &path, &err))?;
    debug!(
        pack = name,
        file,
        bytes = bytes.len(),
        "wrote emoji pack file"
    );
    Ok(())
}

/// Renames `from` to `to` inside pack `name`, creating the target's
/// subdirectories first — Pleroma's `rename_file`.
///
/// Renaming a file onto itself is how an update that only moves the
/// shortcode says "keep the file", and like Pleroma's `get_filename`
/// `File.stat` check it needs the file to be on disk.
///
/// # Errors
///
/// [`AppError::BadRequest`] if either name is unusable,
/// [`AppError::NotFound`] if `from` is not on disk, and
/// [`AppError::Internal`] for any other filesystem failure.
pub async fn rename_pack_file(dir: &Path, name: &str, from: &str, to: &str) -> AppResult<()> {
    check_pack_name(name)?;
    check_file_name(from)?;
    check_file_name(to)?;
    let pack_root = pack_dir(dir, name)?;
    let source = pack_root.join(from);
    let target = pack_root.join(to);
    create_parent(&target).await?;
    match tokio::fs::rename(&source, &target).await {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(AppError::NotFound),
        Err(err) => return Err(io_error("rename emoji file", &source, &err)),
    }
    if from != to {
        prune_empty_dir(&pack_root, &source).await;
    }
    debug!(pack = name, from, to, "renamed emoji pack file");
    Ok(())
}

/// Removes `file` from pack `name` — the file a deleted manifest entry
/// named, Pleroma's `remove_file`.
///
/// An absent file is success: the entry is what the manifest must lose,
/// and a half-installed pack stays fixable through the API.
///
/// # Errors
///
/// [`AppError::BadRequest`] if either name is unusable, and
/// [`AppError::Internal`] for any other filesystem failure.
pub async fn remove_pack_file(dir: &Path, name: &str, file: &str) -> AppResult<()> {
    check_pack_name(name)?;
    check_file_name(file)?;
    let pack_root = pack_dir(dir, name)?;
    let path = pack_root.join(file);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(io_error("remove emoji file", &path, &err)),
    }
    prune_empty_dir(&pack_root, &path).await;
    debug!(pack = name, file, "removed emoji pack file");
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::services::emoji_packs::Pack;
    use crate::services::emoji_packs::save_pack;
    use crate::services::emoji_packs::test_support::{PNG_BYTES, seed_pack, temp_dir, write_file};

    use super::*;

    #[tokio::test]
    async fn write_pack_file_lands_the_bytes_and_makes_the_directories() {
        let dir = temp_dir();
        save_pack(dir.path(), "cats", &Pack::default())
            .await
            .unwrap();
        write_pack_file(dir.path(), "cats", "sub/deep.png", PNG_BYTES)
            .await
            .unwrap();
        assert_eq!(
            read_pack_file(dir.path(), "cats", "sub/deep.png")
                .await
                .unwrap(),
            PNG_BYTES,
            "a nested name creates its directories"
        );
    }

    #[tokio::test]
    async fn file_writes_reject_unusable_names_before_the_filesystem() {
        let dir = temp_dir();
        write_file(&dir.path().join("outside.txt"), b"secret");
        for (name, file) in [
            ("../escape", "a.png"),
            ("cats", "../outside.txt"),
            ("cats", "/abs.png"),
            ("cats", "a/../../b"),
            ("cats", ".hidden"),
        ] {
            std::fs::create_dir_all(dir.path().join("cats")).unwrap();
            let write_err = write_pack_file(dir.path(), name, file, PNG_BYTES)
                .await
                .unwrap_err();
            assert!(
                matches!(write_err, AppError::BadRequest(_)),
                "write rejects {name:?}/{file:?}: {write_err}"
            );
            let rename_err = rename_pack_file(dir.path(), name, "a.png", file)
                .await
                .unwrap_err();
            assert!(
                matches!(rename_err, AppError::BadRequest(_)),
                "rename rejects {name:?}/{file:?}: {rename_err}"
            );
            let remove_err = remove_pack_file(dir.path(), name, file).await.unwrap_err();
            assert!(
                matches!(remove_err, AppError::BadRequest(_)),
                "remove rejects {name:?}/{file:?}: {remove_err}"
            );
        }
        assert_eq!(
            std::fs::read(dir.path().join("outside.txt")).unwrap(),
            b"secret",
            "no refused name wrote, moved or removed the file outside the pack"
        );
    }

    #[tokio::test]
    async fn rename_pack_file_moves_the_bytes_and_prunes_the_emptied_dir() {
        let dir = temp_dir();
        seed_pack(dir.path(), "cats", &[("kitten", "sub/kitten.png")]).await;
        rename_pack_file(dir.path(), "cats", "sub/kitten.png", "kitty.png")
            .await
            .unwrap();
        assert!(
            !dir.path().join("cats").join("sub").exists(),
            "the emptied subdirectory is pruned"
        );
        assert_eq!(
            read_pack_file(dir.path(), "cats", "kitty.png")
                .await
                .unwrap(),
            PNG_BYTES,
            "the bytes moved with the name"
        );
    }

    #[tokio::test]
    async fn renaming_a_file_onto_itself_needs_the_file_on_disk() {
        let dir = temp_dir();
        save_pack(dir.path(), "cats", &Pack::default())
            .await
            .unwrap();
        // Pleroma's stat check: an entry whose file is gone cannot be
        // renamed, even onto its own name.
        let err = rename_pack_file(dir.path(), "cats", "gone.png", "gone.png")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound), "absent source: {err}");
        write_pack_file(dir.path(), "cats", "cat.png", PNG_BYTES)
            .await
            .unwrap();
        rename_pack_file(dir.path(), "cats", "cat.png", "cat.png")
            .await
            .unwrap();
        assert_eq!(
            read_pack_file(dir.path(), "cats", "cat.png").await.unwrap(),
            PNG_BYTES,
            "the self-rename of a present file is a successful no-op"
        );
    }

    #[tokio::test]
    async fn remove_pack_file_deletes_and_tolerates_absent_files() {
        let dir = temp_dir();
        seed_pack(dir.path(), "cats", &[("kitten", "sub/kitten.png")]).await;
        write_file(&dir.path().join("outside.txt"), b"secret");
        remove_pack_file(dir.path(), "cats", "sub/kitten.png")
            .await
            .unwrap();
        assert!(
            !dir.path().join("cats").join("sub").exists(),
            "the emptied subdirectory is pruned"
        );
        remove_pack_file(dir.path(), "cats", "sub/kitten.png")
            .await
            .unwrap();
        assert!(
            matches!(
                remove_pack_file(dir.path(), "cats", "../outside.txt")
                    .await
                    .unwrap_err(),
                AppError::BadRequest(_)
            ),
            "an escaping name still never reaches the filesystem"
        );
        assert_eq!(
            std::fs::read(dir.path().join("outside.txt")).unwrap(),
            b"secret",
            "and the file outside the pack is untouched"
        );
    }
}
