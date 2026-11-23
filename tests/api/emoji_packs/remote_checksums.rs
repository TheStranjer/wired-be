//! `POST /packs/download` verifies the SHA-256 the remote listing
//! declares for the archive (Pleroma's `download_archive`): a match
//! installs, a mismatch or a missing digest is refused with nothing
//! written, for the shared archive and for a `fallback-src` alike.

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::api::instance::sha256_hex_bytes;

use super::{
    remote_mocks::{
        Declared, MockRemote, NOT_SHAREABLE, build_zip, installed_manifest, manifest_files,
        spawn_pack_server,
    },
    support::{PNG_BYTES, admin_token, served_emojis},
};
use crate::common;

/// The 400 body of an archive that does not hash to the declared digest.
const INVALID_CHECKSUM: &str = "Invalid pack checksum";

/// The listing of a pack that shares nothing and points at the mock's
/// own archive route as its `fallback-src`.
fn fallback_listing(base: &str, extra: &Value) -> Value {
    let mut pack = json!({
        "share-files": false,
        "fallback-src": format!("{base}/api/v1/pleroma/emoji/packs/archive"),
    });
    if let (Some(pack), Some(extra)) = (pack.as_object_mut(), extra.as_object()) {
        pack.extend(extra.clone());
    }
    json!({"pack": pack, "files": {"cat": "cat.png"}, "files_count": 1})
}

/// A shared pack whose archive matches its `download-sha256` installs,
/// whichever case the hex is written in (Pleroma writes uppercase).
#[tokio::test]
async fn a_matching_download_sha256_installs_the_shared_pack() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "sharedhasher", "admin:write").await;

    for (index, uppercase) in [false, true].into_iter().enumerate() {
        let archive = build_zip(&[("cat.png", PNG_BYTES)]);
        let digest = sha256_hex_bytes(&archive);
        let declared = if uppercase {
            digest.to_uppercase()
        } else {
            digest
        };
        let (base, _script, server) = spawn_pack_server(move |_| MockRemote {
            pack_first: json!({
                "pack": {"share-files": true, "download-sha256": declared},
                "files": {"cat": "cat.png"},
                "files_count": 1,
            }),
            archive,
            declared: Declared::Absent,
            ..MockRemote::default()
        })
        .await;
        let local = format!("cats-{index}");
        let response = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
            .bearer(&token)
            .json(&json!({"url": base, "name": "cats", "as": local}))
            .send(&app)
            .await;
        server.abort();
        common::assert_answer(&response, StatusCode::OK, "a matching digest");
        let installed = installed_manifest(dir.path(), &local).await;
        assert_eq!(
            manifest_files(&installed),
            [("cat", "cat.png")],
            "the verified archive is installed"
        );
        assert_eq!(
            installed.pack.download_sha256, None,
            "the download digest is not kept in the manifest"
        );
    }
    assert!(
        served_emojis(&app)
            .await
            .contains(&("cats-1".to_owned(), "cat".to_owned())),
        "the last verified download resynced"
    );
}

/// A shared pack whose archive does not hash to `download-sha256` is the
/// 400 "Invalid pack checksum", and nothing is left on disk.
#[tokio::test]
async fn a_mismatching_download_sha256_is_refused_and_writes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "sharedliar", "admin:write").await;

    for wrong in [
        sha256_hex_bytes(b"some other archive"),
        "not hex at all".to_owned(),
    ] {
        let (base, script, server) = spawn_pack_server(move |_| MockRemote {
            declared: Declared::Exactly(wrong),
            ..MockRemote::default()
        })
        .await;
        let response = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
            .bearer(&token)
            .json(&json!({"url": base, "name": "cats"}))
            .send(&app)
            .await;
        server.abort();
        common::assert_answer(&response, StatusCode::BAD_REQUEST, "a wrong digest");
        common::assert_error(&response, INVALID_CHECKSUM, "a wrong digest");
        assert_eq!(
            script.archive_hits.load(Ordering::SeqCst),
            1,
            "the archive was fetched, then refused"
        );
        assert!(
            std::fs::read_dir(dir.path()).is_ok_and(|entries| entries.count() == 0),
            "a refused archive leaves nothing on disk"
        );
    }
}

/// A shared pack whose listing declares no `download-sha256` is refused
/// before the archive is even requested.
#[tokio::test]
async fn a_shared_pack_without_a_digest_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "sharedblank", "admin:write").await;

    let (base, script, server) = spawn_pack_server(|_| MockRemote {
        declared: Declared::Absent,
        ..MockRemote::default()
    })
    .await;
    let response = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": base, "name": "cats"}))
        .send(&app)
        .await;
    server.abort();
    common::assert_answer(&response, StatusCode::BAD_REQUEST, "no digest");
    common::assert_error(&response, NOT_SHAREABLE, "no digest");
    assert_eq!(
        script.archive_hits.load(Ordering::SeqCst),
        0,
        "no archive is fetched without a digest to check it against"
    );
    assert!(
        std::fs::read_dir(dir.path()).is_ok_and(|entries| entries.count() == 0),
        "and nothing is written"
    );
}

/// A `fallback-src` archive installs under a correct
/// `fallback-src-sha256` (uppercase included), is the 400 "Invalid pack
/// checksum" under a wrong one, and is refused outright without one.
#[tokio::test]
async fn a_fallback_src_must_match_its_declared_sha256() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "fallbackhasher", "admin:write").await;

    let archive = build_zip(&[("cat.png", PNG_BYTES)]);
    let right = sha256_hex_bytes(&archive);
    let cases = [
        (json!({"fallback-src-sha256": right.to_uppercase()}), None),
        (json!({"fallback-src-sha256": right}), None),
        (
            json!({"fallback-src-sha256": sha256_hex_bytes(b"other")}),
            Some(INVALID_CHECKSUM),
        ),
        (json!({}), Some(NOT_SHAREABLE)),
    ];
    for (index, (extra, refusal)) in cases.into_iter().enumerate() {
        let archive = archive.clone();
        let (base, script, server) = spawn_pack_server(move |base| MockRemote {
            pack_first: fallback_listing(&base, &extra),
            archive,
            declared: Declared::Absent,
            ..MockRemote::default()
        })
        .await;
        let local = format!("fallback-{index}");
        let response = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
            .bearer(&token)
            .json(&json!({"url": base, "name": "cats", "as": local}))
            .send(&app)
            .await;
        server.abort();
        let hits = script.archive_hits.load(Ordering::SeqCst);
        if let Some(message) = refusal {
            common::assert_answer(&response, StatusCode::BAD_REQUEST, "a refused fallback");
            common::assert_error(&response, message, "a refused fallback");
            assert!(
                !dir.path().join(local.as_str()).exists(),
                "case {index}: a refused fallback leaves no pack"
            );
            assert_eq!(
                hits,
                usize::from(message == INVALID_CHECKSUM),
                "case {index}: only a declared digest gets the archive fetched"
            );
        } else {
            common::assert_answer(&response, StatusCode::OK, "a verified fallback");
            assert_eq!(hits, 1, "case {index}: the archive was fetched once");
            let installed = installed_manifest(dir.path(), &local).await;
            assert_eq!(
                manifest_files(&installed),
                [("cat", "cat.png")],
                "case {index}: the verified archive installed"
            );
        }
    }
}
