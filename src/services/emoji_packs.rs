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
//!
//! The install side of the admin API lives here too:
//! [`extract_pack_archive`] unpacks a pack zip (remote download or upload)
//! into a fresh pack directory under the same name rules — traversal is
//! refused, only images and the manifest are written — and
//! [`import_packs`] writes `pack.json` for pack directories that lack a
//! valid one, from the image files they hold.

mod archive;
mod files;
mod import;
mod names;
mod packs;
#[cfg(test)]
mod test_support;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use self::{
    archive::{extract_pack_archive, pack_archive},
    files::{read_pack_file, remove_pack_file, rename_pack_file, write_pack_file},
    import::{import_packs, sync_custom_emojis},
    names::{emoji_image_url, file_name_valid, pack_name_valid, shortcode_valid},
    packs::{create_pack, delete_pack, list_packs, load_pack, save_pack},
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
/// Largest pack archive [`extract_pack_archive`] installs, fetched or
/// uploaded.
///
/// The ticket's 20 MiB. One unpacked entry is capped at the same size;
/// [`MAX_PACK_EXTRACTED_BYTES`] bounds all entries together.
pub const MAX_PACK_ARCHIVE_BYTES: usize = 20 * 1024 * 1024;
/// Most bytes all entries of one pack archive may inflate to together.
/// [`extract_pack_archive`] refuses the archive with
/// [`ARCHIVE_TOO_LARGE`] before writing past it.
pub const MAX_PACK_EXTRACTED_BYTES: usize = 100 * 1024 * 1024;
/// Largest number of entries a pack archive may hold (the ticket's
/// 2000); one more refuses the whole archive.
pub const MAX_PACK_ARCHIVE_ENTRIES: usize = 2000;
/// 400 body for an archive entry that climbs out of the pack directory
/// (absolute paths and `..` segments, what Pleroma's `SafeZip` refuses
/// through `Path.safe_relative`).
pub const ARCHIVE_UNSAFE_PATH: &str = "Pack archive contains an unsafe path";
/// 400 body for an archive over [`MAX_PACK_ARCHIVE_ENTRIES`] entries.
pub const ARCHIVE_TOO_MANY_FILES: &str = "Pack archive contains too many files";
/// 400 body for bytes that are no readable zip archive — or an entry
/// whose stored bytes corrupt the read.
pub const ARCHIVE_UNREADABLE: &str = "Could not read the pack archive";
/// 400 body for one entry whose unpacked bytes pass
/// [`MAX_PACK_ARCHIVE_BYTES`] — the archive bomb this cap defuses.
pub const ARCHIVE_ENTRY_TOO_LARGE: &str = "Pack archive contains an oversized file";
/// 400 body for an archive whose entries together unpack past
/// [`MAX_PACK_EXTRACTED_BYTES`].
pub const ARCHIVE_TOO_LARGE: &str = "Pack archive unpacks to too much data";
/// The image media types an emoji file may be, read from the bytes
/// themselves.
///
/// The magic-byte allowlist of [`crate::api::emoji_packs`] and
/// [`extract_pack_archive`]. SVG is deliberately absent: it has no magic
/// bytes to read, and the static pack route refuses to serve it, so
/// accepting it could only store a file that never renders.
pub const ACCEPTED_EMOJI_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
/// The file extensions [`import_packs`] treats as emoji images — the
/// loose-file counterpart of [`ACCEPTED_EMOJI_TYPES`], Pleroma's
/// `[:emoji, :pack_extensions]` widened to the formats this instance
/// serves.
pub const PACK_IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

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
    /// SHA-256 of the archive this instance serves, hex: the digest a
    /// listing announces for a shared pack. Read from remote listings to
    /// verify a download; never stored in an installed manifest.
    #[serde(
        default,
        rename = "download-sha256",
        skip_serializing_if = "Option::is_none"
    )]
    pub download_sha256: Option<String>,
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
