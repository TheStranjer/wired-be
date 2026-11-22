//! The on-disk half of a frontend install: reading the zip archive under
//! the safety gates, staging its entries and landing the chosen subtree.

use std::{
    io::{Cursor, Read as _},
    path::{Path, PathBuf},
};

use bytes::Bytes;
use uuid::Uuid;

use super::{DEFAULT_BUILD_DIR, INSTALL_FAILURE, MAX_ZIP_ENTRIES};
use crate::error::{AppError, AppResult};

/// The on-disk half of an install, run on the blocking pool like the
/// media analysis: extract the archive into a staging directory inside
/// `frontends_dir` (same filesystem, so landing it is a rename), then
/// move the `build_dir` subtree — or the whole archive when the request
/// and config name none or the archive carries none — into
/// `destination`, replacing any earlier install.
///
/// # Errors
///
/// As [`install_frontend`]'s install refusals, for the extraction and the
/// filesystem; [`AppError::Internal`] only when the blocking task itself
/// is gone.
pub(super) async fn land_archive(
    frontends_dir: &Path,
    destination: PathBuf,
    build_dir: Option<&str>,
    archive: Bytes,
) -> AppResult<()> {
    let frontends = frontends_dir.to_path_buf();
    let build_dir = build_dir.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        let staging = frontends.join(format!(".tmp-install-{}", Uuid::now_v7()));
        install_staged(&staging, &destination, build_dir.as_deref(), &archive)
    })
    .await
    .map_err(|err| AppError::Internal(format!("frontend install task did not finish: {err}")))?
}

/// Extract `archive` under `staging` and land the chosen subtree at
/// `destination`; the staging directory goes either way.
fn install_staged(
    staging: &Path,
    destination: &Path,
    build_dir: Option<&str>,
    archive: &[u8],
) -> AppResult<()> {
    let entries = read_zip(archive)?;
    std::fs::create_dir_all(staging).map_err(|err| io_failure(&err))?;
    let staged = stage_entries(staging, &entries);
    let outcome = staged.and_then(|()| {
        let source = installable_source(staging, build_dir);
        land_directory(&source, destination)
    });
    let _cleanup = std::fs::remove_dir_all(staging);
    outcome.map_err(|err| io_failure(&err))
}

/// The staged entries of one archive: validated relative path to bytes.
type ZipEntries = Vec<(String, Vec<u8>)>;

/// Read an archive's file entries with the ticket's safety gates up
/// front: the entry-count bound before any read, every name through
/// [`safe_entry_name`]. Directory entries carry nothing to land and are
/// dropped — the file writes create their own parents.
///
/// # Errors
///
/// [`AppError::BadRequest`] with the install refusal for an unreadable
/// archive, the over-full entry count and an unsafe name.
fn read_zip(archive: &[u8]) -> AppResult<ZipEntries> {
    let mut reader = zip::ZipArchive::new(Cursor::new(archive))
        .map_err(|err| install_failure(&format!("the archive could not be read: {err}")))?;
    if reader.len() > MAX_ZIP_ENTRIES {
        return Err(install_failure(&format!(
            "the archive carries {} entries, at most {MAX_ZIP_ENTRIES} are allowed",
            reader.len()
        )));
    }
    let mut entries: ZipEntries = Vec::new();
    for index in 0..reader.len() {
        let mut file = reader.by_index(index).map_err(|err| {
            install_failure(&format!("an archive entry could not be read: {err}"))
        })?;
        let name = file.name().map_err(|err| {
            install_failure(&format!("an archive entry name is undecodable: {err}"))
        })?;
        let name = safe_entry_name(&name)?;
        if file.is_dir() {
            continue;
        }
        let mut bytes: Vec<u8> = Vec::new();
        let _read = file.read_to_end(&mut bytes).map_err(|err| {
            install_failure(&format!("an archive entry could not be read: {err}"))
        })?;
        entries.push((name, bytes));
    }
    Ok(entries)
}

/// Writes the validated `entries` under `staging`, creating parent
/// directories as the names ask for.
///
/// # Errors
///
/// Whatever the filesystem says.
fn stage_entries(staging: &Path, entries: &[(String, Vec<u8>)]) -> std::io::Result<()> {
    for (name, bytes) in entries {
        let path = staging.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)?;
    }
    Ok(())
}

/// The staged subtree to install: `build_dir` when the archive carried
/// one (Pleroma's `build_dir || "dist"` — and a plain "dist" is tried
/// even when the request named nothing, so the stock build layout
/// installs its `dist`); otherwise the archive root.
fn installable_source(staging: &Path, build_dir: Option<&str>) -> PathBuf {
    let wanted = build_dir.unwrap_or(DEFAULT_BUILD_DIR);
    let subtree = staging.join(wanted);
    if subtree.is_dir() {
        subtree
    } else {
        staging.to_path_buf()
    }
}

/// Lands `source` at `destination`: an earlier install goes, the parent
/// chain appears, and the staged tree renames over — a same-filesystem
/// move, staging being inside `FRONTENDS_DIR` itself.
fn land_directory(source: &Path, destination: &Path) -> std::io::Result<()> {
    if destination.exists() {
        std::fs::remove_dir_all(destination)?;
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(source, destination)
}

/// The ticket's archive-entry gate: a relative `/`-separated path whose
/// components are none of them `..` or `.`, empty, or a leading slash —
/// backslashes refuse outright, since on the wire they belong to the
/// Windows spellings this endpoint never installs from.
fn safe_entry_name(name: &str) -> AppResult<String> {
    let backslashed = name.chars().any(|c| c == '\\');
    let clean = !name.is_empty()
        && !backslashed
        && !name.starts_with('/')
        && name
            .split('/')
            .all(|c| !c.is_empty() && c != ".." && c != ".");
    if clean {
        return Ok(name.to_owned());
    }
    Err(install_failure(&format!(
        "the archive entry {name:?} is not a safe relative path"
    )))
}

/// Whether `c` may stand in a single install path segment: the
/// `uploads::check_key` charset minus the separator.
const fn is_segment_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// A single safe path segment (a frontend `name` or `ref`): the
/// [`is_segment_char`] charset, non-empty, no bare `.` or `..`.
///
/// # Errors
///
/// [`AppError::BadRequest`] with the install refusal.
pub(super) fn check_segment(text: &str, what: &str) -> AppResult<()> {
    let safe = !text.is_empty() && text != "." && text != ".." && text.chars().all(is_segment_char);
    if safe {
        return Ok(());
    }
    Err(install_failure(&format!(
        "the {what} {text:?} is not a safe name"
    )))
}

/// A safe relative directory path (a `build_dir`): `/`-separated
/// [`is_segment_char`] segments, no empty ones, no `.` or `..`.
///
/// # Errors
///
/// [`AppError::BadRequest`] with the install refusal.
pub(super) fn check_relative_path(text: &str, what: &str) -> AppResult<()> {
    let safe = !text.is_empty()
        && !text.starts_with('/')
        && text
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && c.chars().all(is_segment_char));
    if safe {
        return Ok(());
    }
    Err(install_failure(&format!(
        "the {what} {text:?} is not a safe relative path"
    )))
}

/// The ticket's install refusal: 400 `{"error":"Could not install
/// frontend: <detail>"}`.
pub(super) fn install_failure(detail: &str) -> AppError {
    AppError::BadRequest(format!("{INSTALL_FAILURE}{detail}"))
}

/// A filesystem failure at install time, in the same refusal shape: this
/// is an operator-facing endpoint, and the operator is the one who can
/// fix a `Permission denied`.
fn io_failure(err: &std::io::Error) -> AppError {
    install_failure(&format!("the filesystem said: {err}"))
}

#[cfg(test)]
mod tests {
    use super::super::{
        frontends::{InstallParams, resolve_install},
        test_support::is_install_failure,
    };
    use super::*;
    use crate::config::Config;

    #[test]
    fn unsafe_names_refs_and_build_dirs_are_refused() {
        let config = Config::for_tests();
        let base = InstallParams {
            name: Some("../../etc".to_owned()),
            reference: None,
            build_url: Some("https://build.example/x.zip".to_owned()),
            build_dir: None,
        };
        let name_refusal = resolve_install(&config, &base, false).expect_err("a traversal name");
        assert!(
            is_install_failure(&name_refusal, "the frontend name"),
            "{name_refusal:?}"
        );

        let slashy = InstallParams {
            name: Some("fe".to_owned()),
            reference: Some("a/b".to_owned()),
            build_url: Some("https://build.example/x.zip".to_owned()),
            build_dir: None,
        };
        let ref_refusal = resolve_install(&config, &slashy, false).expect_err("a slashed ref");
        assert!(
            is_install_failure(&ref_refusal, "the frontend ref"),
            "{ref_refusal:?}"
        );

        let escaping = InstallParams {
            name: Some("fe".to_owned()),
            reference: None,
            build_url: Some("https://build.example/x.zip".to_owned()),
            build_dir: Some("../dist".to_owned()),
        };
        let dir_refusal =
            resolve_install(&config, &escaping, false).expect_err("a traversing build dir");
        assert!(
            is_install_failure(&dir_refusal, "the build directory"),
            "{dir_refusal:?}"
        );
    }

    #[test]
    fn archive_entry_names_walk_the_ticket_gate() {
        assert_eq!(
            safe_entry_name("dist/index.html").expect("a plain path stands"),
            "dist/index.html"
        );
        assert!(
            safe_entry_name(".hidden").is_ok(),
            "a dotfile is no traversal"
        );
        for bad in [
            "",
            "/",
            "/absolute",
            "../escape",
            "a/../../b",
            "a//b",
            "./x",
            "C:\\evil",
        ] {
            let err = safe_entry_name(bad).expect_err(&format!("{bad:?} must refuse"));
            assert!(
                is_install_failure(&err, "the archive entry"),
                "the pinned refusal shape for {bad:?}"
            );
        }
    }

    /// A staging tree with `dist/` and root files, for the source choice.
    fn staged_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a staging dir");
        std::fs::create_dir_all(dir.path().join("dist")).expect("the dist dir");
        std::fs::write(dir.path().join("dist/index.html"), b"d").expect("the dist file");
        std::fs::write(dir.path().join("index.html"), b"r").expect("the root file");
        dir
    }

    #[test]
    fn the_installable_source_prefers_the_named_dist() {
        let staging = staged_tree();
        assert_eq!(
            installable_source(staging.path(), Some("dist")),
            staging.path().join("dist"),
            "the named subtree, when it is there"
        );
        assert_eq!(
            installable_source(staging.path(), None),
            staging.path().join("dist"),
            "and the default dist, when the archive carries one"
        );
        assert_eq!(
            installable_source(staging.path(), Some("webout")),
            staging.path().to_path_buf(),
            "a named subtree the archive lacks falls back to the root"
        );
    }

    #[test]
    fn landing_replaces_the_earlier_install() {
        let root = tempfile::tempdir().expect("a frontends dir");
        let source = root.path().join("staged");
        let destination = root.path().join("fe").join("v1");
        std::fs::create_dir_all(&source).expect("the source dir");
        std::fs::write(source.join("new.js"), b"new").expect("the new file");
        std::fs::create_dir_all(&destination).expect("the old destination");
        std::fs::write(destination.join("old.js"), b"old").expect("the old file");

        land_directory(&source, &destination).expect("the landing runs");

        assert!(!source.exists(), "the staging tree renamed away");
        assert!(
            !destination.join("old.js").exists(),
            "the old install is gone"
        );
        assert!(
            destination.join("new.js").is_file(),
            "the new tree stands in its place"
        );
    }

    /// A minimal valid archive, and one unsafe entry, for the reader.
    fn archive_with(name: &str, bytes: &[u8]) -> Vec<u8> {
        use std::io::Write as _;

        use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file(name, options).expect("entry starts");
        writer.write_all(bytes).expect("bytes write");
        let cursor = writer.finish().expect("the fixture archive closes");
        cursor.into_inner()
    }

    #[test]
    fn the_archive_reader_gates_names_and_returns_files() {
        let bytes = archive_with("dist/index.html", b"hello");
        let entries = read_zip(&bytes).expect("the fixture archive reads");
        assert_eq!(entries.len(), 1, "the one file entry");
        assert_eq!(entries[0].0, "dist/index.html", "under its own name");
        assert_eq!(entries[0].1, b"hello", "with its bytes");

        let slip_err = read_zip(&archive_with("../evil.txt", b"x")).expect_err("a zip-slip entry");
        assert!(
            is_install_failure(&slip_err, "the archive entry"),
            "{slip_err:?}"
        );
        let absolute_err =
            read_zip(&archive_with("/etc/passwd", b"x")).expect_err("an absolute entry");
        assert!(
            is_install_failure(&absolute_err, "the archive entry"),
            "{absolute_err:?}"
        );
    }

    #[test]
    fn an_unreadable_archive_is_the_install_refusal() {
        let err = read_zip(b"not a zip at all").expect_err("junk cannot open");
        assert!(is_install_failure(&err, "the archive"), "{err:?}");
    }
}
