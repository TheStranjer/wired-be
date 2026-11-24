//! Fixtures for the pack install tests (WBE-216): the pinned error bodies,
//! fixture zip archives, and a mock remote instance serving the pack
//! listing and archive routes.

use std::{
    io::{Cursor, Write as _},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    body::Body,
    extract::State,
    http::{StatusCode, Uri, header},
    response::Response,
    routing::get,
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use wired_be::{
    services::emoji_packs::{self, Pack},
    util::token::sha256_hex_bytes,
};

use super::support::{GIF_BYTES, PNG_BYTES};

/// Magic bytes the emoji-file detection names as `image/webp`.
pub(super) const WEBP_BYTES: &[u8] = b"RIFF\x00\x00\x00\x00WEBPVP8 ";
/// The pinned 500 body of the remote-fetch routes.
pub(super) const COULD_NOT_FETCH: &str = "Could not fetch the pack list";
/// The pinned 400 body of an unshared, fallback-less remote pack.
pub(super) const NOT_SHAREABLE: &str =
    "The pack was not set as shared and there is no fallback src to download from";
/// The pinned 400 of the traversal refusal.
pub(super) const UNSAFE_PATH: &str = "Pack archive contains an unsafe path";
/// The pinned 400 of the unreadable-archive refusal.
pub(super) const UNREADABLE: &str = "Could not read the pack archive";

/// The manifest the fixture archives ship.
pub(super) const ARCHIVE_MANIFEST: &str = concat!(
    r#"{"pack": {"license": "MIT", "share-files": true}, "#,
    r#""files": {"cat": "cat.png", "kitten": "sub/kitten.gif", "ghost": "gone.png"}}"#,
);

/// The bytes of a zip holding `entries` (name, payload).
pub(super) fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, payload) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(payload).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// The shared pack's archive: two images (one nested), the manifest, and
/// a text and a hidden file the safe extraction must skip.
pub(super) fn good_archive() -> Vec<u8> {
    build_zip(&[
        ("pack.json", ARCHIVE_MANIFEST.as_bytes()),
        ("cat.png", PNG_BYTES),
        ("sub/kitten.gif", GIF_BYTES),
        ("notes.txt", b"prose, not an emoji".as_slice()),
        (".hidden.png", PNG_BYTES),
    ])
}

/// The SHA-256 a mock remote's one-pack listing declares for its archive.
#[derive(Debug, Default)]
pub(super) enum Declared {
    /// The real digest of the served archive, in `download-sha256` and
    /// `fallback-src-sha256` (unless the listing script sets its own).
    #[default]
    Matching,
    /// No digest at all.
    Absent,
    /// This exact text, whatever the archive hashes to.
    Exactly(String),
}

/// The scripted answer of a mock remote instance.
#[derive(Debug)]
pub(super) struct MockRemote {
    /// The digest the one-pack listing declares for the archive.
    pub(super) declared: Declared,
    /// The body of `GET /api/v1/pleroma/emoji/packs` (the relay's source).
    pub(super) packs: Value,
    /// The body of `GET /api/v1/pleroma/emoji/pack` for the first
    /// (`page_size=0`) read.
    pub(super) pack_first: Value,
    /// The body of the same route when the caller re-reads with a real
    /// `page_size` (Pleroma's `files_count` second pass); `None` serves
    /// the first body again.
    pub(super) pack_full: Option<Value>,
    /// The bytes of `GET /api/v1/pleroma/emoji/packs/archive`.
    pub(super) archive: Vec<u8>,
    /// The status both listing routes answer with.
    pub(super) list_status: StatusCode,
    /// The status the archive route answers with.
    pub(super) archive_status: StatusCode,
    /// Whether the packs route serves non-JSON junk.
    pub(super) packs_as_junk: bool,
    /// The queries the one-pack listing route saw, in order.
    pub(super) seen_pack: Mutex<Vec<String>>,
    /// How often the archive route was called.
    pub(super) archive_hits: AtomicUsize,
}

impl Default for MockRemote {
    fn default() -> Self {
        Self {
            declared: Declared::Matching,
            packs: json!({"count": 0, "packs": {}}),
            pack_first: json!({
                "pack": {"share-files": true},
                "files": {"cat": "cat.png", "kitten": "sub/kitten.gif"},
                "files_count": 2,
            }),
            pack_full: None,
            archive: good_archive(),
            list_status: StatusCode::OK,
            archive_status: StatusCode::OK,
            packs_as_junk: false,
            seen_pack: Mutex::new(Vec::new()),
            archive_hits: AtomicUsize::new(0),
        }
    }
}

/// A JSON response of the mock remote.
fn mock_json(status: StatusCode, body: &Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Serves the all-packs listing, echoing the query it was asked with,
/// so the relay test proves exactly what this instance forwarded.
async fn serve_packs_list(State(remote): State<Arc<MockRemote>>, uri: Uri) -> Response {
    if remote.packs_as_junk {
        return Response::builder()
            .status(remote.list_status)
            .body(Body::from("not json at all"))
            .unwrap();
    }
    let mut body = remote.packs.clone();
    if let Some(fields) = body.as_object_mut() {
        let _seen: Option<Value> = fields.insert(
            "seen_query".to_owned(),
            json!(uri.query().unwrap_or_default()),
        );
    }
    mock_json(remote.list_status, &body)
}

/// Serves one pack's listing, recording the query and answering the
/// full body once the caller asks beyond the first `page_size=0` read.
async fn serve_pack_show(State(remote): State<Arc<MockRemote>>, uri: Uri) -> Response {
    let query = uri.query().unwrap_or_default().to_owned();
    remote.seen_pack.lock().await.push(query.clone());
    let body = match (&remote.pack_full, query.contains("page_size=0")) {
        (Some(full), false) => full,
        _ => &remote.pack_first,
    };
    mock_json(remote.list_status, &with_declared_digest(&remote, body))
}

/// `listing` with the digest `remote` is scripted to declare written
/// into its `pack` block, except where the script already set one.
fn with_declared_digest(remote: &MockRemote, listing: &Value) -> Value {
    let digest = match &remote.declared {
        Declared::Absent => return listing.clone(),
        Declared::Matching => sha256_hex_bytes(&remote.archive),
        Declared::Exactly(text) => text.clone(),
    };
    let mut listing = listing.clone();
    if let Some(pack) = listing.get_mut("pack").and_then(Value::as_object_mut) {
        for key in ["download-sha256", "fallback-src-sha256"] {
            let _entry: &mut Value = pack.entry(key).or_insert_with(|| json!(digest));
        }
    }
    listing
}

/// Serves the pack archive and counts the hit.
async fn serve_pack_archive(State(remote): State<Arc<MockRemote>>) -> Response {
    let _hits = remote.archive_hits.fetch_add(1, Ordering::SeqCst);
    Response::builder()
        .status(remote.archive_status)
        .header(header::CONTENT_TYPE, "application/zip")
        .body(Body::from(remote.archive.clone()))
        .unwrap()
}

/// Serves a mock remote instance on loopback (reachable because the test
/// config allows private hosts exactly as `HTTP_ALLOW_PRIVATE_HOSTS`
/// does), built from its own base URL: returns that URL, the shared
/// script for assertions, and the server task.
pub(super) async fn spawn_pack_server<F>(build: F) -> (String, Arc<MockRemote>, JoinHandle<()>)
where
    F: FnOnce(String) -> MockRemote,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(build(base.clone()));
    let app = axum::Router::new()
        .route("/api/v1/pleroma/emoji/packs", get(serve_packs_list))
        .route("/api/v1/pleroma/emoji/pack", get(serve_pack_show))
        .route(
            "/api/v1/pleroma/emoji/packs/archive",
            get(serve_pack_archive),
        )
        .with_state(Arc::clone(&state));
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("mock emoji-pack remote stopped: {err}");
        }
    });
    (base, state, server)
}

/// The manifest of the pack installed under `name`, as stored on disk.
pub(super) async fn installed_manifest(dir: &Path, name: &str) -> Pack {
    emoji_packs::load_pack(dir, name)
        .await
        .unwrap_or_else(|err| panic!("the installed pack parses: {err}"))
}

/// The `(shortcode, file)` pairs of a pack manifest, ordered.
pub(super) fn manifest_files(pack: &Pack) -> Vec<(&str, &str)> {
    pack.files
        .iter()
        .map(|(code, file)| (code.as_str(), file.as_str()))
        .collect()
}
