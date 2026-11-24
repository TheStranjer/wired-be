//! The instance document routes: upload, read, delete and the on-disk
//! store.

use std::path::{Path, PathBuf};

use axum::{
    Json,
    extract::{Path as AxumPath, State},
    http::header::CONTENT_TYPE,
    response::{IntoResponse as _, Response},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use tracing::debug;

use super::{AREA_DOCUMENTS, CLASS_READ, CLASS_WRITE};
use crate::{
    auth::admin::AdminUser,
    config::Config,
    entities::admin::{InstanceDocumentAck, InstanceDocumentUrl},
    error::{AppError, AppResult},
    extract::multipart::{MultipartForm, UploadedFiles as _},
    state::AppState,
    util::id::new_id,
};

/// 400 body of a document upload whose body carries no `file` part, in
/// the `Missing parameter: <key>` spelling the admin API shares.
const MISSING_FILE: &str = "Missing parameter: file";
/// The size ceiling of one instance document, the ticket's 1 MiB.
const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
/// 413 body of an oversize document upload, naming the ceiling.
const DOCUMENT_TOO_LARGE: &str = "File is too large (maximum is 1048576 bytes)";
/// `content-type` of the admin document read, as Pleroma's `show` sets
/// it.
const HTML_TYPE: &str = "text/html";
/// The name of the one document the public JSON API reads: the terms of
/// service, served by `GET /api/v1/instance/terms_of_service`.
pub const TERMS_OF_SERVICE: &str = "terms-of-service";
/// The multipart field the document upload reads its file from.
const FILE_FIELD: &str = "file";
/// The extension every instance document is stored and served under.
const DOCUMENT_EXTENSION: &str = "html";
/// `GET /api/v1/pleroma/admin/instance_document/{name}` — Pleroma's
/// `show`: the stored bytes as `text/html`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::RouteNotFound`] for a name off
/// the configured list or a document never uploaded;
/// [`AppError::Internal`] for a store that refuses to be read.
pub(super) async fn show_document(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(name): AxumPath<String>,
) -> AppResult<Response> {
    admin.require(CLASS_READ, AREA_DOCUMENTS)?;
    let bytes = read_document(&state.config, &name).await?;
    Ok(([(CONTENT_TYPE, HTML_TYPE)], bytes).into_response())
}

/// `PATCH /api/v1/pleroma/admin/instance_document/{name}` — Pleroma's
/// `update`: the multipart `file` copied into the store and its public
/// URL answered.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::RouteNotFound`] for a name off
/// the configured list; [`AppError::BadRequest`] for a body without the
/// `file` part; [`AppError::PayloadTooLarge`] for a file over
/// [`MAX_DOCUMENT_BYTES`]; [`AppError::Internal`] for a store that
/// refuses to be written.
pub(super) async fn update_document(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(name): AxumPath<String>,
    mut form: MultipartForm<serde::de::IgnoredAny>,
) -> AppResult<Json<InstanceDocumentUrl>> {
    admin.require(CLASS_WRITE, AREA_DOCUMENTS)?;
    let path = document_path(&state.config, &name).ok_or(AppError::RouteNotFound)?;
    let file = form
        .files
        .take(FILE_FIELD)
        .ok_or_else(|| AppError::BadRequest(MISSING_FILE.to_owned()))?;
    let size = file.bytes.len();
    if size > MAX_DOCUMENT_BYTES {
        return Err(AppError::PayloadTooLarge(DOCUMENT_TOO_LARGE.to_owned()));
    }
    store_document(path, file.bytes).await?;
    debug!(document = %name, size, "instance document stored");
    Ok(Json(InstanceDocumentUrl {
        url: document_url(&state.config, &name),
    }))
}

/// `DELETE /api/v1/pleroma/admin/instance_document/{name}` — Pleroma's
/// `delete`: the file gone, the empty object back.
///
/// # Errors
///
/// As [`show_document`]'s gate and refusals; a store that refuses the
/// removal for any reason but absence is [`AppError::Internal`].
pub(super) async fn delete_document(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(name): AxumPath<String>,
) -> AppResult<Json<InstanceDocumentAck>> {
    admin.require(CLASS_WRITE, AREA_DOCUMENTS)?;
    let path = document_path(&state.config, &name).ok_or(AppError::RouteNotFound)?;
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {
            debug!(document = %name, "instance document deleted");
            Ok(Json(InstanceDocumentAck))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(AppError::RouteNotFound),
        Err(err) => Err(internal_store_failure(&err)),
    }
}

/// The stored path of a configured document name: `<dir>/<name>.html`,
/// `None` for every name the instance does not offer — off the list, or
/// not a safe segment. That is also the traversal gate: a `../` from a
/// path parameter names no file, and no write can escape the directory.
fn document_path(config: &Config, name: &str) -> Option<PathBuf> {
    if !is_document_name(name) {
        return None;
    }
    config
        .statics
        .documents
        .names
        .iter()
        .find(|configured| configured.as_str() == name)
        .map(|_| {
            config
                .statics
                .documents
                .dir
                .join(format!("{name}.{DOCUMENT_EXTENSION}"))
        })
}

/// Whether `name` may stand as one document segment: a safe filename
/// charset, non-empty, and neither `.` nor `..`.
fn is_document_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Where the frontend (or a proxy pointing at the document directory)
/// serves `name`: `{FRONTEND_BASE_URL}/instance/<name>.html`. The backend
/// serves no page there; the terms of service are read as JSON from
/// `GET /api/v1/instance/terms_of_service`.
fn document_url(config: &Config, name: &str) -> String {
    format!(
        "{}/instance/{name}.{DOCUMENT_EXTENSION}",
        config.frontend_base_url
    )
}

/// The document bytes behind a configured name.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for the unlisted and the never-uploaded;
/// [`AppError::Internal`] for any other read failure.
async fn read_document(config: &Config, name: &str) -> AppResult<Bytes> {
    let path = document_path(config, name).ok_or(AppError::RouteNotFound)?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|err| store_read_failure(&err))?;
    Ok(Bytes::from(bytes))
}

/// The document bytes behind a configured name and the time the file was
/// last written, the `updated_at` of the JSON API.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for the unlisted and the never-uploaded;
/// [`AppError::Internal`] for any other read failure.
pub async fn read_document_stamped(
    config: &Config,
    name: &str,
) -> AppResult<(Bytes, DateTime<Utc>)> {
    let path = document_path(config, name).ok_or(AppError::RouteNotFound)?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|err| store_read_failure(&err))?;
    let modified = tokio::fs::metadata(&path)
        .await
        .map_err(|err| store_read_failure(&err))?
        .modified()
        .map_err(|err| internal_store_failure(&err))?;
    Ok((Bytes::from(bytes), DateTime::<Utc>::from(modified)))
}

/// The error of a failed document read: absence is the 404, anything
/// else a 500.
fn store_read_failure(err: &std::io::Error) -> AppError {
    if err.kind() == std::io::ErrorKind::NotFound {
        AppError::RouteNotFound
    } else {
        internal_store_failure(err)
    }
}

/// Writes the document atomically: the bytes land in a hidden staging
/// file beside the target (same directory, so the rename is a plain
/// move) and the rename swaps them in, replacing whatever stood there.
/// The filesystem work runs on the blocking pool.
///
/// # Errors
///
/// [`AppError::Internal`] for a store that cannot be created, written or
/// renamed, and for a blocking task that did not finish.
async fn store_document(path: PathBuf, bytes: Bytes) -> AppResult<()> {
    tokio::task::spawn_blocking(move || write_document_blocking(&path, &bytes))
        .await
        .map_err(|err| {
            AppError::Internal(format!(
                "instance document write task did not finish: {err}"
            ))
        })?
}

/// The blocking half of [`store_document`]: directory, staging file,
/// rename.
fn write_document_blocking(path: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| internal_store_failure(&err))?;
    }
    let staging = staging_path(path);
    write_then_rename(&staging, path, bytes)
}

/// The staging sibling of `path`: a hidden dotfile in the same directory,
/// unique per attempt, so two concurrent uploads never share a file and
/// a reader never sees a half-written document.
fn staging_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.tmp-{}", new_id()))
}

/// Writes `bytes` to `staging` and renames it over `path`, removing the
/// staging file when only the rename failed.
fn write_then_rename(staging: &Path, path: &Path, bytes: &[u8]) -> AppResult<()> {
    std::fs::write(staging, bytes).map_err(|err| internal_store_failure(&err))?;
    match std::fs::rename(staging, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _cleanup = std::fs::remove_file(staging);
            Err(internal_store_failure(&err))
        }
    }
}

/// A filesystem failure that is not the client's: a 500 whose detail is
/// logged, never sent.
fn internal_store_failure(err: &std::io::Error) -> AppError {
    AppError::Internal(format!("the instance document store said: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document path: only configured, segment-safe names resolve,
    /// and every traversal-shaped or unlisted name is `None`.
    #[test]
    fn only_listed_safe_names_find_a_document_path() {
        let mut config = Config::for_tests();
        config.statics.documents.names = vec!["terms-of-service".to_owned()];
        assert_eq!(
            document_path(&config, "terms-of-service"),
            Some(config.statics.documents.dir.join("terms-of-service.html")),
            "the configured name maps to <dir>/<name>.html"
        );
        for refused in [
            "instance-panel",
            "",
            ".",
            "..",
            "../evil",
            "..%2fevil",
            "a/b",
            "terms of service",
        ] {
            assert!(
                document_path(&config, refused).is_none(),
                "`{refused}` is no document path"
            );
        }
    }

    /// The name gate itself: the plain-segment charset, nothing else.
    #[test]
    fn document_names_are_plain_segments() {
        for ok in ["terms-of-service", "about", "a_b.c-1", ".hidden"] {
            assert!(is_document_name(ok), "`{ok}` is a name");
        }
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "a b", "über", "~home"] {
            assert!(!is_document_name(bad), "`{bad}` is not a name");
        }
    }

    /// The answer URL hangs off the frontend origin, not the backend's.
    #[test]
    fn the_document_url_is_the_frontend_address() {
        let mut config = Config::for_tests();
        assert_eq!(
            document_url(&config, "terms-of-service"),
            "https://test.example/instance/terms-of-service.html"
        );
        config.frontend_base_url = "https://fe.example".to_owned();
        assert_eq!(
            document_url(&config, "terms-of-service"),
            "https://fe.example/instance/terms-of-service.html",
            "FRONTEND_BASE_URL, not BASE_URL"
        );
    }

    /// The stamped read returns the bytes with the file's write time, and
    /// the unlisted and never-uploaded are the 404.
    #[tokio::test]
    async fn the_stamped_read_returns_bytes_and_write_time() {
        let dir = tempfile::tempdir().expect("a docs dir");
        let mut config = Config::for_tests();
        config.statics.documents.dir = dir.path().to_path_buf();
        config.statics.documents.names = vec![TERMS_OF_SERVICE.to_owned()];

        let absent = read_document_stamped(&config, TERMS_OF_SERVICE)
            .await
            .expect_err("never uploaded");
        assert!(matches!(absent, AppError::RouteNotFound), "{absent:?}");
        let unlisted = read_document_stamped(&config, "privacy")
            .await
            .expect_err("unlisted");
        assert!(matches!(unlisted, AppError::RouteNotFound), "{unlisted:?}");

        let path = dir.path().join("terms-of-service.html");
        store_document(path, Bytes::from_static(b"<h1>terms</h1>"))
            .await
            .expect("the store works");
        let (bytes, stamp) = read_document_stamped(&config, TERMS_OF_SERVICE)
            .await
            .expect("the stored document reads");
        assert_eq!(bytes.as_ref(), b"<h1>terms</h1>", "the bytes verbatim");
        let age = Utc::now().signed_duration_since(stamp).num_seconds();
        assert!((-5..=60).contains(&age), "the stamp is the write time");
    }

    /// The staging sibling sits next to the target, hidden and uniquely
    /// named, so a crash or a concurrent upload never exposes a partial
    /// document.
    #[test]
    fn the_staging_sibling_is_a_hidden_unique_name_in_the_directory() {
        let path = Path::new("/srv/instance/terms-of-service.html");
        let staging = staging_path(path);
        let file_name = staging
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a name");
        assert!(
            file_name.starts_with(".terms-of-service.html.tmp-"),
            "hidden and suffixed: {file_name}"
        );
        assert_eq!(staging.parent(), path.parent(), "same directory");
        assert_ne!(staging, staging_path(path), "two stagings never collide");
    }

    /// A round trip against a real temporary directory: the first store
    /// creates the directory and the file, a re-store replaces it whole,
    /// and nothing is left staged.
    #[tokio::test]
    async fn documents_round_trip_through_the_store() {
        let dir = tempfile::tempdir().expect("a docs dir");
        let path = dir.path().join("deep").join("terms-of-service.html");

        store_document(path.clone(), Bytes::from_static(b"<h1>terms</h1>"))
            .await
            .expect("the first store works, creating the parent");
        assert_eq!(
            std::fs::read(&path).expect("readable"),
            b"<h1>terms</h1>",
            "the bytes landed whole"
        );

        store_document(path.clone(), Bytes::from_static(b"<h1>new</h1>"))
            .await
            .expect("the second store replaces");
        assert_eq!(
            std::fs::read(&path).expect("readable"),
            b"<h1>new</h1>",
            "and the new bytes are the whole file"
        );

        let deep = dir.path().join("deep");
        let strays: Vec<String> = std::fs::read_dir(&deep)
            .expect("the dir reads")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            strays,
            vec!["terms-of-service.html".to_owned()],
            "no staging left"
        );
    }

    /// A store path the process cannot write is an internal failure, not
    /// a client error dressed up.
    #[tokio::test]
    async fn an_unwritable_store_is_an_internal_failure() {
        let dir = tempfile::tempdir().expect("a docs dir");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").expect("a file to block with");
        let behind_file = blocker.join("terms-of-service.html");
        let err = store_document(behind_file, Bytes::from_static(b"x"))
            .await
            .expect_err("the store cannot write through a file");
        assert!(
            matches!(err, AppError::Internal(_)),
            "a filesystem refusal is a 500: {err:?}"
        );
    }
}
