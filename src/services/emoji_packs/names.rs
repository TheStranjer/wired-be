//! The name boundary of the pack service: the validity predicates for pack
//! directories, file names and shortcodes, the checks the entry points run
//! on client-supplied names, and the shared error and path builders every
//! other child routes the filesystem through.

use std::{
    io,
    path::{Path, PathBuf},
};

use crate::error::{AppError, AppResult};

use super::PACK_NOT_SHARED;

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

/// Whether `code` may be an emoji shortcode.
///
/// A non-empty run of `[A-Za-z0-9_-]`, deliberately stricter than a pack
/// name — no dots, no separators — so a shortcode can neither hide an
/// extension nor name a path. The pattern the file routes pin and
/// [`import_packs`](super::import_packs) applies to the file stems it
/// turns into shortcodes.
#[must_use]
pub fn shortcode_valid(code: &str) -> bool {
    !code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Rejects a pack name that [`pack_name_valid`] would refuse, for the
/// service entry points that take a client-supplied name.
pub(super) fn check_pack_name(name: &str) -> AppResult<()> {
    if pack_name_valid(name) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("invalid pack name: {name}")))
    }
}

/// Rejects a file name that [`file_name_valid`] would refuse, for the
/// service entry points that take a client-supplied file name.
pub(super) fn check_file_name(file: &str) -> AppResult<()> {
    if file_name_valid(file) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "invalid emoji file name: {file}"
        )))
    }
}

/// Wraps a filesystem failure, keeping the operation and path for the log:
/// [`AppError::Internal`] never sends its detail to a client.
pub(super) fn io_error(operation: &str, path: &Path, err: &io::Error) -> AppError {
    AppError::Internal(format!("{operation} {} failed: {err}", path.display()))
}

/// The 403 for anything an archive may not include: a pack that is not
/// shared, and Pleroma's `downloadable?` condition — a pack whose files are
/// all present — that a broken pack fails.
pub(super) fn not_shared() -> AppError {
    AppError::Forbidden(PACK_NOT_SHARED.to_owned())
}

/// The directory of pack `name` under the packs root `dir`.
pub(super) fn pack_dir(dir: &Path, name: &str) -> AppResult<PathBuf> {
    check_pack_name(name)?;
    Ok(dir.join(name))
}

/// The public URL of `file` in pack `name`, as
/// [`sync_custom_emojis`](super::sync_custom_emojis) stores it:
/// `{base_url}/emoji/{name}/{file}`, the address the static emoji route of
/// [`crate::api::emoji_packs`] serves.
#[must_use]
pub fn emoji_image_url(base_url: &str, name: &str, file: &str) -> String {
    format!("{base_url}/emoji/{name}/{file}")
}

#[cfg(test)]
mod tests {
    use crate::services::emoji_packs::{
        Pack, load_pack, read_pack_file, save_pack,
        test_support::{BASE, temp_dir, write_file},
    };

    use super::*;

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

    #[test]
    fn shortcodes_follow_the_pinned_pattern_in_the_service_too() {
        for code in ["cat", "a", "0", "dog_1", "well-known", "MiXeD42"] {
            assert!(shortcode_valid(code), "{code} is a shortcode");
        }
        for code in ["", "cat.png", "two words", "a/b", ".hidden", "é"] {
            assert!(!shortcode_valid(code), "{code} is not a shortcode");
        }
    }
}
