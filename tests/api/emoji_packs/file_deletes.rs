//! The emoji-file delete route and the file-route gates: `DELETE
//! /packs/files`, the unknown-pack refusals shared by all three file
//! routes and the admin gate on every file write verb.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::services::emoji_packs;

use super::{
    file_mocks::{pack_entries, upload_parts},
    support::{
        BOUNDARY, PNG_BYTES, Refusal, admin_token, log_rows, pack, plain_token, seed_pack,
        served_emojis,
    },
};
use crate::common;

/// `DELETE /packs/files` drops the entry from `pack.json` and the file
/// from the pack directory — pruning the subdirectory a nested name
/// emptied — accepts the `shortcode` (and the `name`) from the query or
/// the body, answers the pack's full `files` object, and the resync
/// forgets the custom emoji with it.
#[tokio::test]
async fn delete_removes_the_entry_and_the_file() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(
        dir.path(),
        "cats",
        &pack(
            true,
            &[
                ("cat", "cat.png"),
                ("kitten", "sub/kitten.png"),
                ("lone", "lone.gif"),
            ],
        ),
    )
    .await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "filereaper", "admin:write").await;

    let via_query =
        common::TestRequest::delete("/api/v1/pleroma/emoji/packs/files?name=cats&shortcode=cat")
            .bearer(&token)
            .send(&app)
            .await;
    common::assert_answer(&via_query, StatusCode::OK, "delete from the query");
    assert_eq!(
        via_query.json_value(),
        json!({"kitten": "sub/kitten.png", "lone": "lone.gif"}),
        "the entry is gone from the files object"
    );
    assert!(
        !dir.path().join("cats").join("cat.png").exists(),
        "and its file with it"
    );

    let via_body = common::TestRequest::delete("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .form(&[("shortcode", "kitten")])
        .send(&app)
        .await;
    common::assert_answer(&via_body, StatusCode::OK, "delete from the body");
    assert_eq!(
        via_body.json_value(),
        json!({"lone": "lone.gif"}),
        "the nested entry left"
    );
    assert!(
        !dir.path().join("cats").join("sub").exists(),
        "the emptied subdirectory is pruned"
    );

    let last = common::TestRequest::delete("/api/v1/pleroma/emoji/packs/files")
        .bearer(&token)
        .json(&json!({"name": "cats", "shortcode": "lone"}))
        .send(&app)
        .await;
    common::assert_answer(&last, StatusCode::OK, "delete with the name in the body");
    assert_eq!(last.json_value(), json!({}), "the pack is empty now");
    assert_eq!(
        pack_entries(dir.path(), "cats"),
        ["pack.json"],
        "every file left with its entry"
    );
    assert!(
        served_emojis(&app).await.is_empty(),
        "and the custom emoji list followed the deletes"
    );
}

/// A delete of a shortcode the pack does not name is the 400
/// `{"error":"Emoji \"<shortcode>\" does not exist"}` — the same refusal
/// twice-deleted entries and never-there ones — while an entry whose
/// file already vanished off disk is still deletable: the manifest is
/// what must stop naming it.
#[tokio::test]
async fn delete_refuses_unknown_shortcodes_and_tolerates_off_disk_entries() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    emoji_packs::save_pack(dir.path(), "haunt", &pack(true, &[("ghost", "ghost.png")]))
        .await
        .expect("the manifest-only pack saves");
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "fileghoster", "admin:write").await;

    let ghost = common::TestRequest::delete("/api/v1/pleroma/emoji/packs/files?name=haunt")
        .bearer(&token)
        .json(&json!({"shortcode": "ghost"}))
        .send(&app)
        .await;
    common::assert_answer(&ghost, StatusCode::OK, "delete an off-disk entry");
    assert_eq!(ghost.json_value(), json!({}), "the entry left the manifest");

    for shortcode in ["ghost", "never-was"] {
        let again = common::TestRequest::delete(&format!(
            "/api/v1/pleroma/emoji/packs/files?name=haunt&shortcode={shortcode}"
        ))
        .bearer(&token)
        .send(&app)
        .await;
        common::assert_answer(&again, StatusCode::BAD_REQUEST, "delete twice");
        common::assert_error(
            &again,
            &format!("Emoji \"{shortcode}\" does not exist"),
            "delete twice",
        );
    }
}

/// All three file routes answer an unknown pack with the ticket's 400
/// `{"error":"Pack <name> does not exist"}`, fold a name that could
/// traverse into the same refusal (the routes cannot be used to probe
/// the filesystem), and demand the `name`: absent or blank is the
/// shared empty-value 400.
#[tokio::test]
async fn file_routes_refuse_unknown_and_unnamed_packs() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("outside.txt"), b"do not touch me").unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "fileguesser", "admin:write").await;

    for (request, message) in [
        (
            common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=nope")
                .json(&json!({"shortcode": "cat", "file": "https://unreachable.invalid/c.png"})),
            "Pack nope does not exist",
        ),
        (
            common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=nope")
                .json(&json!({"shortcode": "cat", "new_shortcode": "dog"})),
            "Pack nope does not exist",
        ),
        (
            common::TestRequest::delete(
                "/api/v1/pleroma/emoji/packs/files?name=nope&shortcode=cat",
            ),
            "Pack nope does not exist",
        ),
        (
            common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=%2e%2e%2fsecret")
                .json(&json!({"shortcode": "cat", "file": "https://unreachable.invalid/c.png"})),
            "Pack ../secret does not exist",
        ),
        (
            common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=%2fetc%2fpasswd")
                .json(&json!({"shortcode": "cat"})),
            "Pack /etc/passwd does not exist",
        ),
        (
            common::TestRequest::delete("/api/v1/pleroma/emoji/packs/files?name=&shortcode=cat"),
            "pack name cannot be empty",
        ),
        (
            common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files")
                .json(&json!({"shortcode": "cat"})),
            "pack name cannot be empty",
        ),
    ] {
        let refused = request.bearer(&token).send(&app).await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "the refused file write");
        common::assert_error(&refused, message, "the refused file write");
    }

    let siblings: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        siblings,
        ["outside.txt"],
        "no refused file write created or touched anything"
    );
    assert!(log_rows(&pool).await.is_empty(), "refusals log nothing");
}

/// One file-route `verb` against `/packs/files?name=gated` as `token`
/// (`None` for no bearer header), always with a body that would pass if
/// the caller were allowed: the gate refuses before anything else reads
/// it, and only the scope refusal needs the parseable body — the
/// extractor's role gate runs first.
async fn gated_file_write(
    app: &axum::Router,
    verb: &str,
    token: Option<&str>,
) -> common::TestResponse {
    let request = match verb {
        "post" => common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=gated")
            .multipart(
                BOUNDARY,
                &upload_parts(&[("shortcode", "gated")], Some(("g.png", PNG_BYTES))),
            ),
        "patch" => common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=gated")
            .json(&json!({"shortcode": "gated", "new_shortcode": "renamed"})),
        _ => common::TestRequest::delete(
            "/api/v1/pleroma/emoji/packs/files?name=gated&shortcode=renamed",
        ),
    };
    match token {
        Some(bearer) => request.bearer(bearer).send(app).await,
        None => request.send(app).await,
    }
}

/// Every file write refuses weaker credentials the way the shared admin
/// gate words them, on all three verbs, and none of them touches the
/// pack on disk or the moderation log; the narrow `admin:write:emoji`
/// alias grants all three.
#[tokio::test]
async fn file_writes_demand_an_admin_with_admin_write() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "gated", &pack(false, &[])).await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let app_only = common::create_app_only_token(&pool, "admin:write").await;
    let plain = plain_token(&pool, "filepleb", "admin:write").await;
    let rootless = admin_token(&pool, "filerootless", "read write").await;

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
            let response = gated_file_write(&app, verb, *token).await;
            common::assert_answer(&response, *status, &format!("{verb} as {who}"));
            common::assert_error(&response, message, &format!("{verb} as {who}"));
        }
    }
    assert_eq!(
        pack_entries(dir.path(), "gated"),
        ["pack.json"],
        "no refused file write touched the pack"
    );
    assert!(log_rows(&pool).await.is_empty(), "refusals log nothing");

    let narrow = admin_token(&pool, "filenarrow", "admin:write:emoji").await;
    let added = gated_file_write(&app, "post", Some(&narrow)).await;
    common::assert_answer(&added, StatusCode::OK, "the narrow alias adds");
    assert_eq!(added.json_value(), json!({"gated": "gated.png"}));
    let renamed = gated_file_write(&app, "patch", Some(&narrow)).await;
    common::assert_answer(&renamed, StatusCode::OK, "the narrow alias renames");
    assert_eq!(renamed.json_value(), json!({"renamed": "gated.png"}));
    let removed = gated_file_write(&app, "delete", Some(&narrow)).await;
    common::assert_answer(&removed, StatusCode::OK, "the narrow alias deletes");
    assert_eq!(removed.json_value(), json!({}));
}
