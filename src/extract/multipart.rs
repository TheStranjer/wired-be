//! Multipart request bodies for upload endpoints, following
//! `docs/ARCHITECTURE.md` decision 10.
//!
//! Handlers that take files use [`MultipartForm<T>`] as their last
//! argument, with [`ApiBody`](crate::extract::ApiBody) as the fallback for
//! every other body.
//!
//! For a `multipart/form-data` body the extractor streams every part: text
//! fields are structured into one parameters value with exactly the
//! bracket/array rules [`ApiBody`](crate::extract::ApiBody) applies to
//! urlencoded bodies (the shared `parse_pairs` machinery), so a part named
//! `a[b]` or `ids[]` nests like the form key of the same shape, and file
//! parts are collected into [`UploadedFile`]s. Files must fit the
//! per-request [`UploadLimits`] in `AppState.config`: a part over
//! `UPLOAD_LIMIT_BYTES` is answered 413, more than `UPLOAD_MAX_FILES` of
//! them 422.
//!
//! For any other content type [`MultipartForm`] behaves exactly like
//! [`ApiBody`](crate::extract::ApiBody) and leaves `files` empty, so one
//! handler can accept JSON, urlencoded and multipart bodies alike. Errors
//! follow decision 8: a malformed body, part or boundary 400, a type
//! mismatch or too many files 422, an unsupported content type with a
//! non-empty body 415, an oversize file 413.
//!
//! Axum caps the bodies its extractors read at 2 MiB by default; routers
//! serving uploads must raise that with [`upload_body_limit`].

use axum::{
    body::Body,
    extract::{
        DefaultBodyLimit, FromRequest, Multipart,
        multipart::{Field, MultipartError},
    },
    http::{HeaderMap, Request, StatusCode, header::CONTENT_TYPE},
};
use bytes::Bytes;
use serde::de::DeserializeOwned;

use crate::{
    config::Config,
    error::AppError,
    extract::{body_value, into_params, media_type_of, parse_pairs},
    state::AppState,
};

/// Media type of a multipart request body.
const MULTIPART_MIME: &str = "multipart/form-data";

/// Bytes [`upload_body_limit`] allows on top of the configured file
/// payload: multipart framing (boundaries and part headers) and the text
/// fields that ride along with the files. Text fields have no limit of
/// their own; the body limit is what bounds them.
const UPLOAD_FRAMING_ALLOWANCE: usize = 1024 * 1024;

/// Per-request upload limits, configured by `UPLOAD_LIMIT_BYTES` and
/// `UPLOAD_MAX_FILES` and read from `AppState.config`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadLimits {
    /// Maximum size of one uploaded file, in bytes.
    pub max_file_bytes: usize,
    /// Maximum number of file parts in one request.
    pub max_files: usize,
}

/// One file part of a `multipart/form-data` body, buffered in memory.
#[derive(Clone, Debug)]
pub struct UploadedFile {
    /// Name of the form field the file was uploaded under.
    pub field: String,
    /// Client-sent file name, if the part carried one.
    pub filename: Option<String>,
    /// Part `content-type`, if the part carried one.
    pub content_type: Option<String>,
    /// The file's bytes.
    pub bytes: Bytes,
}

/// Accessor for a collection of [`UploadedFile`]s, the value of
/// [`MultipartForm::files`].
pub trait UploadedFiles {
    /// Removes and returns the first file uploaded under `field_name`.
    ///
    /// `None` when no file part arrived with that field name.
    fn take(&mut self, field_name: &str) -> Option<UploadedFile>;
}

impl UploadedFiles for Vec<UploadedFile> {
    fn take(&mut self, field_name: &str) -> Option<UploadedFile> {
        let index = self.iter().position(|file| file.field == field_name)?;
        // The range names the single found element, so it is in bounds.
        self.drain(index..=index).next()
    }
}

/// The parameters of an endpoint and the files sent along with them.
///
/// A handler argument `MultipartForm(form): MultipartForm<MyParams>`,
/// always the last argument because it consumes the body. A
/// `multipart/form-data` body fills `fields` from its text parts and
/// `files` from its file parts; any other content type parses by
/// [`ApiBody`](crate::extract::ApiBody)'s rules with `files` left empty.
#[derive(Debug)]
pub struct MultipartForm<T> {
    /// The deserialized text parameters.
    pub fields: T,
    /// The file parts, in the order they arrived.
    pub files: Vec<UploadedFile>,
}

/// The parameters and files of a multipart body, or the parameters alone
/// for any other content type, per decision 10.
///
/// # Errors
///
/// As [`ApiBody`](crate::extract::ApiBody)'s, plus [`AppError::PayloadTooLarge`]
/// for a file over `UPLOAD_LIMIT_BYTES` and [`AppError::Unprocessable`] for
/// more file parts than `UPLOAD_MAX_FILES`.
impl<T> FromRequest<AppState> for MultipartForm<T>
where
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(req: Request<Body>, state: &AppState) -> Result<Self, Self::Rejection> {
        if !is_multipart(req.headers()) {
            let fields = into_params(body_value(req).await?)?;
            return Ok(Self {
                fields,
                files: Vec::new(),
            });
        }
        let stream = Multipart::from_request(req, state)
            .await
            .map_err(|rejection| AppError::BadRequest(rejection.body_text()))?;
        let (texts, files) = read_parts(stream, state.config.upload_limits).await?;
        Ok(Self {
            fields: into_params(parse_pairs(texts))?,
            files,
        })
    }
}

/// Whether `headers` announce a `multipart/form-data` body. The boundary
/// itself is left to the multipart parser, which answers a missing or
/// malformed one with a 400.
fn is_multipart(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| media_type_of(content_type) == MULTIPART_MIME)
}

/// Drains the multipart `stream`, splitting it into the text fields'
/// decoded pairs and the file parts, with `limits` applied to the files.
///
/// # Errors
///
/// As [`MultipartForm`]'s `from_request`.
async fn read_parts(
    mut stream: Multipart,
    limits: UploadLimits,
) -> Result<(Vec<(String, String)>, Vec<UploadedFile>), AppError> {
    let mut texts: Vec<(String, String)> = Vec::new();
    let mut files: Vec<UploadedFile> = Vec::new();
    while let Some(field) = stream.next_field().await.map_err(|err| failure(&err))? {
        let name = field.name().unwrap_or_default().to_owned();
        let Some(filename) = field.file_name().map(str::to_owned) else {
            let bytes = field.bytes().await.map_err(|err| failure(&err))?;
            let text = std::str::from_utf8(&bytes).map_err(|err| {
                AppError::BadRequest(format!("Text field {name} is not valid UTF-8: {err}"))
            })?;
            texts.push((name, text.to_owned()));
            continue;
        };
        if files.len() >= limits.max_files {
            return Err(too_many_files(limits.max_files));
        }
        files.push(read_file(field, name, filename, limits.max_file_bytes).await?);
    }
    Ok((texts, files))
}

/// Buffers one file part, rejecting it as soon as it passes `max_bytes`.
///
/// # Errors
///
/// [`AppError::PayloadTooLarge`] when the part grows past `max_bytes`;
/// anything the broken stream leaves behind, mapped by [`failure`].
async fn read_file(
    mut field: Field<'_>,
    name: String,
    filename: String,
    max_bytes: usize,
) -> Result<UploadedFile, AppError> {
    let content_type = field.content_type().map(str::to_owned);
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(|err| failure(&err))? {
        if buffer.len() + chunk.len() > max_bytes {
            return Err(oversize(&name, max_bytes));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(UploadedFile {
        field: name,
        filename: Some(filename),
        content_type,
        bytes: Bytes::from(buffer),
    })
}

/// Maps a multipart parse failure onto an [`AppError`]: exceeding any body
/// limit keeps its 413, everything else is a malformed request (400).
fn failure(err: &MultipartError) -> AppError {
    let detail = err.body_text();
    if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
        AppError::PayloadTooLarge(detail)
    } else {
        AppError::BadRequest(detail)
    }
}

/// The 413 naming the field whose part passed the per-file byte limit.
fn oversize(name: &str, max_bytes: usize) -> AppError {
    AppError::PayloadTooLarge(format!(
        "File in field '{name}' exceeds the upload limit of {max_bytes} bytes"
    ))
}

/// The 422 naming the per-request file limit the body passed.
fn too_many_files(max_files: usize) -> AppError {
    AppError::Unprocessable(format!("Too many files: at most {max_files} per request"))
}

/// A [`DefaultBodyLimit`] layer sized for routers that serve uploads.
///
/// Axum's body extractors — the [`Multipart`] behind [`MultipartForm`]'s
/// multipart path — refuse request bodies over 2 MiB unless the route
/// carries a larger [`DefaultBodyLimit`], while the application-wide
/// middleware stack ([`crate::middleware`]) caps every body at
/// [`MAX_BODY_BYTES`] (16 MiB). A router whose routes take files that big
/// must therefore lift the extractor-level cap for exactly those routes:
///
/// ```text
/// Router::new()
///     .route("/api/v1/media", post(upload_media))
///     .layer(upload_body_limit(&state.config))
///     .with_state(state)
/// ```
///
/// The limit allows a maximal upload under `config`'s [`UploadLimits`] —
/// `UPLOAD_MAX_FILES` files of `UPLOAD_LIMIT_BYTES` bytes each — plus room
/// for multipart framing and text fields, and never less than
/// [`MAX_BODY_BYTES`], the cap [`ApiBody`](crate::extract::ApiBody) (and so
/// [`MultipartForm`]'s fallback path) reads other bodies under. It
/// composes with the middleware stack as the smaller of the two: keeping
/// `UPLOAD_LIMIT_BYTES` under the 16 MiB middleware cap — the
/// [`DEFAULT_UPLOAD_LIMIT_BYTES`] default does — makes the configured
/// upload limits, not the raw body size, what rejects an upload.
///
/// [`MAX_BODY_BYTES`]: crate::middleware::MAX_BODY_BYTES
/// [`DEFAULT_UPLOAD_LIMIT_BYTES`]: crate::config::DEFAULT_UPLOAD_LIMIT_BYTES
pub fn upload_body_limit(config: &Config) -> DefaultBodyLimit {
    let limits = config.upload_limits;
    let payload = limits
        .max_file_bytes
        .saturating_mul(limits.max_files)
        .saturating_add(UPLOAD_FRAMING_ALLOWANCE);
    DefaultBodyLimit::max(payload.max(crate::middleware::MAX_BODY_BYTES))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{Json, Router, routing::post};
    use http_body_util::BodyExt as _;
    use serde_json::{Value, json};
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt as _;

    use super::*;
    use crate::{
        config::{
            BASE_URL_VAR, DATABASE_URL_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
            UPLOAD_LIMIT_BYTES_VAR, UPLOAD_MAX_FILES_VAR,
        },
        http_client::HttpClient,
        uploads::UploaderBackend,
    };

    /// Boundary the test bodies are written with.
    const BOUNDARY: &str = "wired-test-boundary";

    /// The `content-type` announcing [`BOUNDARY`].
    const MULTIPART_TYPE: &str = "multipart/form-data; boundary=wired-test-boundary";

    /// A [`Config`] whose upload settings are the given limits, read
    /// through [`Config::from_lookup`] like the real environment.
    fn config_with_limits(max_file_bytes: usize, max_files: usize) -> Config {
        let bytes = max_file_bytes.to_string();
        let files = max_files.to_string();
        Config::from_lookup(|key| match key {
            DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
            BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
            UPLOAD_LIMIT_BYTES_VAR => Some(bytes.clone()),
            UPLOAD_MAX_FILES_VAR => Some(files.clone()),
            _ => None,
        })
        .unwrap()
    }

    /// State for extractor tests: the given upload limits over a pool that
    /// never connects (these handlers do not touch the database).
    fn state_with_limits(max_file_bytes: usize, max_files: usize) -> AppState {
        let config = Arc::new(config_with_limits(max_file_bytes, max_files));
        let db = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        let hub = crate::streaming::hub::Hub::new(&db);
        AppState {
            db,
            hub,
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
        }
    }

    /// The JSON an upload handler reports: what parsed, what arrived.
    async fn upload_handler(form: MultipartForm<Value>) -> Json<Value> {
        Json(json!({
            "fields": form.fields,
            "files": form.files.iter().map(file_summary).collect::<Vec<_>>(),
        }))
    }

    /// A JSON summary of one [`UploadedFile`].
    fn file_summary(file: &UploadedFile) -> Value {
        json!({
            "field": file.field,
            "filename": file.filename,
            "content_type": file.content_type,
            "size": file.bytes.len(),
            "text": String::from_utf8_lossy(&file.bytes),
        })
    }

    /// The upload route wired up with [`upload_body_limit`] and `state`.
    fn upload_app(state: AppState) -> Router {
        let limit = upload_body_limit(&state.config);
        Router::new()
            .route("/upload", post(upload_handler))
            .layer(limit)
            .with_state(state)
    }

    /// A text part of a multipart body.
    fn text_part(name: &str, value: &str) -> String {
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
    }

    /// A file part of a multipart body.
    fn file_part(name: &str, filename: &str, content_type: &str, data: &[u8]) -> Vec<u8> {
        let mut bytes = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; \
             filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .into_bytes();
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(b"\r\n");
        bytes
    }

    /// `parts` concatenated, closed off with the final boundary.
    fn multipart_body(parts: &[&[u8]]) -> Vec<u8> {
        let mut body = Vec::new();
        for part in parts {
            body.extend_from_slice(part);
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    /// POSTs `body` to the upload route as `multipart/form-data`.
    async fn send_multipart(body: &[u8], state: AppState) -> axum::response::Response {
        upload_app(state)
            .oneshot(
                Request::post("/upload")
                    .header(CONTENT_TYPE, MULTIPART_TYPE)
                    .body(Body::from(body.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// The status and body of a successful upload as JSON.
    async fn upload_json(body: &[u8], max_file_bytes: usize, max_files: usize) -> Value {
        let response = send_multipart(body, state_with_limits(max_file_bytes, max_files)).await;
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            StatusCode::OK,
            "upload accepted, got: {}",
            String::from_utf8_lossy(&bytes)
        );
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn multipart_with_text_and_file_fields_splits_fields_and_files() {
        let text = text_part("description", "hello world");
        let file = file_part("file", "cat.png", "image/png", b"PNGDATA");
        let body = multipart_body(&[text.as_bytes(), file.as_slice()]);

        let got = upload_json(&body, 1024, 4).await;
        assert_eq!(
            got["fields"]["description"], "hello world",
            "the text field reaches fields"
        );
        assert_eq!(got["files"].as_array().unwrap().len(), 1, "one file");
        let summary = &got["files"][0];
        assert_eq!(summary["field"], "file", "the field name");
        assert_eq!(summary["filename"], "cat.png", "the file name");
        assert_eq!(summary["content_type"], "image/png", "the content type");
        assert_eq!(summary["size"], 7, "the byte count");
        assert_eq!(summary["text"], "PNGDATA", "the bytes");
    }

    #[tokio::test]
    async fn bracket_keys_nest_in_multipart_text_fields() {
        let first = text_part("poll[options][]", "yes");
        let second = text_part("poll[options][]", "no");
        let third = text_part("poll[expires_in]", "3600");
        let body = multipart_body(&[first.as_bytes(), second.as_bytes(), third.as_bytes()]);

        let got = upload_json(&body, 1024, 4).await;
        assert_eq!(
            got["fields"],
            json!({"poll": {"options": ["yes", "no"], "expires_in": "3600"}}),
            "the same bracket rules as a urlencoded body"
        );
    }

    #[tokio::test]
    async fn oversize_file_gets_413() {
        let data = vec![b'x'; 64];
        let file = file_part("file", "big.bin", "application/octet-stream", &data);
        let response = send_multipart(
            &multipart_body(&[file.as_slice()]),
            state_with_limits(16, 4),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "a file over UPLOAD_LIMIT_BYTES"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let message: String = serde_json::from_slice::<Value>(&bytes).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            message.contains("16 bytes"),
            "the 413 names the limit: {message}"
        );
    }

    #[tokio::test]
    async fn too_many_files_gets_422() {
        let first = file_part("file", "a.txt", "text/plain", b"a");
        let second = file_part("file", "b.txt", "text/plain", b"b");
        let third = file_part("file", "c.txt", "text/plain", b"c");
        let body = multipart_body(&[first.as_slice(), second.as_slice(), third.as_slice()]);

        let response = send_multipart(&body, state_with_limits(1024, 2)).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "a third file over UPLOAD_MAX_FILES"
        );
    }

    #[tokio::test]
    async fn json_body_passes_through_with_empty_files() {
        let state = state_with_limits(1024, 4);
        let response = upload_app(state)
            .oneshot(
                Request::post("/upload")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"description":"from json"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "json accepted");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let got: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(got["fields"]["description"], "from json", "body parsed");
        assert_eq!(
            got["files"].as_array().unwrap().len(),
            0,
            "no files on a json body"
        );
    }

    #[tokio::test]
    async fn missing_file_field_takes_none() {
        /// Handler: `taken`/`present` are what the two `files.take` calls
        /// found.
        async fn take_handler(mut form: MultipartForm<Value>) -> Json<Value> {
            let taken = form.files.take("missing").is_some();
            let present = form.files.take("file").is_some();
            Json(json!({"taken": taken, "present": present}))
        }

        let text = text_part("description", "no file here");
        let body = multipart_body(&[text.as_bytes()]);
        let state = state_with_limits(1024, 4);
        let limit = upload_body_limit(&state.config);
        let app = Router::new()
            .route("/upload", post(take_handler))
            .layer(limit)
            .with_state(state);
        let response = app
            .oneshot(
                Request::post("/upload")
                    .header(CONTENT_TYPE, MULTIPART_TYPE)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "upload accepted");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let got: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(got["taken"], false, "a field no file arrived under");
        assert_eq!(got["present"], false, "and none under the other either");
    }

    #[tokio::test]
    async fn take_removes_only_the_named_files() {
        /// Handler: takes `a` once, then reports what remains.
        async fn take_handler(mut form: MultipartForm<Value>) -> Json<Value> {
            let taken = form.files.take("a").map(|file| file.field);
            Json(json!({
                "taken": taken,
                "left": form.files.iter().map(|file| file.field.clone()).collect::<Vec<_>>(),
            }))
        }

        let a = file_part("a", "a.txt", "text/plain", b"a");
        let b = file_part("b", "b.txt", "text/plain", b"b");
        let a_again = file_part("a", "a2.txt", "text/plain", b"a2");
        let body = multipart_body(&[a.as_slice(), b.as_slice(), a_again.as_slice()]);
        let state = state_with_limits(1024, 4);
        let limit = upload_body_limit(&state.config);
        let app = Router::new()
            .route("/upload", post(take_handler))
            .layer(limit)
            .with_state(state);
        let response = app
            .oneshot(
                Request::post("/upload")
                    .header(CONTENT_TYPE, MULTIPART_TYPE)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "upload accepted");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let got: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(got["taken"], "a", "the first a part");
        assert_eq!(
            got["left"],
            json!(["b", "a"]),
            "take removed exactly the first match"
        );
    }

    #[tokio::test]
    async fn missing_boundary_is_a_bad_request() {
        let state = state_with_limits(1024, 4);
        let response = upload_app(state)
            .oneshot(
                Request::post("/upload")
                    .header(CONTENT_TYPE, "multipart/form-data")
                    .body(Body::from("no parts at all"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "multipart without a boundary"
        );
    }

    #[tokio::test]
    async fn upload_body_limit_lifts_the_two_mebibyte_extractor_default() {
        // A file of just over 2 MiB, within the configured per-file limit.
        let data = vec![b'x'; 2 * 1024 * 1024 + 1024];
        let file = file_part("file", "big.bin", "application/octet-stream", &data);
        let body = multipart_body(&[file.as_slice()]);

        let got = upload_json(&body, 4_000_000, 4).await;
        assert_eq!(got["files"][0]["size"], data.len(), "the full file arrived");

        // Without the layer the same body fails: axum's multipart extractor
        // stops at its 2 MiB default before the 4 MB per-file limit applies.
        let state = state_with_limits(4_000_000, 4);
        let bare = Router::new()
            .route("/upload", post(upload_handler))
            .with_state(state);
        let response = bare
            .oneshot(
                Request::post("/upload")
                    .header(CONTENT_TYPE, MULTIPART_TYPE)
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "the 2 MiB extractor default rejects the same body"
        );
    }
}
