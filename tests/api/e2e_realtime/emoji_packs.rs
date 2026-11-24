//! Emoji-pack lifecycles on the served instance: creating, filling,
//! sharing and archiving a pack, installing one from a mock remote, and
//! importing a loose directory.

use std::{
    collections::BTreeMap,
    io::{Cursor, Read as _, Write as _},
    sync::Arc,
};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::Response,
    routing::get,
};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use wired_be::util::token::sha256_hex_bytes;

use super::server::{
    BASE, Part, SCOPES, Server, Verb, call, expect, get_value, png_fixture, post_multipart,
    write_ok,
};
use super::sockets::{account, admin, post_status};
use crate::common;

// ──────────────────────────  Scenario 5: emoji packs  ──────────────────────────

/// A pack listing's names, sorted: only their presence matters to the
/// scenarios, and the listing keys packs by name.
fn pack_names(body: &Value) -> Vec<String> {
    let mut names: Vec<String> = body["packs"]
        .as_object()
        .expect("the listing keys its packs")
        .keys()
        .cloned()
        .collect();
    names.sort_unstable();
    names
}

/// The pack directory names inside an emoji root, sorted: the level
/// `GET /packs` reads.
fn root_pack_dirs(root: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .expect("the root reads")
        .filter_map(|entry| {
            let entry = entry.expect("the entry reads");
            entry
                .path()
                .is_dir()
                .then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect();
    names.sort_unstable();
    names
}

/// A zip archive holding `entries`.
fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, payload) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .expect("the entry opens");
        writer.write_all(payload).expect("the entry writes");
    }
    writer.finish().expect("the archive closes").into_inner()
}

/// The named bytes of an archive body.
fn read_zip(body: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut reader = zip::ZipArchive::new(Cursor::new(body.to_vec())).expect("the body is a zip");
    let mut entries = BTreeMap::new();
    for index in 0..reader.len() {
        let mut file = reader.by_index(index).expect("the entry reads");
        let name = file.name().expect("the entry has a name").into_owned();
        let mut bytes = Vec::new();
        let read: usize = file.read_to_end(&mut bytes).expect("the entry drains");
        assert_eq!(read, bytes.len(), "entry {name} came back complete");
        let _past = entries.insert(name, bytes);
    }
    entries
}

/// A `Server` with the emoji root moved into a fresh scratch directory.
async fn emoji_server() -> (Server, tempfile::TempDir) {
    let root = tempfile::TempDir::new().expect("a scratch emoji root");
    let mut config = common::test_config();
    config.emoji_dir = root.path().to_path_buf();
    let srv = Server::with_config(config).await;
    (srv, root)
}

/// `GET /packs` and `GET /api/v1/custom_emojis` agree on the freshly
/// filled pack: one pack, one synced emoji, its image on the static
/// route's address and its category the pack name.
async fn assert_pack_listed(srv: &Server) {
    let listing = get_value(srv, None, "/api/v1/pleroma/emoji/packs").await;
    assert_eq!(pack_names(&listing), ["dogs"], "the pack lists");
    assert_eq!(listing["count"], json!(1), "with its total");
    let emojis = get_value(srv, None, "/api/v1/custom_emojis").await;
    let woof = emojis
        .as_array()
        .expect("the endpoint answers an array")
        .iter()
        .find(|emoji| emoji["shortcode"] == json!("woof"))
        .expect("the resync served the new file");
    assert_eq!(
        woof["url"],
        json!(format!("{BASE}/emoji/dogs/woof.png")),
        "the image hangs off the static route"
    );
    assert_eq!(woof["category"], json!("dogs"), "the pack is its category");
}

/// The sharing `PATCH` and the archive it unlocks: the zip comes back with
/// the manifest and the file byte for byte.
async fn assert_share_and_archive(srv: &Server, admin_token: &str, woof_png: &[u8]) {
    let _shared = write_ok(
        srv,
        Verb::Patch,
        "/api/v1/pleroma/emoji/pack?name=dogs",
        admin_token,
        &json!({ "metadata": { "share-files": true } }),
    )
    .await;
    let archive = call(
        srv,
        Verb::Get,
        "/api/v1/pleroma/emoji/packs/archive?name=dogs",
        None,
        None,
    )
    .await;
    expect(&archive, StatusCode::OK, "the archive download");
    assert_eq!(
        archive.header("content-type"),
        Some("application/zip"),
        "the body is a zip"
    );
    let entries = read_zip(&archive.body);
    let names: Vec<&str> = entries.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["pack.json", "woof.png"],
        "the manifest plus the file"
    );
    let manifest: Value = serde_json::from_slice(&entries["pack.json"]).expect("a manifest");
    assert_eq!(
        manifest["files"]["woof"],
        json!("woof.png"),
        "mapping its emoji"
    );
    assert_eq!(
        entries["woof.png"], woof_png,
        "the file bytes survived the round trip"
    );
}

/// The full pack journey on the served instance: `POST /pack` creates the
/// pack, `POST /packs/files` lands an uploaded image and resyncs the
/// custom emojis so `GET /api/v1/custom_emojis` lists it and a
/// `:shortcode:` reaction on a real status renders its image, `PATCH`
/// shares the pack, `GET /packs/archive` zips it back out with the
/// manifest, and the file itself serves from the static emoji route.
#[tokio::test]
async fn a_pack_is_created_filled_shared_archived_and_reacted_with() {
    let (srv, root) = emoji_server().await;
    let (_admin, admin_token) = admin(&srv, "rt226-emoji-admin", "admin:read admin:write").await;
    let (reactor, reactor_token) = account(&srv, "rt226-emoji-reactor", SCOPES).await;

    let _created = write_ok(
        &srv,
        Verb::Post,
        "/api/v1/pleroma/emoji/pack?name=dogs",
        &admin_token,
        &json!({}),
    )
    .await;
    let woof_png = png_fixture(11);
    let files = post_multipart(
        &srv,
        "/api/v1/pleroma/emoji/packs/files?name=dogs",
        &admin_token,
        &[
            Part::text("shortcode", "woof"),
            Part::file("file", "whatever name.png", woof_png.clone()),
        ],
    )
    .await;
    expect(&files, StatusCode::OK, "the pack file upload");
    assert_eq!(
        files.json(),
        json!({"woof": "woof.png"}),
        "the files object after the add, the client name replaced"
    );

    assert_pack_listed(&srv).await;

    let status = post_status(&srv, &reactor_token, "react to this", "public").await;
    let status_id = common::id_of(&status);
    let reacted = call(
        &srv,
        Verb::Put,
        &format!("/api/v1/pleroma/statuses/{status_id}/reactions/:woof:"),
        Some(&reactor_token),
        None,
    )
    .await;
    expect(&reacted, StatusCode::OK, "the custom-emoji reaction");
    assert_eq!(
        reacted.json()["pleroma"]["emoji_reactions"],
        json!([{
            "name": ":woof:",
            "count": 1,
            "me": true,
            "url": format!("{BASE}/emoji/dogs/woof.png"),
            "account_ids": [reactor.id.to_string()],
        }]),
        "the shortcode reacts with its image"
    );

    assert_share_and_archive(&srv, &admin_token, &woof_png).await;

    let served = call(&srv, Verb::Get, "/emoji/dogs/woof.png", None, None).await;
    expect(&served, StatusCode::OK, "the static emoji route");
    assert_eq!(served.body.as_ref(), woof_png.as_slice(), "byte for byte");
    assert_eq!(
        root_pack_dirs(root.path()),
        ["dogs"],
        "the pack lives under the scratch root only"
    );
}

/// The mock remote's script: the one-pack listing the download reads and
/// the archive it fetches.
#[derive(Debug)]
struct RemoteScript {
    /// The body of `GET /api/v1/pleroma/emoji/pack`.
    listing: Value,
    /// The bytes of `GET /api/v1/pleroma/emoji/packs/archive`.
    archive: Vec<u8>,
}

/// Serves the pack listing the download's two reads expect.
async fn remote_pack_list(State(script): State<Arc<RemoteScript>>) -> Json<Value> {
    Json(script.listing.clone())
}

/// Serves the pack archive.
async fn remote_pack_archive(State(script): State<Arc<RemoteScript>>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/zip")
        .body(Body::from(script.archive.clone()))
        .expect("the mock answer is a valid response")
}

/// A mock remote instance serving `script` on a loopback port: its base
/// URL and its task — the instance `POST /packs/download` installs from,
/// reachable because the test config allows private hosts.
async fn spawn_pack_remote(script: RemoteScript) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port for the mock remote");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    let app = Router::new()
        .route("/api/v1/pleroma/emoji/pack", get(remote_pack_list))
        .route(
            "/api/v1/pleroma/emoji/packs/archive",
            get(remote_pack_archive),
        )
        .with_state(Arc::new(script));
    let server = tokio::spawn(async move {
        if axum::serve(listener, app).await.is_err() {
            // A dead remote reports itself through the download's answer.
        }
    });
    (base, server)
}

/// Installing a pack from the outside, end to end: `POST /packs/download`
/// reads a mock remote instance's listing over real HTTP, fetches the
/// shared archive it names, and lands the pack under its local name with
/// its manifest's metadata, its files on disk and its emoji served.
#[tokio::test]
async fn a_pack_downloads_from_a_mock_remote() {
    let (srv, root) = emoji_server().await;
    let (_admin, admin_token) = admin(&srv, "rt226-installer", "admin:read admin:write").await;

    let cat_png = png_fixture(21);
    let archive = build_zip(&[
        (
            "pack.json",
            br#"{"pack": {"license": "MIT", "share-files": true}, "files": {"cat": "cat.png"}}"#
                .as_slice(),
        ),
        ("cat.png", cat_png.as_slice()),
    ]);
    let listing = json!({
        "pack": {
            "license": "MIT",
            "share-files": true,
            "download-sha256": sha256_hex_bytes(&archive).to_uppercase(),
        },
        "files": {"cat": "cat.png"},
        "files_count": 1,
    });
    let (base, remote) = spawn_pack_remote(RemoteScript { listing, archive }).await;

    let downloaded = write_ok(
        &srv,
        Verb::Post,
        "/api/v1/pleroma/emoji/packs/download",
        &admin_token,
        &json!({ "url": base, "name": "cats", "as": "imported" }),
    )
    .await;
    remote.abort();
    assert_eq!(downloaded, json!("ok"), "Pleroma's ok body");

    let installed = get_value(&srv, None, "/api/v1/pleroma/emoji/packs").await;
    assert_eq!(pack_names(&installed), ["imported"], "the installed pack");
    let emojis = get_value(&srv, None, "/api/v1/custom_emojis").await;
    assert!(
        emojis
            .as_array()
            .expect("an array")
            .iter()
            .any(|emoji| emoji["shortcode"] == json!("cat")),
        "the installed emoji serves: {emojis}"
    );
    assert_eq!(
        root_pack_dirs(root.path()),
        ["imported"],
        "the pack directory landed on disk"
    );
    let served = call(&srv, Verb::Get, "/emoji/imported/cat.png", None, None).await;
    expect(&served, StatusCode::OK, "the installed file serves");
    assert_eq!(
        served.body.as_ref(),
        cat_png.as_slice(),
        "the archive's bytes"
    );
}

/// The other install path: `GET /packs/import` writes the manifest for a
/// loose directory dropped into the emoji root by hand, so its files
/// become pack entries named after their stems — and a second import
/// finds nothing left to do.
#[tokio::test]
async fn loose_pack_directories_import_by_their_file_stems() {
    let (srv, root) = emoji_server().await;
    let (_admin, admin_token) = admin(&srv, "rt226-looser", "admin:read admin:write").await;

    std::fs::create_dir_all(root.path().join("loose")).expect("the loose dir writes");
    std::fs::write(root.path().join("loose/bark.png"), png_fixture(22))
        .expect("the loose file writes");

    let imported = get_value(
        &srv,
        Some(&admin_token),
        "/api/v1/pleroma/emoji/packs/import",
    )
    .await;
    assert_eq!(imported, json!(["loose"]), "the loose directory reported");
    let listing = get_value(&srv, None, "/api/v1/pleroma/emoji/packs").await;
    assert_eq!(pack_names(&listing), ["loose"], "now a pack");
    let manifest = std::fs::read_to_string(root.path().join("loose").join("pack.json"))
        .expect("the import wrote the manifest");
    assert!(
        manifest.contains("bark"),
        "the file stem keyed its emoji: {manifest}"
    );
    let emojis = get_value(&srv, None, "/api/v1/custom_emojis").await;
    assert!(
        emojis
            .as_array()
            .expect("an array")
            .iter()
            .any(|emoji| emoji["shortcode"] == json!("bark")),
        "the imported emoji serves: {emojis}"
    );
    let after = get_value(
        &srv,
        Some(&admin_token),
        "/api/v1/pleroma/emoji/packs/import",
    )
    .await;
    assert_eq!(after, json!([]), "everything has a manifest now");
}
