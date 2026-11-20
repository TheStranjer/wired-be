//! Fixtures for the emoji-file route tests: the add route's byte cap,
//! multipart body builders, a directory listing helper and a mock origin
//! the URL-file tests fetch from.

use std::{path::Path, sync::Arc};

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::StatusCode,
    response::Response,
    routing::get,
};
use tokio::{net::TcpListener, task::JoinHandle};

use crate::common;

/// The add route's per-file byte cap, mirrored for the test body.
pub(super) const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

/// `multipart/form-data` parts: text `fields` plus, when `file` is given,
/// a `file` part uploaded under that name.
pub(super) fn upload_parts(
    fields: &[(&'static str, &'static str)],
    file: Option<(&'static str, &'static [u8])>,
) -> Vec<common::MultipartPart> {
    let mut parts: Vec<common::MultipartPart> = fields
        .iter()
        .map(|(name, value)| common::MultipartPart::text(name, value))
        .collect();
    if let Some((filename, bytes)) = file {
        parts.push(common::MultipartPart::file(
            "file",
            filename,
            "image/png",
            Bytes::from_static(bytes),
        ));
    }
    parts
}

/// The sorted names inside pack `name`'s directory, for "nothing else
/// landed" assertions.
pub(super) fn pack_entries(dir: &Path, name: &str) -> Vec<String> {
    let mut entries: Vec<String> = std::fs::read_dir(dir.join(name))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort_unstable();
    entries
}

/// The scripted answer of the emoji-file mock origin.
#[derive(Debug)]
struct MockFile {
    /// The body to serve.
    bytes: Vec<u8>,
    /// The status to serve it with.
    status: StatusCode,
}

/// Answers with the script's status and bytes.
async fn serve_mock(State(mock): State<Arc<MockFile>>) -> Response {
    Response::builder()
        .status(mock.status)
        .body(Body::from(mock.bytes.clone()))
        .unwrap()
}

/// Serves `bytes` at `/file` on loopback with `status`, returning the
/// URL and the server task: the mock origin the URL-file tests fetch
/// from, reachable because the test config turns the SSRF guard off for
/// private hosts exactly as `HTTP_ALLOW_PRIVATE_HOSTS` does.
pub(super) async fn spawn_file_server(
    bytes: &[u8],
    status: StatusCode,
) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/file", get(serve_mock))
        .with_state(Arc::new(MockFile {
            bytes: bytes.to_vec(),
            status,
        }));
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("mock emoji-file server stopped: {err}");
        }
    });
    (format!("http://{addr}/file"), server)
}
