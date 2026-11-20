//! The emoji-file rename route: `PATCH /packs/files` moving a shortcode,
//! a file or both, the `force` overwrite and the rename refusals.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::services::emoji_packs;

use super::{
    file_mocks::pack_entries,
    support::{PNG_BYTES, admin_token, pack, seed_pack, served_emojis},
};
use crate::common;

/// `PATCH /packs/files` renames the shortcode, the file, or both: the
/// file on disk follows the entry (emptying subdirectories are pruned),
/// parameters ride the query, the body or both, and every success
/// answers the pack's full `files` object and moves the custom emoji
/// with it.
#[tokio::test]
async fn rename_moves_the_shortcode_and_the_file() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(
        dir.path(),
        "cats",
        &pack(true, &[("cat", "sub/cat.png"), ("dog", "dog.png")]),
    )
    .await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "filerenamer", "admin:write").await;

    let both = common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .json(&json!({
            "shortcode": "cat",
            "new_shortcode": "kitty",
            "new_filename": "kitty.png",
        }))
        .send(&app)
        .await;
    common::assert_answer(&both, StatusCode::OK, "rename both");
    assert_eq!(
        both.json_value(),
        json!({"dog": "dog.png", "kitty": "kitty.png"}),
        "the full files object after the rename"
    );
    assert_eq!(
        std::fs::read(dir.path().join("cats").join("kitty.png")).unwrap(),
        PNG_BYTES,
        "the file moved with the entry"
    );
    assert!(
        !dir.path().join("cats").join("sub").exists(),
        "the emptied subdirectory is pruned"
    );
    assert_eq!(
        served_emojis(&app).await,
        [
            ("cats".to_owned(), "dog".to_owned()),
            ("cats".to_owned(), "kitty".to_owned()),
        ],
        "the custom emoji list followed the rename"
    );

    // Shortcode only: the file stays where it is.
    let code = common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .form(&[("shortcode", "kitty"), ("new_shortcode", "cat")])
        .send(&app)
        .await;
    common::assert_answer(&code, StatusCode::OK, "rename the shortcode alone");
    assert_eq!(
        code.json_value(),
        json!({"cat": "kitty.png", "dog": "dog.png"}),
        "the stored file name kept"
    );

    // File only, and everything from the query string, no body at all.
    let file = common::TestRequest::patch(
        "/api/v1/pleroma/emoji/packs/files?name=cats&shortcode=cat&new_filename=c.png",
    )
    .bearer(&token)
    .send(&app)
    .await;
    common::assert_answer(&file, StatusCode::OK, "rename the file from the query");
    assert_eq!(
        file.json_value(),
        json!({"cat": "c.png", "dog": "dog.png"}),
        "query-only parameters work"
    );
    assert!(
        dir.path().join("cats").join("c.png").exists(),
        "and the file moved again"
    );
}

/// A rename into an occupied new shortcode is the 409
/// `{"error":"New shortcode \"<new>\" is already used. If you want to
/// override emoji use 'force' option"}` until `force` overwrites the
/// other entry — the custom emoji list follows the map.
#[tokio::test]
async fn rename_conflicts_unless_forced() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(
        dir.path(),
        "cats",
        &pack(true, &[("cat", "c.png"), ("dog", "dog.png")]),
    )
    .await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "fileclasher", "admin:write").await;

    let clash = common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .json(&json!({"shortcode": "cat", "new_shortcode": "dog"}))
        .send(&app)
        .await;
    common::assert_answer(&clash, StatusCode::CONFLICT, "the occupied new shortcode");
    common::assert_error(
        &clash,
        "New shortcode \"dog\" is already used. If you want to override emoji use 'force' option",
        "the occupied new shortcode",
    );
    assert_eq!(
        pack_entries(dir.path(), "cats"),
        ["c.png", "dog.png", "pack.json"],
        "the refused rename moved nothing"
    );

    let forced = common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .json(&json!({
            "shortcode": "cat",
            "new_shortcode": "dog",
            "new_filename": "c.png",
            "force": true,
        }))
        .send(&app)
        .await;
    common::assert_answer(&forced, StatusCode::OK, "the forced rename");
    assert_eq!(
        forced.json_value(),
        json!({"dog": "c.png"}),
        "force replaces the other entry with this one"
    );
    assert_eq!(
        served_emojis(&app).await,
        [("cats".to_owned(), "dog".to_owned())],
        "the overwritten shortcode left the emoji list"
    );
}

/// One refused rename: the pack, the body, the owed status and the
/// pinned error wording.
type RenameCase<'a> = (&'a str, Value, StatusCode, &'a str);

/// The rename's refusals: an unknown shortcode and an entry whose file
/// vanished off disk are the 400 `{"error":"Emoji \"<shortcode>\" does
/// not exist"}`, an empty `new_shortcode` or `new_filename` the 422, a
/// new shortcode that breaks the pattern and a new file name that could
/// traverse the 400s — and none of them moves anything.
#[tokio::test]
async fn rename_refuses_unknown_empty_and_unsafe() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(true, &[("cat", "cat.png")])).await;
    emoji_packs::save_pack(dir.path(), "haunt", &pack(true, &[("ghost", "ghost.png")]))
        .await
        .expect("the manifest-only pack saves");
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "filescreener", "admin:write").await;

    let cases: [RenameCase<'_>; 6] = [
        (
            "haunt",
            json!({"shortcode": "ghost", "new_filename": "here.png"}),
            StatusCode::BAD_REQUEST,
            "Emoji \"ghost\" does not exist",
        ),
        (
            "cats",
            json!({"shortcode": "nope", "new_shortcode": "x"}),
            StatusCode::BAD_REQUEST,
            "Emoji \"nope\" does not exist",
        ),
        (
            "cats",
            json!({"shortcode": "cat", "new_shortcode": ""}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "new_shortcode or new_filename cannot be empty",
        ),
        (
            "cats",
            json!({"shortcode": "cat", "new_filename": "", "new_shortcode": "ok"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "new_shortcode or new_filename cannot be empty",
        ),
        (
            "cats",
            json!({"shortcode": "cat", "new_shortcode": "bad code"}),
            StatusCode::BAD_REQUEST,
            "invalid emoji shortcode: bad code",
        ),
        (
            "cats",
            json!({"shortcode": "cat", "new_filename": "../escape.png"}),
            StatusCode::BAD_REQUEST,
            "invalid emoji file name: ../escape.png",
        ),
    ];
    for (pack_name, body, status, message) in cases {
        let refused = common::TestRequest::patch(&format!(
            "/api/v1/pleroma/emoji/packs/files?name={pack_name}"
        ))
        .bearer(&token)
        .json(&body)
        .send(&app)
        .await;
        common::assert_answer(&refused, status, "the refused rename");
        common::assert_error(&refused, message, "the refused rename");
    }

    assert_eq!(
        pack_entries(dir.path(), "cats"),
        ["cat.png", "pack.json"],
        "no refused rename touched a file"
    );
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("cats").join("pack.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        manifest["files"],
        json!({"cat": "cat.png"}),
        "and none touched the manifest"
    );
}
