//! Helpers shared by every emoji-pack test area: fixture image bytes, pack
//! seeding, response readers, admin and plain bearer tokens and the
//! moderation-log and custom-emoji readers.

use std::{
    collections::BTreeMap,
    io::{Cursor, Read as _},
    path::Path,
};

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    models::user::{Flag, NewUser},
    services::emoji_packs::{self, Pack, PackMeta},
};

use crate::common;

/// Bytes for every fixture image file.
pub(super) const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfake";
/// Magic bytes the emoji-file detection names as `image/gif`.
pub(super) const GIF_BYTES: &[u8] = b"GIF89a....";
/// Bytes no accepted image matcher names.
pub(super) const NOT_AN_IMAGE: &[u8] = b"print(1); // definitely no image";
/// Boundary the multipart file-route bodies are written with.
pub(super) const BOUNDARY: &str = "wired-test-boundary";

/// A manifest naming `files` (shortcode, file name) pairs, sharing its
/// files when `shared`.
pub(super) fn pack(shared: bool, files: &[(&str, &str)]) -> Pack {
    Pack {
        pack: PackMeta {
            share_files: shared,
            ..PackMeta::default()
        },
        files: files
            .iter()
            .map(|(shortcode, file)| ((*shortcode).to_owned(), (*file).to_owned()))
            .collect(),
    }
}

/// Writes pack `name` under the packs root `dir`: the manifest plus a real
/// PNG for every file it lists.
pub(super) async fn seed_pack(dir: &Path, name: &str, manifest: &Pack) {
    emoji_packs::save_pack(dir, name, manifest)
        .await
        .expect("fixture pack saves");
    for file in manifest.files.values() {
        let path = dir.join(name).join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture directories create");
        }
        std::fs::write(&path, PNG_BYTES).expect("fixture image writes");
    }
}

/// A minimal shared pack with two images, one nested, for the download and
/// static-route tests.
pub(super) fn cats_pack() -> Pack {
    pack(true, &[("cat", "cat.png"), ("kitten", "sub/kitten.png")])
}

/// The sorted entry names and bytes of a zip archive body.
pub(super) fn read_zip(body: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(body)).expect("response is a zip");
    let mut entries = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).expect("entry readable");
        let name = entry.name().expect("entry name is utf-8").into_owned();
        let mut payload = Vec::new();
        let read: usize = entry.read_to_end(&mut payload).expect("entry reads fully");
        assert_eq!(read, payload.len(), "entry {name} came back complete");
        let _previous: Option<Vec<u8>> = entries.insert(name, payload);
    }
    entries
}

/// The keys of the `packs` object of a listing body, in serialization
/// order.
pub(super) fn pack_names(body: &Value) -> Vec<&str> {
    body["packs"]
        .as_object()
        .expect("packs is an object")
        .keys()
        .map(String::as_str)
        .collect()
}

/// A local admin named `nickname` with a bearer token granting `scopes`.
pub(super) async fn admin_token(pool: &PgPool, nickname: &str, scopes: &str) -> String {
    let admin = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    common::create_app_and_token(pool, &admin, scopes).await
}

/// A local user with no staff role, with a bearer token granting `scopes`.
pub(super) async fn plain_token(pool: &PgPool, nickname: &str, scopes: &str) -> String {
    let user = common::create_local_user(pool, nickname).await;
    common::create_app_and_token(pool, &user, scopes).await
}

/// The `(action, message, actor)` triples of the moderation log, oldest
/// write first.
pub(super) async fn log_rows(pool: &PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as::<_, (String, String, String)>(
        "SELECT action, message, actor_nickname FROM moderation_log ORDER BY inserted_at, id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The expected form of one pack-write log row by `actor`.
pub(super) fn log_row(
    actor: &str,
    action: &str,
    verb: &str,
    pack: &str,
) -> (String, String, String) {
    (
        action.to_owned(),
        format!("@{actor} {verb} emoji pack: {pack}"),
        actor.to_owned(),
    )
}

/// The `GET /api/v1/custom_emojis` body as `(category, shortcode)` pairs
/// in the endpoint's category-then-shortcode order.
pub(super) async fn served_emojis(app: &axum::Router) -> Vec<(String, String)> {
    let response = common::TestRequest::get("/api/v1/custom_emojis")
        .send(app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "custom_emojis answers");
    response
        .json_value()
        .as_array()
        .expect("the endpoint answers an array")
        .iter()
        .map(|emoji| {
            (
                emoji["category"].as_str().unwrap_or_default().to_owned(),
                emoji["shortcode"]
                    .as_str()
                    .expect("an emoji entity")
                    .to_owned(),
            )
        })
        .collect()
}

/// The `verb` write against the gated pack route as `token` (`None` for
/// a request with no bearer header at all).
pub(super) async fn gated_write(
    app: &axum::Router,
    verb: &str,
    token: Option<&str>,
    name: &str,
) -> common::TestResponse {
    let path = format!("/api/v1/pleroma/emoji/pack?name={name}");
    let request = match verb {
        "post" => common::TestRequest::post(&path),
        "patch" => common::TestRequest::patch(&path).json(&json!({"metadata": {}})),
        _ => common::TestRequest::delete(&path),
    };
    match token {
        Some(bearer) => request.bearer(bearer).send(app).await,
        None => request.send(app).await,
    }
}

/// One write-auth case: who the caller is, the bearer token to send
/// (`None` for no header at all), the status owed and its error wording.
pub(super) type Refusal<'a> = (&'a str, Option<&'a str>, StatusCode, &'a str);
