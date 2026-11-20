//! The remote-instance install routes of WBE-216: the `GET /packs/remote`
//! listing relay and `POST /packs/download` from a mock remote (shared,
//! fallback and refused).

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use wired_be::services::emoji_packs;

use super::{
    file_mocks::pack_entries,
    remote_mocks::{
        COULD_NOT_FETCH, MockRemote, NOT_SHAREABLE, UNREADABLE, UNSAFE_PATH, build_zip,
        installed_manifest, manifest_files, spawn_pack_server,
    },
    support::{GIF_BYTES, PNG_BYTES, admin_token, pack, seed_pack, served_emojis},
};
use crate::common;

/// `GET /packs/remote` reads `{url}/api/v1/pleroma/emoji/packs` through
/// the guarded client and relays it verbatim, forwarding the requested
/// `page` and this module's capped `page_size`.
#[tokio::test]
async fn remote_relays_the_packs_listing_of_an_instance() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "remotereader", "admin:read").await;

    let listing = json!({
        "count": 1,
        "packs": {
            "cats": {"pack": {"share-files": true}, "files": {"cat": "cat.png"}, "files_count": 1},
        },
    });
    let (base, _script, server) = spawn_pack_server(|_| MockRemote {
        packs: listing.clone(),
        ..MockRemote::default()
    })
    .await;

    let response = common::TestRequest::get(&format!(
        "/api/v1/pleroma/emoji/packs/remote?url={base}&page=2&page_size=500"
    ))
    .bearer(&token)
    .send(&app)
    .await;
    server.abort();
    common::assert_answer(&response, StatusCode::OK, "remote relay");
    let mut expected = listing;
    let _seen: Option<Value> = expected
        .as_object_mut()
        .unwrap()
        .insert("seen_query".to_owned(), json!("page=2&page_size=100"));
    assert_eq!(
        response.json_value(),
        expected,
        "the remote body as it answered, with the forwarded page and the clamped size"
    );
}

/// The relay's refusals: an absent or unusable `url` is the 400, and a
/// remote that is down, answers an error status or talks non-JSON is the
/// 500 `{"error":"Could not fetch the pack list"}` — and nothing of any
/// of them touches the local packs.
#[tokio::test]
async fn remote_refuses_bad_urls_and_broken_remotes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "remotescreener", "admin:read").await;

    for query in [
        "",
        "?url=",
        "?url=%20%20",
        "?url=not-a-url",
        "?url=ftp://x.test/packs",
    ] {
        let refused =
            common::TestRequest::get(&format!("/api/v1/pleroma/emoji/packs/remote{query}"))
                .bearer(&token)
                .send(&app)
                .await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, &format!("url {query:?}"));
        common::assert_error(
            &refused,
            "url must be a valid http(s) URL",
            &format!("url {query:?}"),
        );
    }

    // A port nothing listens on any more: the connect failure is the 500.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = listener.local_addr().unwrap().port();
    drop(listener);
    let dead = common::TestRequest::get(&format!(
        "/api/v1/pleroma/emoji/packs/remote?url=http://127.0.0.1:{dead_port}"
    ))
    .bearer(&token)
    .send(&app)
    .await;
    common::assert_answer(&dead, StatusCode::INTERNAL_SERVER_ERROR, "a dead remote");
    common::assert_error(&dead, COULD_NOT_FETCH, "a dead remote");

    // An error status and a non-JSON body answer the same.
    let (failing_base, _failing, failing_server) = spawn_pack_server(|_| MockRemote {
        list_status: StatusCode::INTERNAL_SERVER_ERROR,
        ..MockRemote::default()
    })
    .await;
    let failing = common::TestRequest::get(&format!(
        "/api/v1/pleroma/emoji/packs/remote?url={failing_base}"
    ))
    .bearer(&token)
    .send(&app)
    .await;
    failing_server.abort();
    common::assert_answer(&failing, StatusCode::INTERNAL_SERVER_ERROR, "a 500 remote");
    common::assert_error(&failing, COULD_NOT_FETCH, "a 500 remote");

    let (junk_base, _junk, junk_server) = spawn_pack_server(|_| MockRemote {
        packs_as_junk: true,
        ..MockRemote::default()
    })
    .await;
    let junk = common::TestRequest::get(&format!(
        "/api/v1/pleroma/emoji/packs/remote?url={junk_base}"
    ))
    .bearer(&token)
    .send(&app)
    .await;
    junk_server.abort();
    common::assert_answer(
        &junk,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a non-JSON remote",
    );
    common::assert_error(&junk, COULD_NOT_FETCH, "a non-JSON remote");

    assert!(
        std::fs::read_dir(dir.path()).is_ok_and(|entries| entries.count() == 0),
        "a relay never reads or writes local packs"
    );
}

/// `POST /packs/download` of a shared pack: the listing is re-read with
/// its `files_count` (Pleroma's two-pass read), the archive lands under
/// the `as` name, the shipped manifest survives reduced to the files
/// that actually arrived, non-images and hidden entries are skipped,
/// the custom emojis resync and the answer is the JSON string `"ok"`.
#[tokio::test]
async fn download_installs_a_shared_remote_pack() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packpuller", "admin:write").await;

    let remote = MockRemote {
        pack_first: json!({
            "pack": {"license": "CC0-1.0", "share-files": true},
            "files": {"cat": "cat.png"},
            "files_count": 3,
        }),
        pack_full: Some(json!({
            "pack": {"license": "CC0-1.0", "share-files": true},
            "files": {"cat": "cat.png", "kitten": "sub/kitten.gif", "ghost": "gone.png"},
            "files_count": 3,
        })),
        ..MockRemote::default()
    };
    let (base, script, server) = spawn_pack_server(|_| remote).await;

    let response = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": base, "name": "cats", "as": "imported-cats"}))
        .send(&app)
        .await;
    server.abort();
    common::assert_answer(&response, StatusCode::OK, "shared pack download");
    assert_eq!(response.json_value(), json!("ok"), "Pleroma's `ok` body");

    let seen = script.seen_pack.lock().await;
    assert_eq!(seen.len(), 2, "the two-pass listing read: {seen:?}");
    assert!(
        seen[0].contains("page_size=0") && seen[1].contains("page_size=3"),
        "the truncated first page is re-read with the announced files_count: {seen:?}"
    );
    drop(seen);

    let installed = installed_manifest(dir.path(), "imported-cats").await;
    assert_eq!(
        manifest_files(&installed),
        [("cat", "cat.png"), ("kitten", "sub/kitten.gif")],
        "the archive manifest, reduced to the files that landed"
    );
    assert_eq!(
        installed.pack.license.as_deref(),
        Some("MIT"),
        "the shipped metadata came with the pack"
    );
    assert!(installed.pack.share_files, "and its share flag too");
    assert_eq!(
        pack_entries(dir.path(), "imported-cats"),
        ["cat.png", "pack.json", "sub"],
        "the text and hidden entries wrote nothing"
    );
    assert_eq!(
        std::fs::read(dir.path().join("imported-cats/sub/kitten.gif")).unwrap(),
        GIF_BYTES,
        "nested files keep their path"
    );
    assert_eq!(
        served_emojis(&app).await,
        [
            ("imported-cats".to_owned(), "cat".to_owned()),
            ("imported-cats".to_owned(), "kitten".to_owned()),
        ],
        "the resync served the installed pack"
    );
}

/// An unshared pack with a `fallback-src` downloads from it; that
/// archive ships no manifest, so the listing's files map and metadata
/// become the pack's, and an absent `as` installs under the remote name.
#[tokio::test]
async fn download_follows_a_fallback_src_and_keeps_the_listing_map() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packfaller", "admin:write").await;

    let (base, script, server) = spawn_pack_server(|base| MockRemote {
        pack_first: json!({
            "pack": {
                "description": "from the listing",
                "share-files": false,
                "fallback-src": format!("{base}/api/v1/pleroma/emoji/packs/archive"),
            },
            "files": {"cat": "cat.png", "ghost": "gone.png"},
            "files_count": 2,
        }),
        archive: build_zip(&[("cat.png", PNG_BYTES)]),
        ..MockRemote::default()
    })
    .await;

    let response = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": base, "name": "cats"}))
        .send(&app)
        .await;
    server.abort();
    common::assert_answer(&response, StatusCode::OK, "fallback download");
    assert_eq!(
        script.archive_hits.load(Ordering::SeqCst),
        1,
        "the fallback archive was fetched once"
    );

    let installed = installed_manifest(dir.path(), "cats").await;
    assert_eq!(
        manifest_files(&installed),
        [("cat", "cat.png")],
        "the listing's map, minus the file that never arrived"
    );
    assert_eq!(
        installed.pack.description.as_deref(),
        Some("from the listing"),
        "the listing's metadata came with the pack"
    );
    assert!(!installed.pack.share_files, "and it stayed unshared");
    assert_eq!(
        served_emojis(&app).await,
        [("cats".to_owned(), "cat".to_owned())],
        "the resync served the installed pack"
    );
}

/// A remote pack that shares nothing and names no fallback is the 400
/// `{"error":"The pack was not set as shared and there is no fallback
/// src to download from"}`; no archive is ever asked for and no
/// directory is left behind.
#[tokio::test]
async fn download_refuses_an_unshared_pack_without_a_fallback_src() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packbeggar", "admin:write").await;

    let (base, script, server) = spawn_pack_server(|_| MockRemote {
        pack_first: json!({"pack": {"share-files": false}, "files": {}, "files_count": 0}),
        ..MockRemote::default()
    })
    .await;
    let response = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": base, "name": "cats"}))
        .send(&app)
        .await;
    server.abort();
    common::assert_answer(&response, StatusCode::BAD_REQUEST, "unshared pack");
    common::assert_error(&response, NOT_SHAREABLE, "unshared pack");
    assert_eq!(
        script.archive_hits.load(Ordering::SeqCst),
        0,
        "an unshared pack never asks for an archive"
    );
    assert!(
        !dir.path().join("cats").exists(),
        "the refused download created no pack"
    );
}

/// A download that cannot finish — a remote listing that errors, or an
/// archive that is not a zip — answers the 500 "Could not fetch the pack
/// list" / 400 "Could not read the pack archive", and the half-created
/// pack directory of the extraction failure is removed again.
#[tokio::test]
async fn download_refuses_broken_remotes_and_archives() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packchecker", "admin:write").await;

    let (failing_base, _failing, failing_server) = spawn_pack_server(|_| MockRemote {
        list_status: StatusCode::NOT_FOUND,
        ..MockRemote::default()
    })
    .await;
    let failing = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": failing_base, "name": "cats"}))
        .send(&app)
        .await;
    failing_server.abort();
    common::assert_answer(
        &failing,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a failing listing",
    );
    common::assert_error(&failing, COULD_NOT_FETCH, "a failing listing");

    let (junk_base, _junk, junk_server) = spawn_pack_server(|_| MockRemote {
        archive: b"this is not a zip".to_vec(),
        ..MockRemote::default()
    })
    .await;
    let junk = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": junk_base, "name": "cats"}))
        .send(&app)
        .await;
    junk_server.abort();
    common::assert_answer(&junk, StatusCode::BAD_REQUEST, "a broken archive");
    common::assert_error(&junk, UNREADABLE, "a broken archive");
    assert!(
        !dir.path().join("cats").exists(),
        "the failed install removed its half-written pack"
    );
    assert!(
        std::fs::read_dir(dir.path()).is_ok_and(|entries| entries.count() == 0),
        "and left no other trace"
    );
}

/// An archive whose entries climb out of the pack directory is the 400
/// `{"error":"Pack archive contains an unsafe path"}`: nothing outside
/// the pack is written and the half-created pack is removed.
#[tokio::test]
async fn download_refuses_a_traversal_archive() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packescaper", "admin:write").await;

    for (bad, outside) in [
        ("../../escape.png", "escape.png"),
        ("/abs.png", "abs.png"),
        ("ok/../../up.png", "up.png"),
    ] {
        let (base, _script, server) = spawn_pack_server(move |_| MockRemote {
            archive: build_zip(&[("cat.png", PNG_BYTES), (bad, PNG_BYTES)]),
            ..MockRemote::default()
        })
        .await;
        let refused = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
            .bearer(&token)
            .json(&json!({"url": base, "name": "cats"}))
            .send(&app)
            .await;
        server.abort();
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, &format!("entry {bad}"));
        common::assert_error(&refused, UNSAFE_PATH, &format!("entry {bad}"));
        assert!(
            !dir.path().join(outside).exists(),
            "the {bad} entry wrote nothing outside the pack"
        );
        assert!(
            !dir.path().join("cats").exists(),
            "and its pack was removed as the install failed"
        );
    }
}

/// An archive past the 20 MiB cap is refused mid-stream with the 500
/// "Could not download the pack archive", and an occupied local name —
/// checked after the remote work — answers the pinned 400 "Pack already
/// exists" without disturbing the pack that stands there.
#[tokio::test]
async fn download_respects_the_size_cap_and_the_existing_pack() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(true, &[("cat", "cat.png")])).await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packhoarder", "admin:write").await;

    let oversized = vec![b'x'; emoji_packs::MAX_PACK_ARCHIVE_BYTES + 1];
    let (base, _script, server) = spawn_pack_server(move |_| MockRemote {
        archive: oversized,
        ..MockRemote::default()
    })
    .await;
    let big = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": base, "name": "not-here"}))
        .send(&app)
        .await;
    server.abort();
    common::assert_answer(
        &big,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a 20 MiB plus one archive",
    );
    common::assert_error(
        &big,
        "Could not download the pack archive",
        "an oversize archive",
    );
    assert!(
        !dir.path().join("not-here").exists(),
        "the oversize fetch created no pack"
    );

    let (occupied_base, _occupied, occupied_server) =
        spawn_pack_server(|_| MockRemote::default()).await;
    let occupied = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": occupied_base, "name": "cats", "as": "  "}))
        .send(&app)
        .await;
    occupied_server.abort();
    common::assert_answer(&occupied, StatusCode::BAD_REQUEST, "an occupied name");
    common::assert_error(&occupied, "Pack already exists", "an occupied name");
    let kept = installed_manifest(dir.path(), "cats").await;
    assert_eq!(
        manifest_files(&kept),
        [("cat", "cat.png")],
        "the standing pack was neither overwritten nor emptied"
    );
}
