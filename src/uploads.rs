//! Upload storage: where files go, how their keys are shaped, and how they
//! come back out.
//!
//! Pleroma keeps object storage behind the `Pleroma.Uploaders.Uploader`
//! behaviour: `lib/pleroma/uploaders/local.ex` is its local-filesystem
//! implementation (write the upload's `path` under the configured uploads
//! directory, delete by path), and `Pleroma.Upload.store` in
//! `lib/pleroma/upload.ex` mints each upload's path as `id/name`. The
//! equivalents here are [`UploaderBackend`], [`LocalUploader`] and
//! [`media_key`].
//!
//! A *key* is the relative, slash-separated path a file is stored under, e.g.
//! `018f4c2e-…/9b3c1d0e4f5a6b7c8d9e0f1a.png`. Keys travel in URLs —
//! [`UploaderBackend::public_url`] turns one into its `GET /media/{key}`
//! address (see [`crate::api::media_files`]) — so every operation runs it
//! through [`check_key`] first: only `[A-Za-z0-9._/-]`, no leading `/`, no
//! `..` segment and no empty segment, which is what keeps a client-supplied
//! key inside the uploads directory.
//!
//! The stored file carries no metadata: its extension is what the media
//! route's `content_type_for_key` reads back when it serves a file, which is
//! why [`media_key`] picks the extension from the upload's content type.

use std::{io, path::PathBuf};

use bytes::Bytes;
use rand::RngCore as _;
use tracing::debug;
use uuid::Uuid;

use crate::{
    config::Config,
    error::{AppError, AppResult, io_error},
};

/// Bytes behind the random part of a media key: 8 bytes are 16 hex characters.
const RANDOM_HEX_BYTES: usize = 8;
/// Most characters of a client filename's extension that are kept, so a
/// nonsense name cannot make an unbounded key.
const MAX_EXTENSION_CHARS: usize = 8;
/// Extension of a stored file when neither its content type nor its filename
/// says what kind of file it is.
const DEFAULT_EXTENSION: &str = "bin";
/// What the media route serves a key with an unknown extension as: byte soup,
/// which is always an attachment (see [`is_inline_media`]).
const OCTET_STREAM: &str = "application/octet-stream";

/// Known media types with the extension they are stored under, as
/// `(content type, extension)` pairs, lowercase.
///
/// The rows are ordered so that lookups work in both directions: the first
/// row matching a content type gives an upload its canonical extension
/// (`image/jpeg` becomes `.jpg`, not `.jpeg`), and the first row matching an
/// extension gives the media route its content type (`.jpeg` still serves as
/// `image/jpeg`, `.ogg` as the `audio/ogg` it is listed as first).
const MEDIA_TYPES: &[(&str, &str)] = &[
    ("image/jpeg", "jpg"),
    ("image/jpeg", "jpeg"),
    ("image/jpg", "jpg"),
    ("image/png", "png"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
    ("video/mp4", "mp4"),
    ("video/webm", "webm"),
    ("audio/ogg", "ogg"),
    ("video/ogg", "ogg"),
    ("audio/mpeg", "mp3"),
    ("audio/mp3", "mp3"),
    ("audio/wav", "wav"),
    ("audio/x-wav", "wav"),
];

/// Where an instance's uploaded files live.
///
/// An enum rather than a trait object so another backend (S3, and what
/// Pleroma calls `Pleroma.Uploaders.S3`) can join as a variant without
/// changing any caller.
#[derive(Debug, Clone)]
pub enum UploaderBackend {
    /// Files on this machine's filesystem, under `UPLOADS_DIR`.
    Local(LocalUploader),
}

impl UploaderBackend {
    /// Stores `bytes` at `key`.
    ///
    /// `content_type` is the media type the file was uploaded as: the local
    /// backend does not record it (the extension in `key` is what the media
    /// route serves later), a future object-storage backend will set it on the
    /// object.
    ///
    /// # Errors
    ///
    /// [`AppError::BadRequest`] if `key` fails [`check_key`];
    /// [`AppError::Internal`] if the filesystem rejects the write.
    pub async fn put(&self, key: &str, bytes: Bytes, content_type: &str) -> AppResult<()> {
        match self {
            Self::Local(uploader) => uploader.put(key, bytes, content_type).await,
        }
    }

    /// Removes the file stored at `key`.
    ///
    /// # Errors
    ///
    /// [`AppError::BadRequest`] if `key` fails [`check_key`],
    /// [`AppError::NotFound`] if no file is stored there, and
    /// [`AppError::Internal`] if the filesystem rejects the removal.
    pub async fn delete(&self, key: &str) -> AppResult<()> {
        match self {
            Self::Local(uploader) => uploader.delete(key).await,
        }
    }

    /// Reads back the file stored at `key`.
    ///
    /// # Errors
    ///
    /// [`AppError::BadRequest`] if `key` fails [`check_key`],
    /// [`AppError::NotFound`] if no file is stored there, and
    /// [`AppError::Internal`] for any other read failure.
    pub async fn read(&self, key: &str) -> AppResult<Bytes> {
        match self {
            Self::Local(uploader) => uploader.read(key).await,
        }
    }

    /// The public URL of the file stored at `key`: the backend's base URL
    /// (`MEDIA_BASE_URL`) with the key appended, e.g.
    /// `https://social.example/media/{key}`. The key is not validated; callers
    /// pass keys they generated with [`media_key`].
    #[must_use]
    pub fn public_url(&self, key: &str) -> String {
        match self {
            Self::Local(uploader) => uploader.public_url(key),
        }
    }
}

/// The local-filesystem backend, Pleroma's `Pleroma.Uploaders.Local`
/// (`lib/pleroma/uploaders/local.ex`).
#[derive(Debug, Clone)]
pub struct LocalUploader {
    /// Directory all keys are relative to.
    dir: PathBuf,
    /// Public prefix of stored files' URLs.
    base_url: String,
}

impl LocalUploader {
    /// An uploader storing files under `dir` and announcing them at
    /// `base_url`, e.g. `https://social.example/media`.
    #[must_use]
    pub const fn new(dir: PathBuf, base_url: String) -> Self {
        Self { dir, base_url }
    }

    /// Stores `bytes` at `key`, creating the directories the key needs.
    ///
    /// # Errors
    ///
    /// [`AppError::BadRequest`] if `key` fails [`check_key`];
    /// [`AppError::Internal`] if the filesystem rejects the write.
    pub async fn put(&self, key: &str, bytes: Bytes, content_type: &str) -> AppResult<()> {
        check_key(key)?;
        let path = self.path_for(key);
        // Keys are namespaced (`{user}/{name}`), so the directory usually
        // does not exist yet.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|err| io_error("create upload directory", parent, &err))?;
        }
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|err| io_error("write upload", &path, &err))?;
        debug!(%key, %content_type, "stored upload");
        Ok(())
    }

    /// Removes the file stored at `key`.
    ///
    /// # Errors
    ///
    /// [`AppError::BadRequest`] if `key` fails [`check_key`],
    /// [`AppError::NotFound`] if no file is stored there, and
    /// [`AppError::Internal`] if the filesystem rejects the removal.
    pub async fn delete(&self, key: &str) -> AppResult<()> {
        check_key(key)?;
        let path = self.path_for(key);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Err(AppError::NotFound),
            Err(err) => Err(io_error("delete upload", &path, &err)),
        }
    }

    /// Reads back the file stored at `key`.
    ///
    /// # Errors
    ///
    /// [`AppError::BadRequest`] if `key` fails [`check_key`],
    /// [`AppError::NotFound`] if no file is stored there, and
    /// [`AppError::Internal`] for any other read failure.
    pub async fn read(&self, key: &str) -> AppResult<Bytes> {
        check_key(key)?;
        let path = self.path_for(key);
        match tokio::fs::read(&path).await {
            Ok(contents) => Ok(Bytes::from(contents)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Err(AppError::NotFound),
            Err(err) => Err(io_error("read upload", &path, &err)),
        }
    }

    /// See [`UploaderBackend::public_url`].
    #[must_use]
    pub fn public_url(&self, key: &str) -> String {
        format!("{}/{key}", self.base_url)
    }

    /// Where the file for `key` lives: `key` is relative and free of `..`
    /// segments ([`check_key`]), so the result is always inside [`dir`](Self::dir).
    fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(key)
    }
}

/// The backend for an instance's configuration: the local filesystem under
/// `UPLOADS_DIR`, served at `MEDIA_BASE_URL`.
impl From<&Config> for UploaderBackend {
    fn from(config: &Config) -> Self {
        Self::Local(LocalUploader::new(
            config.uploads_dir.clone(),
            config.media_base_url.clone(),
        ))
    }
}

/// The key to store `user_id`'s upload under: `{user_id}/{16 random hex}.{ext}`,
/// the `id/name` shape of Pleroma's `Pleroma.Upload.store`
/// (`lib/pleroma/upload.ex`) with a random name instead of the client's.
///
/// The random part is unguessable, so a key is a capability: anyone who has it
/// may read the file, and nobody may enumerate another user's uploads. The
/// result always passes [`check_key`].
///
/// `filename` is the name the client uploaded under, if any, and is only used
/// for its extension; `content_type` decides when it names a known media
/// type.
#[must_use]
pub fn media_key(user_id: Uuid, filename: Option<&str>, content_type: &str) -> String {
    let extension = extension_for(content_type, filename);
    let random = random_hex();
    format!("{user_id}/{random}.{extension}")
}

/// The extension a new key gets: from `content_type` when [`MEDIA_TYPES`]
/// knows it, else the sanitized extension of `filename`, else
/// [`DEFAULT_EXTENSION`].
fn extension_for(content_type: &str, filename: Option<&str>) -> String {
    extension_for_media_type(content_type).map_or_else(
        || {
            filename
                .and_then(filename_extension)
                .unwrap_or_else(|| DEFAULT_EXTENSION.to_owned())
        },
        str::to_owned,
    )
}

/// The canonical extension of `content_type`, ignoring any `; charset=…`
/// parameters and letter case; `None` for a type [`MEDIA_TYPES`] does not list.
fn extension_for_media_type(content_type: &str) -> Option<&'static str> {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    MEDIA_TYPES
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(media_type))
        .map(|(_, extension)| *extension)
}

/// The extension of a client filename, sanitized for use in a key: whatever
/// precedes the final path separator is dropped (some clients send a whole
/// path), the text after the last dot is kept, lowercased, stripped of anything
/// but `[a-z0-9]` and cut to [`MAX_EXTENSION_CHARS`] so it cannot carry `..`, a
/// separator or control bytes. `None` when a name has no extension or nothing
/// usable after the dot.
fn filename_extension(filename: &str) -> Option<String> {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let (_, extension) = base.rsplit_once('.')?;
    let sanitized: String = extension
        .to_ascii_lowercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(MAX_EXTENSION_CHARS)
        .collect();
    (!sanitized.is_empty()).then_some(sanitized)
}

/// 16 lowercase random hex characters from the OS CSPRNG: the unguessable name
/// part of a media key.
fn random_hex() -> String {
    let mut bytes = [0_u8; RANDOM_HEX_BYTES];
    // `fill_bytes` on the OS CSPRNG of this platform cannot fail; rand aborts
    // internally if the kernel ever refuses to deliver entropy.
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let number = u64::from_be_bytes(bytes);
    format!("{number:016x}")
}

/// Whether `c` may appear in a media key: the URL- and path-safe subset of
/// ASCII that cannot encode anything else.
const fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')
}

/// Validates a media key, the guard every storage operation runs first.
///
/// A key is a relative path inside the uploads directory: only
/// `[A-Za-z0-9._/-]` characters, no leading `/`, no `..` segment and no empty
/// segment (which covers a trailing `/`). Anything else could point outside
/// that directory or at a directory itself.
///
/// # Errors
///
/// [`AppError::BadRequest`] describing the first rule `key` breaks.
pub fn check_key(key: &str) -> AppResult<()> {
    let invalid = |rule: &str| AppError::BadRequest(format!("invalid media key: {rule}"));
    if key.is_empty() {
        return Err(invalid("must not be empty"));
    }
    if key.starts_with('/') {
        return Err(invalid("must be relative"));
    }
    if !key.chars().all(is_key_char) {
        return Err(invalid("must use only [A-Za-z0-9._/-]"));
    }
    for segment in key.split('/') {
        if segment == ".." {
            return Err(invalid("must not contain a `..` segment"));
        }
        if segment.is_empty() {
            return Err(invalid("must not contain an empty segment"));
        }
    }
    Ok(())
}

/// The content type to serve `key` as: the type [`MEDIA_TYPES`] pairs with the
/// key's extension, else `application/octet-stream` — an unknown extension
/// (`.html`, `.svg`, `.js`) never gets a type a browser would render.
pub(crate) fn content_type_for_key(key: &str) -> &'static str {
    let extension = key_extension(key).unwrap_or_default();
    MEDIA_TYPES
        .iter()
        .find(|(_, known)| known.eq_ignore_ascii_case(extension))
        .map_or(OCTET_STREAM, |(content_type, _)| *content_type)
}

/// The extension of `key`: the text after the last dot of its last segment,
/// `None` when that segment has no dot or nothing after it.
fn key_extension(key: &str) -> Option<&str> {
    let last = key.rsplit('/').next().unwrap_or(key);
    let (_, extension) = last.rsplit_once('.')?;
    (!extension.is_empty()).then_some(extension)
}

/// Whether `content_type` may be served inline: it must be one of the media
/// types in [`MEDIA_TYPES`], the allowlist the media route serves from.
/// Anything else — HTML, SVG (which would otherwise sneak past an `image/`
/// prefix), JSON, byte soup — arrives as an attachment, so it cannot run in
/// this instance's origin.
pub(crate) fn is_inline_media(content_type: &str) -> bool {
    MEDIA_TYPES.iter().any(|(known, _)| *known == content_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local uploader writing into a throwaway directory that is removed
    /// when the returned guard drops.
    fn temp_uploader() -> (tempfile::TempDir, LocalUploader) {
        let dir = tempfile::TempDir::new().unwrap();
        let uploader =
            LocalUploader::new(dir.path().to_path_buf(), "https://cdn.example/media".into());
        (dir, uploader)
    }

    /// Whether `text` is 16 lowercase hex characters.
    fn is_hex16(text: &str) -> bool {
        text.len() == 16
            && text
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    }

    #[test]
    fn media_key_uses_the_content_type_extension() {
        let id = Uuid::now_v7();
        for (content_type, extension) in [
            ("image/jpeg", "jpg"),
            ("image/png", "png"),
            ("image/gif", "gif"),
            ("image/webp", "webp"),
            ("video/mp4", "mp4"),
            ("video/webm", "webm"),
            ("video/ogg", "ogg"),
            ("audio/ogg", "ogg"),
            ("audio/mpeg", "mp3"),
            ("audio/wav", "wav"),
        ] {
            let key = media_key(id, Some("whatever.txt"), content_type);
            let (prefix, name) = key.split_once('/').unwrap();
            assert_eq!(
                prefix,
                id.as_hyphenated().to_string(),
                "key starts with the user: {key}"
            );
            assert!(
                name.ends_with(&format!(".{extension}")),
                "{content_type} stores as .{extension}, got {name}"
            );
        }
    }

    #[test]
    fn media_key_ignores_content_type_parameters_and_case() {
        let id = Uuid::now_v7();
        for content_type in ["IMAGE/PNG", "image/png; name=x", " image/png "] {
            let key = media_key(id, None, content_type);
            let (_, extension) = key.rsplit_once('.').unwrap();
            assert_eq!(extension, "png", "{content_type} is still a png: {key}");
        }
    }

    #[test]
    fn media_key_falls_back_to_the_filename_extension() {
        let id = Uuid::now_v7();
        for (filename, extension) in [
            ("screenshot.bmp", "bmp"),
            ("Cat.MOV", "mov"),
            ("C:\\Users\\sam\\photo.GIF", "gif"),
            ("notes.txt", "txt"),
            ("a.b.c.mkv", "mkv"),
            ("clip.002", "002"),
            ("way.long.abcdefghij", "abcdefgh"),
            ("weird.x y? z.tar.gz", "gz"),
        ] {
            let key = media_key(id, Some(filename), "application/octet-stream");
            assert!(
                key.ends_with(&format!(".{extension}")),
                "filename {filename} gives .{extension}, got {key}"
            );
        }
    }

    #[test]
    fn media_key_without_any_useful_extension_is_bin() {
        let id = Uuid::now_v7();
        for (content_type, filename) in [
            ("application/octet-stream", None),
            ("video/quicktime", None),
            ("video/quicktime", Some("clip")),
            ("video/quicktime", Some("clip.")),
            ("video/quicktime", Some("trailing-dot.wav.")),
            ("video/quicktime", Some("dir/")),
            ("video/quicktime", Some("clip.?!~")),
        ] {
            let key = media_key(id, filename, content_type);
            let (_, extension) = key.rsplit_once('.').unwrap();
            assert_eq!(
                extension, "bin",
                "{content_type} with {filename:?} gives .bin, got {key}"
            );
        }
    }

    #[test]
    fn media_key_name_is_random_hex_and_unique() {
        let id = Uuid::now_v7();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let key = media_key(id, None, "image/png");
            let (user, name) = key.split_once('/').unwrap();
            assert_eq!(
                user,
                id.as_hyphenated().to_string(),
                "namespace is the user"
            );
            let (random, extension) = name.rsplit_once('.').unwrap();
            assert!(is_hex16(random), "name part is 16 hex characters: {random}");
            assert_eq!(extension, "png", "extension is kept after the random part");
            assert!(seen.insert(key.clone()), "keys must not repeat: {key}");
        }
    }

    #[test]
    fn generated_keys_are_valid() {
        for content_type in ["image/png", "video/quicktime", "text/plain"] {
            let key = media_key(Uuid::now_v7(), Some("../../etc/passwd"), content_type);
            check_key(&key).unwrap_or_else(|err| panic!("{key} must be a valid key: {err}"));
        }
    }

    #[test]
    fn check_key_accepts_relative_media_paths() {
        for key in [
            "9b3c1d0e4f5a6b7c.png",
            "018f4c2e-1a2b-7000-8000-000000000000/9b3c1d0e4f5a6b7c.png",
            "a/b/c/d.mp4",
            "dots..in..name.txt",
            "_hidden/file-name_1.wav",
            "UPPER/CASE.PNG",
        ] {
            check_key(key).unwrap_or_else(|err| panic!("{key} is a valid key: {err}"));
        }
    }

    #[test]
    fn check_key_rejects_anything_that_could_escape() {
        for key in [
            "",
            "/",
            "/absolute.png",
            "../escape.png",
            "a/../../escape.png",
            "a/../b.png",
            "..",
            "a/..",
            "double//slash.png",
            "trailing-slash/",
            "with space.png",
            "with%20space.png",
            "nul\0byte.png",
            "back\\slash.png",
            "question?.png",
            "hash#tag.png",
            "new\nline.png",
            "unicode/é.png",
        ] {
            let err = check_key(key).unwrap_err();
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "{key:?} must be rejected with 400, got {err}"
            );
        }
    }

    #[test]
    fn public_url_joins_the_base_url_and_key() {
        let (_dir, uploader) = temp_uploader();
        assert_eq!(
            uploader.public_url("user/9b3c.png"),
            "https://cdn.example/media/user/9b3c.png",
            "the base URL and the key are concatenated with one slash"
        );
    }

    #[tokio::test]
    async fn put_creates_the_directories_and_read_returns_the_bytes() {
        let (_dir, uploader) = temp_uploader();
        let key = "018f4c2e-1a2b-7000-8000-000000000000/9b3c1d0e4f5a6b7c.png";
        let body = Bytes::from_static(b"\x89PNG\r\n\x1a\nfake");
        uploader.put(key, body.clone(), "image/png").await.unwrap();
        assert_eq!(
            uploader.read(key).await.unwrap(),
            body,
            "read returns what was stored"
        );
        assert!(
            uploader.dir.join(key).is_file(),
            "the file is on disk under the uploads directory"
        );
    }

    #[tokio::test]
    async fn put_overwrites_the_same_key() {
        let (_dir, uploader) = temp_uploader();
        uploader
            .put("u/a.txt", Bytes::from_static(b"one"), "text/plain")
            .await
            .unwrap();
        uploader
            .put("u/a.txt", Bytes::from_static(b"two"), "text/plain")
            .await
            .unwrap();
        assert_eq!(
            uploader.read("u/a.txt").await.unwrap(),
            Bytes::from_static(b"two")
        );
    }

    #[tokio::test]
    async fn delete_removes_the_file() {
        let (_dir, uploader) = temp_uploader();
        uploader
            .put("u/a.png", Bytes::from_static(b"png"), "image/png")
            .await
            .unwrap();
        uploader.delete("u/a.png").await.unwrap();
        assert!(!uploader.dir.join("u/a.png").exists(), "the file is gone");
        let err = uploader.read("u/a.png").await.unwrap_err();
        assert!(
            matches!(err, AppError::NotFound),
            "reading it back is a 404: {err}"
        );
    }

    #[tokio::test]
    async fn missing_files_are_not_found() {
        let (_dir, uploader) = temp_uploader();
        for err in [
            uploader.read("u/nope.png").await.unwrap_err(),
            uploader.delete("u/nope.png").await.unwrap_err(),
        ] {
            assert!(
                matches!(err, AppError::NotFound),
                "absent file is a 404: {err}"
            );
        }
    }

    #[tokio::test]
    async fn every_operation_rejects_an_unusable_key() {
        let (_dir, uploader) = temp_uploader();
        for key in ["../secret", "/etc/passwd", "a/../b", ""] {
            let errors = [
                uploader
                    .put(key, Bytes::new(), "image/png")
                    .await
                    .unwrap_err(),
                uploader.read(key).await.unwrap_err(),
                uploader.delete(key).await.unwrap_err(),
            ];
            let all_bad_requests = errors
                .iter()
                .all(|err| matches!(err, AppError::BadRequest(_)));
            assert!(
                all_bad_requests,
                "{key:?} must be a 400 from every operation, got {errors:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_traversal_key_cannot_touch_a_file_outside_the_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let secret = dir.path().join("outside.txt");
        std::fs::write(&secret, b"do not read me").unwrap();
        let uploads = dir.path().join("uploads");
        std::fs::create_dir_all(&uploads).unwrap();
        let uploader = LocalUploader::new(uploads, "https://test.example/media".into());
        // The obvious traversal is rejected outright...
        assert!(matches!(
            uploader.read("../outside.txt").await.unwrap_err(),
            AppError::BadRequest(_)
        ));
        // ...and the only thing a valid key can reach is inside the directory.
        assert!(matches!(
            uploader.read("outside.txt").await.unwrap_err(),
            AppError::NotFound
        ));
    }

    #[tokio::test]
    async fn backend_delegates_every_operation() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut config = Config::for_tests();
        config.uploads_dir = dir.path().to_path_buf();
        config.media_base_url = "https://test.example/media".to_owned();
        let backend = UploaderBackend::from(&config);
        let body = Bytes::from_static(b"video bytes");
        backend
            .put("u/clip.mp4", body.clone(), "video/mp4")
            .await
            .unwrap();
        assert_eq!(backend.read("u/clip.mp4").await.unwrap(), body);
        assert_eq!(
            backend.public_url("u/clip.mp4"),
            "https://test.example/media/u/clip.mp4"
        );
        backend.delete("u/clip.mp4").await.unwrap();
        assert!(matches!(
            backend.read("u/clip.mp4").await.unwrap_err(),
            AppError::NotFound
        ));
    }

    #[test]
    fn content_type_comes_from_the_extension() {
        for (key, content_type) in [
            ("u/a.jpg", "image/jpeg"),
            ("u/a.jpeg", "image/jpeg"),
            ("u/A.PNG", "image/png"),
            ("u/a.gif", "image/gif"),
            ("u/a.webp", "image/webp"),
            ("u/a.mp4", "video/mp4"),
            ("u/a.webm", "video/webm"),
            ("u/a.ogg", "audio/ogg"),
            ("u/a.mp3", "audio/mpeg"),
            ("u/a.wav", "audio/wav"),
        ] {
            assert_eq!(content_type_for_key(key), content_type, "type for {key}");
        }
    }

    #[test]
    fn unknown_or_missing_extensions_are_octet_stream() {
        for key in [
            "u/page.html",
            "u/icon.svg",
            "u/feed.xml",
            "u/script.js",
            "u/archive.tar.gz",
            "u/no-extension",
            "u/.hidden",
            "u/dots.in.name",
            "u/",
        ] {
            assert_eq!(
                content_type_for_key(key),
                OCTET_STREAM,
                "{key} is never served as a rendered type"
            );
        }
    }

    #[test]
    fn only_media_types_may_be_inline() {
        for content_type in ["image/png", "video/mp4", "audio/ogg"] {
            assert!(
                is_inline_media(content_type),
                "{content_type} renders inline"
            );
        }
        for content_type in [
            "text/html",
            "image/svg+xml",
            "application/json",
            OCTET_STREAM,
            "",
        ] {
            assert!(
                !is_inline_media(content_type),
                "{content_type} must be an attachment"
            );
        }
    }

    #[test]
    fn filename_extension_rules() {
        assert_eq!(
            filename_extension("path/to/photo.jpeg").as_deref(),
            Some("jpeg")
        );
        assert_eq!(filename_extension("..png").as_deref(), Some("png"));
        assert_eq!(filename_extension("no-extension"), None);
        assert_eq!(filename_extension("trailing."), None);
        assert_eq!(filename_extension("dir/"), None);
        assert_eq!(
            filename_extension("way.too.long.extension"),
            Some("extensio".to_owned()),
            "an extension is cut to MAX_EXTENSION_CHARS"
        );
    }
}
