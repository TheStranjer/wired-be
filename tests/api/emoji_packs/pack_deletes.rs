//! The pack delete route and the cross-cutting pack-write behavior:
//! `DELETE /pack`, the `custom_emojis` resync every write triggers and the
//! admin gate on all three pack write verbs.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::{config::TEST_BASE_URL, services::emoji_packs};

use super::support::{
    Refusal, admin_token, cats_pack, gated_write, log_row, log_rows, pack, plain_token, seed_pack,
    served_emojis,
};
use crate::common;

/// `DELETE /pack` removes the pack directory with its files, resyncs
/// `custom_emojis` so the pack's shortcodes leave
/// `GET /api/v1/custom_emojis`, and answers `"ok"`; an absent or unusable
/// pack is the 404 `{"error":"Pack <name> does not exist"}` — including
/// deleting the same pack twice.
#[tokio::test]
async fn delete_removes_the_pack_and_forgets_its_emojis() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &cats_pack()).await;
    seed_pack(dir.path(), "dogs", &pack(false, &[("dog", "dog.png")])).await;
    let synced: usize = emoji_packs::sync_custom_emojis(&pool, TEST_BASE_URL, dir.path())
        .await
        .expect("fixture packs sync");
    assert_eq!(synced, 3, "three pack files stored before the delete");
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packreaper", "admin:write").await;
    assert_eq!(
        served_emojis(&app).await,
        [
            ("cats".to_owned(), "cat".to_owned()),
            ("cats".to_owned(), "kitten".to_owned()),
            ("dogs".to_owned(), "dog".to_owned()),
        ],
        "every pack file is served before the delete"
    );

    let deleted = common::TestRequest::delete("/api/v1/pleroma/emoji/pack?name=cats")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&deleted, StatusCode::OK, "delete");
    assert_eq!(deleted.json::<String>(), "ok");
    assert!(
        !dir.path().join("cats").exists(),
        "the whole directory — manifest and images — is gone"
    );
    assert!(dir.path().join("dogs").exists(), "the sibling pack stays");
    assert_eq!(
        served_emojis(&app).await,
        [("dogs".to_owned(), "dog".to_owned())],
        "the deleted pack's shortcodes left custom_emojis with it"
    );

    let again = common::TestRequest::delete("/api/v1/pleroma/emoji/pack?name=cats")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&again, StatusCode::NOT_FOUND, "delete twice");
    common::assert_error(&again, "Pack cats does not exist", "delete twice");

    for (query, message) in [
        ("%2e%2e%2fsecret", "Pack ../secret does not exist"),
        ("%2fetc%2fpasswd", "Pack /etc/passwd does not exist"),
        (".hidden", "Pack .hidden does not exist"),
    ] {
        let refused =
            common::TestRequest::delete(&format!("/api/v1/pleroma/emoji/pack?name={query}"))
                .bearer(&token)
                .send(&app)
                .await;
        common::assert_answer(&refused, StatusCode::NOT_FOUND, "the escaping delete");
        common::assert_error(&refused, message, "the escaping delete");
    }
    let no_name = common::TestRequest::delete("/api/v1/pleroma/emoji/pack")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&no_name, StatusCode::BAD_REQUEST, "delete without a name");
    common::assert_error(
        &no_name,
        "pack name cannot be empty",
        "delete without a name",
    );

    assert_eq!(
        log_rows(&pool).await,
        vec![log_row(
            "packreaper",
            "emoji_pack_delete",
            "deleted",
            "cats"
        )],
        "only the successful delete is logged"
    );
}

/// Any pack write re-runs the sync: a file that appeared in a pack's
/// manifest out of band is stored by the next write's resync, and
/// `GET /api/v1/custom_emojis` shows it without a restart.
#[tokio::test]
async fn every_write_resyncs_custom_emojis() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(false, &[("cat", "cat.png")])).await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packsyncer", "admin:write").await;
    assert!(
        served_emojis(&app).await.is_empty(),
        "nothing synced before the first write"
    );
    // A second shortcode arrives in the manifest out of band (the file
    // upload routes are their own ticket); no sync has run since.
    seed_pack(
        dir.path(),
        "cats",
        &pack(false, &[("cat", "cat.png"), ("kitten", "sub/kitten.png")]),
    )
    .await;

    // The create of an unrelated pack resyncs everything: kitten lands.
    let created = common::TestRequest::post("/api/v1/pleroma/emoji/pack?name=dogs")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&created, StatusCode::OK, "the resyncing create");
    assert_eq!(
        served_emojis(&app).await,
        [
            ("cats".to_owned(), "cat".to_owned()),
            ("cats".to_owned(), "kitten".to_owned()),
        ],
        "the new pack adds no emoji; the resync brought the cat pack"
    );
    // The patch resyncs too: drop kitten from the manifest on disk,
    // patch another pack, and the sweep forgets kitten.
    seed_pack(dir.path(), "cats", &pack(false, &[("cat", "cat.png")])).await;
    let patched = common::TestRequest::patch("/api/v1/pleroma/emoji/pack?name=dogs")
        .bearer(&token)
        .json(&json!({"metadata": {"license": "CC0-1.0"}}))
        .send(&app)
        .await;
    common::assert_answer(&patched, StatusCode::OK, "the resyncing patch");
    assert_eq!(
        served_emojis(&app).await,
        [("cats".to_owned(), "cat".to_owned())],
        "the sweep ran on the patch resync"
    );
}

/// Every pack write refuses weaker credentials the way the shared admin
/// gate words them, on all three verbs: no bearer is the 401 "Invalid
/// credentials", an unknown token the 401 "The access token is invalid",
/// an app-only token the 403 that demands a user, a valid user who is not
/// an admin the 403 "User is not an admin.", and an admin whose token
/// lacks `admin:write` the 403 naming it. None of them touches the disk
/// or the log; the narrow `admin:write:emoji` alias grants every route,
/// like the root it stands under.
#[tokio::test]
async fn pack_writes_demand_an_admin_with_admin_write() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let app_only = common::create_app_only_token(&pool, "admin:write").await;
    let plain = plain_token(&pool, "packpleb", "admin:write").await;
    let rootless = admin_token(&pool, "packrootless", "read write").await;

    let refusals: &[Refusal<'_>] = &[
        (
            "no bearer token",
            None,
            StatusCode::UNAUTHORIZED,
            "Invalid credentials",
        ),
        (
            "a bogus token",
            Some("not-a-real-token"),
            StatusCode::UNAUTHORIZED,
            "The access token is invalid",
        ),
        (
            "an app-only token",
            Some(&app_only),
            StatusCode::FORBIDDEN,
            "This API requires an authenticated user",
        ),
        (
            "a non-admin user",
            Some(&plain),
            StatusCode::FORBIDDEN,
            "User is not an admin.",
        ),
        (
            "an admin without admin:write",
            Some(&rootless),
            StatusCode::FORBIDDEN,
            "Insufficient permissions: admin:write.",
        ),
    ];
    for verb in ["post", "patch", "delete"] {
        for (who, token, status, message) in refusals {
            let response = gated_write(&app, verb, *token, "gated").await;
            common::assert_answer(&response, *status, &format!("{verb} as {who}"));
            common::assert_error(&response, message, &format!("{verb} as {who}"));
        }
    }
    assert!(
        !dir.path().join("gated").exists(),
        "no refused write touched the filesystem"
    );
    assert!(log_rows(&pool).await.is_empty(), "refusals log nothing");

    let narrow = admin_token(&pool, "packnarrow", "admin:write:emoji").await;
    let created = gated_write(&app, "post", Some(&narrow), "gated").await;
    common::assert_answer(&created, StatusCode::OK, "the narrow alias creates");
    let patched = gated_write(&app, "patch", Some(&narrow), "gated").await;
    common::assert_answer(&patched, StatusCode::OK, "the narrow alias patches");
    let deleted = gated_write(&app, "delete", Some(&narrow), "gated").await;
    common::assert_answer(&deleted, StatusCode::OK, "the narrow alias deletes");
}
