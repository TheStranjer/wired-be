//! The fixtures shared by the unit tests of several emoji-pack children:
//! a throwaway packs root, manifest and file builders, and the magic-byte
//! fixtures the image rules read. Test-only.

use std::path::Path;

use tempfile::TempDir;

use super::{Pack, PackMeta, save_pack};

/// The instance origin the sync builds URLs from.
pub(super) const BASE: &str = "https://test.example";
/// Bytes for a fake image file.
pub(super) const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfake";
/// Magic bytes the [`infer`] match names as `image/gif`.
pub(super) const GIF_BYTES: &[u8] = b"GIF89a....";
/// Magic bytes the [`infer`] match names as `image/jpeg`.
pub(super) const JPEG_BYTES: &[u8] = b"\xFF\xD8\xFF\xE0rest";

/// A throwaway packs root; removed when the guard drops.
pub(super) fn temp_dir() -> TempDir {
    TempDir::new().unwrap()
}

/// A pack manifest naming `files` (shortcode, file name) pairs, sharing
/// its files when `shared`.
pub(super) fn pack_with(shared: bool, files: &[(&str, &str)]) -> Pack {
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
pub(super) fn write_file(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// Installs pack `name` under `dir`: the shared manifest plus real
/// files for every entry of `files`.
pub(super) async fn seed_pack(dir: &Path, name: &str, files: &[(&str, &str)]) {
    save_pack(dir, name, &pack_with(true, files)).await.unwrap();
    let pack_dir = dir.join(name);
    for (_, file) in files {
        write_file(&pack_dir.join(file), PNG_BYTES);
    }
}
