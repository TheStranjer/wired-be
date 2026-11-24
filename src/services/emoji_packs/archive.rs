//! The zip side of the pack service: [`pack_archive`] builds the download
//! of a shared pack, and [`extract_pack_archive`] installs one, refusing
//! traversal, oversized entry counts and archive bombs the way Pleroma's
//! `SafeZip` does.

use std::{
    collections::BTreeSet,
    io::{self, Cursor, Write as _},
    path::Path,
};

use tracing::debug;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::error::{AppError, AppResult, io_error};

use super::{
    ACCEPTED_EMOJI_TYPES, ARCHIVE_ENTRY_TOO_LARGE, ARCHIVE_TOO_LARGE, ARCHIVE_TOO_MANY_FILES,
    ARCHIVE_UNREADABLE, ARCHIVE_UNSAFE_PATH, MAX_PACK_ARCHIVE_BYTES, MAX_PACK_ARCHIVE_ENTRIES,
    MAX_PACK_EXTRACTED_BYTES, PACK_FILE, file_name_valid,
    files::create_parent,
    names::{not_shared, pack_dir},
    packs::load_pack,
};

/// The whole pack as a zip archive, ready for the download endpoint.
///
/// The archive holds `pack.json` first, then every file listed in it under
/// its relative name — Pleroma's `["pack.json" | pack_file_list]` in
/// `create_archive_and_cache`.
///
/// # Errors
///
/// Whatever [`load_pack`] answers, [`AppError::Forbidden`]
/// ([`PACK_NOT_SHARED`](super::PACK_NOT_SHARED)) if the pack does not share
/// its files, if one of its file names is unusable, or if a listed file is
/// missing from disk — Pleroma's `downloadable?` condition;
/// [`AppError::Internal`] for any other read or archive failure.
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

/// The 400 a refused pack archive is answered with.
fn archive_refused(message: &str) -> AppError {
    AppError::BadRequest(message.to_owned())
}

/// Whether `file` names a zip entry that would escape the pack
/// directory: an absolute path or a `..` segment. These refuse the
/// whole archive (the ticket's reject-traversal rule); other unusable
/// names are merely skipped, since they can neither traverse nor hide
/// once [`file_name_valid`] has had them.
fn path_traversal(file: &str) -> bool {
    file.starts_with('/') || file.split('/').any(|segment| segment == "..")
}

/// Whether `bytes` are an accepted emoji image, read from the magic
/// bytes like the file routes do — never from the name or a
/// `content-type` the archive's author claimed.
fn is_emoji_image(bytes: &[u8]) -> bool {
    infer::get(bytes).is_some_and(|kind| ACCEPTED_EMOJI_TYPES.contains(&kind.mime_type()))
}

/// Installs a pack archive the way Pleroma's `SafeZip.unzip_data` must:
/// the zip `bytes` are unpacked into the pack directory `name` under
/// the packs root `dir`, entry by entry and safely.
///
/// An entry name that traverses out of the pack (absolute, or with a
/// `..` segment) refuses the whole archive with [`ARCHIVE_UNSAFE_PATH`],
/// as do more than [`MAX_PACK_ARCHIVE_ENTRIES`] entries
/// ([`ARCHIVE_TOO_MANY_FILES`]) and one entry inflating past
/// [`MAX_PACK_ARCHIVE_BYTES`] ([`ARCHIVE_ENTRY_TOO_LARGE`]) or all entries
/// together inflating past [`MAX_PACK_EXTRACTED_BYTES`]
/// ([`ARCHIVE_TOO_LARGE`]); bytes that
/// are no readable zip fail with [`ARCHIVE_UNREADABLE`]. Beyond those
/// rules only [`PACK_FILE`] and files whose magic bytes name an accepted
/// image type are written — every other entry is skipped without
/// touching the disk. Directory entries name nothing to write.
///
/// Returns the relative names of the entries it wrote.
///
/// # Errors
///
/// [`AppError::BadRequest`] per the rules above and for a pack name
/// [`pack_name_valid`](super::pack_name_valid) rejects;
/// [`AppError::Internal`] for a filesystem failure.
pub async fn extract_pack_archive(
    dir: &Path,
    name: &str,
    bytes: &[u8],
) -> AppResult<BTreeSet<String>> {
    let caps = SizeCaps {
        entry: MAX_PACK_ARCHIVE_BYTES,
        total: MAX_PACK_EXTRACTED_BYTES,
    };
    extract_with_caps(dir, name, bytes, caps).await
}

/// The inflated-size limits an extraction enforces.
#[derive(Clone, Copy)]
struct SizeCaps {
    /// Most bytes one entry may inflate to.
    entry: usize,
    /// Most bytes all entries may inflate to together.
    total: usize,
}

/// [`extract_pack_archive`] with explicit `caps`, so tests can use small
/// budgets.
async fn extract_with_caps(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    caps: SizeCaps,
) -> AppResult<BTreeSet<String>> {
    let pack_root = pack_dir(dir, name)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|_err| archive_refused(ARCHIVE_UNREADABLE))?;
    if archive.len() > MAX_PACK_ARCHIVE_ENTRIES {
        return Err(archive_refused(ARCHIVE_TOO_MANY_FILES));
    }
    let mut extracted = BTreeSet::new();
    let mut total = 0_usize;
    for index in 0..archive.len() {
        if let Some(file) =
            extract_archive_entry(&mut archive, &pack_root, index, caps, &mut total).await?
        {
            let _fresh: bool = extracted.insert(file);
        }
    }
    debug!(
        pack = name,
        files = extracted.len(),
        "extracted emoji pack archive"
    );
    Ok(extracted)
}

/// Unpacks entry `index` of `archive` into `pack_root`. `Ok(None)` is
/// an entry there is nothing to write: a directory, a name
/// [`file_name_valid`] refuses, or bytes no accepted image type names.
///
/// `total` is the running count of bytes inflated so far; the entry's
/// bytes are added to it, and the entry is refused if that passes
/// `caps.total`.
async fn extract_archive_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    pack_root: &Path,
    index: usize,
    caps: SizeCaps,
    total: &mut usize,
) -> AppResult<Option<String>> {
    let entry = archive
        .by_index(index)
        .map_err(|_err| archive_refused(ARCHIVE_UNREADABLE))?;
    if entry.is_dir() {
        return Ok(None);
    }
    let file = entry
        .name()
        .map_err(|_err| archive_refused(ARCHIVE_UNREADABLE))?
        .into_owned();
    if path_traversal(&file) {
        return Err(archive_refused(ARCHIVE_UNSAFE_PATH));
    }
    if !file_name_valid(&file) {
        return Ok(None);
    }
    let bytes = read_archive_entry_bytes(entry, caps, *total)?;
    *total = total.saturating_add(bytes.len());
    if file != PACK_FILE && !is_emoji_image(&bytes) {
        return Ok(None);
    }
    let path = pack_root.join(&file);
    create_parent(&path).await?;
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(|err| io_error("write emoji pack file", &path, &err))?;
    Ok(Some(file))
}

/// The unpacked bytes of one entry, reading at most one byte past the
/// smaller of `caps.entry` and the budget left of `caps.total` after
/// `used` bytes, so an oversized entry fails while inflating without
/// being given the whole budget. An entry past `caps.entry` is
/// [`ARCHIVE_ENTRY_TOO_LARGE`]; one that fits the entry cap but not the
/// remaining budget is [`ARCHIVE_TOO_LARGE`]. A read that breaks midway
/// (a corrupt entry) is the unreadable archive it came from.
fn read_archive_entry_bytes<R: io::Read>(
    entry: zip::read::ZipFile<'_, R>,
    caps: SizeCaps,
    used: usize,
) -> AppResult<Vec<u8>> {
    let remaining = caps.total.saturating_sub(used);
    let allowed = caps.entry.min(remaining);
    let limit = u64::try_from(allowed).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    let mut capped = io::Read::take(entry, limit.saturating_add(1));
    let _read: usize = io::Read::read_to_end(&mut capped, &mut bytes)
        .map_err(|_err| archive_refused(ARCHIVE_UNREADABLE))?;
    if bytes.len() > caps.entry {
        return Err(archive_refused(ARCHIVE_ENTRY_TOO_LARGE));
    }
    if bytes.len() > remaining {
        return Err(archive_refused(ARCHIVE_TOO_LARGE));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use zip::ZipArchive;

    use crate::services::emoji_packs::PACK_NOT_SHARED;
    use crate::services::emoji_packs::save_pack;
    use crate::services::emoji_packs::test_support::{
        GIF_BYTES, PNG_BYTES, pack_with, seed_pack, temp_dir, write_file,
    };

    use super::*;

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

    /// The bytes of a zip holding `entries` (name, payload) as written.
    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, payload) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(payload).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    /// A manifest naming the two images of the fixture archives.
    const ARCHIVE_MANIFEST: &[u8] =
        br#"{"pack": {"share-files": true}, "files": {"cat": "cat.png"}}"#;

    /// The 400 a refused archive is answered with, checking the pinned
    /// wording.
    fn archive_error(err: &AppError, message: &str) -> bool {
        matches!(err, AppError::BadRequest(_)) && err.to_string() == message
    }

    #[tokio::test]
    async fn extraction_lands_images_and_the_manifest_and_skips_the_rest() {
        let dir = temp_dir();
        let archive = zip_of(&[
            (PACK_FILE, ARCHIVE_MANIFEST),
            ("cat.png", PNG_BYTES),
            ("sub/kitten.gif", GIF_BYTES),
            ("notes.txt", b"not an image".as_slice()),
            (".hidden.png", PNG_BYTES),
            ("two words.png", PNG_BYTES),
            ("sub/", b"".as_slice()),
        ]);
        let extracted = extract_pack_archive(dir.path(), "fresh", &archive)
            .await
            .unwrap();
        let names: Vec<&str> = extracted.iter().map(String::as_str).collect();
        assert_eq!(
            names,
            ["cat.png", "pack.json", "sub/kitten.gif"],
            "only the manifest and the images are written, nested paths included"
        );
        assert_eq!(
            std::fs::read(dir.path().join("fresh").join("sub/kitten.gif")).unwrap(),
            GIF_BYTES,
            "the nested bytes landed"
        );
        assert!(
            !dir.path().join("fresh/notes.txt").exists()
                && !dir.path().join("fresh/.hidden.png").exists()
                && !dir.path().join("fresh/two words.png").exists(),
            "the skipped entries wrote nothing"
        );
    }

    #[tokio::test]
    async fn extraction_refuses_traversal_and_keeps_the_outside_untouched() {
        let dir = temp_dir();
        write_file(&dir.path().join("outside.txt"), b"secret");
        for bad in ["../../escape.png", "/abs.png", "ok/../../../up.png"] {
            let archive = zip_of(&[("cat.png", PNG_BYTES), (bad, PNG_BYTES)]);
            let err = extract_pack_archive(dir.path(), "fresh", &archive)
                .await
                .unwrap_err();
            assert!(
                archive_error(&err, ARCHIVE_UNSAFE_PATH),
                "{bad} refuses the archive: {err}"
            );
        }
        assert!(
            !dir.path().join("escape.png").exists()
                && !dir.path().join("abs.png").exists()
                && !dir.path().join("up.png").exists(),
            "nothing escaped the pack directory"
        );
        assert_eq!(
            std::fs::read(dir.path().join("outside.txt")).unwrap(),
            b"secret",
            "and the file outside is untouched"
        );
    }

    #[tokio::test]
    async fn extraction_caps_the_entry_count_and_one_entries_size() {
        let dir = temp_dir();
        let many: Vec<(String, &[u8])> = (0..=MAX_PACK_ARCHIVE_ENTRIES)
            .map(|index| (format!("e{index}.png"), PNG_BYTES))
            .collect();
        let many_refs: Vec<(&str, &[u8])> = many
            .iter()
            .map(|(name, bytes)| (name.as_str(), *bytes))
            .collect();
        let err = extract_pack_archive(dir.path(), "many", &zip_of(&many_refs))
            .await
            .unwrap_err();
        assert!(
            archive_error(&err, ARCHIVE_TOO_MANY_FILES),
            "2001 entries refuse the archive: {err}"
        );

        // One deflate-compressed entry of one byte past the cap: the
        // archive itself is tiny, only the unpacked bytes are big.
        let bomb = vec![0_u8; MAX_PACK_ARCHIVE_BYTES + 1];
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(
                "bomb.png",
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .unwrap();
        writer.write_all(&bomb).unwrap();
        let archive = writer.finish().unwrap().into_inner();
        assert!(
            archive.len() < 1024 * 1024,
            "the bomb is small compressed: {} bytes",
            archive.len()
        );
        let bomb_err = extract_pack_archive(dir.path(), "bomb", &archive)
            .await
            .unwrap_err();
        assert!(
            archive_error(&bomb_err, ARCHIVE_ENTRY_TOO_LARGE),
            "a 20 MiB plus one entry refuses: {bomb_err}"
        );
        assert!(
            !dir.path().join("many").exists() && !dir.path().join("bomb").exists(),
            "the refused archives installed nothing"
        );
    }

    /// A PNG of exactly `len` bytes: the real header, then zeros.
    fn padded_png(len: usize) -> Vec<u8> {
        let mut bytes = PNG_BYTES.to_vec();
        bytes.resize(len, 0);
        bytes
    }

    /// A deflate-compressed zip of `entries` (name, payload).
    fn deflated_zip_of(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, payload) in entries {
            zip_write(&mut writer, name, options, payload).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[tokio::test]
    async fn extraction_caps_the_total_inflated_size() {
        let dir = temp_dir();
        let caps = SizeCaps {
            entry: 500,
            total: 1000,
        };
        let image = padded_png(400);

        // Each entry is under the per-entry cap, together they pass 1000.
        let over = deflated_zip_of(&[
            ("a.png", image.clone()),
            ("b.png", image.clone()),
            ("c.png", image.clone()),
        ]);
        let err = extract_with_caps(dir.path(), "over", &over, caps)
            .await
            .unwrap_err();
        assert!(
            archive_error(&err, ARCHIVE_TOO_LARGE),
            "entries that sum past the total are refused: {err}"
        );
        assert!(
            !dir.path().join("over").join("c.png").exists(),
            "the entry that broke the budget was never written"
        );

        // Exactly the budget still extracts.
        let exact = deflated_zip_of(&[
            ("a.png", image.clone()),
            ("b.png", image.clone()),
            ("c.png", padded_png(200)),
        ]);
        let extracted = extract_with_caps(dir.path(), "exact", &exact, caps)
            .await
            .unwrap();
        assert_eq!(extracted.len(), 3, "an archive at the total cap installs");

        // One byte over the budget is refused.
        let one_over = deflated_zip_of(&[
            ("a.png", image.clone()),
            ("b.png", image),
            ("c.png", padded_png(201)),
        ]);
        let one_err = extract_with_caps(dir.path(), "one_over", &one_over, caps)
            .await
            .unwrap_err();
        assert!(
            archive_error(&one_err, ARCHIVE_TOO_LARGE),
            "one byte past the total is refused: {one_err}"
        );

        // A single entry past the per-entry cap keeps its own answer.
        let single = deflated_zip_of(&[("big.png", padded_png(501))]);
        let single_err = extract_with_caps(dir.path(), "single", &single, caps)
            .await
            .unwrap_err();
        assert!(
            archive_error(&single_err, ARCHIVE_ENTRY_TOO_LARGE),
            "an oversized entry is still the entry error: {single_err}"
        );
    }

    #[tokio::test]
    async fn skipped_entries_count_against_the_total_too() {
        let dir = temp_dir();
        let caps = SizeCaps {
            entry: 800,
            total: 1000,
        };
        let filler = vec![b'x'; 600];
        let archive = deflated_zip_of(&[("a.txt", filler.clone()), ("b.txt", filler)]);
        let err = extract_with_caps(dir.path(), "junk", &archive, caps)
            .await
            .unwrap_err();
        assert!(
            archive_error(&err, ARCHIVE_TOO_LARGE),
            "inflated bytes count whether or not they are written: {err}"
        );
    }

    #[tokio::test]
    async fn extraction_refuses_non_archives_and_unusable_pack_names() {
        let dir = temp_dir();
        let junk_err = extract_pack_archive(dir.path(), "fresh", b"PK not really a zip")
            .await
            .unwrap_err();
        assert!(
            archive_error(&junk_err, ARCHIVE_UNREADABLE),
            "bytes no zip parser reads are refused: {junk_err}"
        );
        let archive = zip_of(&[("cat.png", PNG_BYTES)]);
        for name in ["../escape", ".hidden", ""] {
            let name_err = extract_pack_archive(dir.path(), name, &archive)
                .await
                .unwrap_err();
            assert!(
                matches!(name_err, AppError::BadRequest(_)),
                "no unusable pack name reaches the filesystem: {name_err}"
            );
        }
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort_unstable();
        assert!(
            left.is_empty(),
            "the refused archives created nothing: {left:?}"
        );
    }
}
