//! Frontend settings store, mascot and preferred frontend
//! (`/api/v1/pleroma/settings/{app}`, `/api/v1/pleroma/mascot`,
//! `/api/v1/pleroma/preferred_frontend`, WBE-122) over the real router.
//!
//! The settings tests walk the deep-merge lifecycle (add, merge nested,
//! delete with `null`, sibling apps kept), pin the `{app}` name rule and
//! the 1 MiB store cap, and prove a refused write changes nothing. The
//! mascot tests pin the default fox, the upload path (stored bytes
//! readable at the announced URL, the attachment round-tripped through
//! `settings_store["mascot"]`), the 415 for non-image bytes and the 422
//! for a missing `file`. The frontend tests pin the config-backed
//! available list, the pick's column-and-cookie pair, and the 422 for a
//! name nobody made pickable. Every route runs the `AuthUser` pipeline;
//! `GET`/`PATCH` settings and the mascot routes also run their scope gate.

#![cfg(test)]

use axum::{Router, body::Bytes, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;
use wired_be::models::user::{self, User};

use crate::common::{self, MultipartPart};

/// The pickable frontend the fixtures configure.
const FRONTEND: &str = "pleroma-fe";
/// A frontend the fixtures never configure on the default list.
const OTHER_APP: &str = "mochi-pub";
/// A second frontend name: pickable when configured, refused when not.
const SECOND_FRONTEND: &str = "ghost-fe";
/// Boundary the multipart bodies are framed with.
const BOUNDARY: &str = "wired-settings-test-boundary";
/// Where the test instance serves stored files from.
const MEDIA_BASE_URL: &str = "https://test.example/media";
/// The size cap the endpoint enforces on the whole store.
const MAX_STORE_BYTES: usize = 1024 * 1024;
/// A `PNG`: the signature `infer` matches on, plus the start of an `IHDR`
/// chunk. Detection reads magic bytes only; nothing decodes the body.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
/// A `PDF`: bytes with a type, just not an image.
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
/// Text: bytes no matcher can name at all.
const TEXT: &[u8] = b"a plain text file, definitely not an image\n";

/// The settings route for `app_name`.
fn settings_path(app_name: &str) -> String {
    format!("/api/v1/pleroma/settings/{app_name}")
}

/// The mascot route.
const MASCOT: &str = "/api/v1/pleroma/mascot";
/// The available-frontends route.
const AVAILABLE: &str = "/api/v1/pleroma/preferred_frontend/available";
/// The preferred-frontend pick route.
const PREFERRED: &str = "/api/v1/pleroma/preferred_frontend";

/// Inserts a local user named `nickname` plus a bearer token granting
/// `scopes`: both, since one assertion needs the id and every request
/// needs the token.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// The stored row of user `id`.
async fn stored(pool: &PgPool, id: Uuid) -> User {
    user::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the fixture row exists")
}

/// `PATCH /api/v1/pleroma/settings/{app}` with `body` as JSON.
async fn patch_settings(
    app: &Router,
    token: &str,
    app_name: &str,
    body: Value,
) -> common::TestResponse {
    common::TestRequest::patch(&settings_path(app_name))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `GET /api/v1/pleroma/settings/{app}`.
async fn get_settings(app: &Router, token: &str, app_name: &str) -> common::TestResponse {
    common::TestRequest::get(&settings_path(app_name))
        .bearer(token)
        .send(app)
        .await
}

/// Wired's default mascot, as the endpoint fixes it for the test origin.
fn default_mascot() -> Value {
    json!({
        "id": "default-mascot",
        "url": "https://test.example/media/default/mascot.png",
        "type": "image",
        "pleroma": { "mime_type": "image/png" },
    })
}

/// The JSON overhead of the store `{"pleroma-fe": {"blob": "…"}}` carries
/// around the padded value: the wrapper the limit is measured against.
fn store_overhead() -> usize {
    serde_json::to_vec(&json!({FRONTEND: {"blob": ""}}))
        .unwrap()
        .len()
}

/// A throwaway uploads directory, removed when the returned guard drops.
fn uploads_dir() -> TempDir {
    TempDir::new().expect("temp dir for uploads")
}

/// The storage key of a mascot URL: the part below the media prefix.
fn storage_key(url: &str) -> String {
    url.strip_prefix(&format!("{MEDIA_BASE_URL}/"))
        .unwrap_or_else(|| panic!("{url} is not under {MEDIA_BASE_URL}"))
        .to_owned()
}

/// An unknown `{app}` reads as `{}`, and the merge lifecycle — add,
/// deep-merge, `null`-delete — round-trips through `GET` while a sibling
/// app's settings are kept untouched and the whole store lands in jsonb.
#[tokio::test]
async fn settings_merge_delete_and_read_back() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let app = common::test_app(pool.clone());

    let empty = get_settings(&app, &token, FRONTEND).await;
    assert_eq!(empty.status, StatusCode::OK, "GET answers 200");
    assert_eq!(
        empty.json_value(),
        json!({}),
        "an unknown app is the empty object"
    );

    let first = patch_settings(
        &app,
        &token,
        FRONTEND,
        json!({"theme": "dark", "nested": {"a": 1, "b": 2}}),
    )
    .await;
    assert_eq!(
        first.json_value(),
        json!({"theme": "dark", "nested": {"a": 1, "b": 2}}),
        "the merged object is the answer"
    );
    let sibling = patch_settings(&app, &token, OTHER_APP, json!({"keep": true})).await;
    assert_eq!(sibling.status, StatusCode::OK, "the second app joins");

    let second = patch_settings(
        &app,
        &token,
        FRONTEND,
        json!({"theme": null, "nested": {"b": null, "c": 3}, "plain": 7}),
    )
    .await;
    assert_eq!(
        second.json_value(),
        json!({"nested": {"a": 1, "c": 3}, "plain": 7}),
        "null deletes at every depth, a nested merge keeps siblings, scalars replace"
    );

    let read_back = get_settings(&app, &token, FRONTEND).await.json_value();
    assert_eq!(
        read_back,
        json!({"nested": {"a": 1, "c": 3}, "plain": 7}),
        "GET answers what PATCH stored"
    );
    assert_eq!(
        get_settings(&app, &token, OTHER_APP).await.json_value(),
        json!({"keep": true}),
        "the sibling app kept its own settings"
    );
    assert_eq!(
        stored(&pool, alice.id).await.settings_store,
        json!({
            FRONTEND: {"nested": {"a": 1, "c": 3}, "plain": 7},
            OTHER_APP: {"keep": true},
        }),
        "the whole store, as one jsonb blob"
    );
}

/// A stored non-object under the app name reads (and merges) as `{}` —
/// Pleroma's `ensure_object` — and a form-encoded patch body merges like
/// the JSON one (decision 10).
#[tokio::test]
async fn a_scalar_app_setting_reads_empty_and_a_form_body_merges() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.settings_store = json!({"junk-app": 5});
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;
    let app = common::test_app(pool.clone());

    assert_eq!(
        get_settings(&app, &token, "junk-app").await.json_value(),
        json!({}),
        "a non-object stored value is not a settings object"
    );

    let merged = common::TestRequest::patch(&settings_path("junk-app"))
        .bearer(&token)
        .form(&[("a[b]", "c"), ("n", "1")])
        .send(&app)
        .await;
    assert_eq!(
        merged.json_value(),
        json!({"a": {"b": "c"}, "n": "1"}),
        "the bracket keys nest, and the scalar 5 is gone"
    );
}

/// `{app}` must be 1 to 64 characters of `[A-Za-z0-9_.:-]`: a space, a
/// 65th character, a star and non-ASCII are 422 `{"error":"Invalid app"}`
/// on both verbs; the maximal legal name works.
#[tokio::test]
async fn the_app_segment_is_limited_to_bare_short_names() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read write").await;
    let app = common::test_app(pool.clone());

    for bad in ["bad%20name", &"x".repeat(65), "a%2Ab", "un%C3%AFcode"] {
        let refused = get_settings(&app, &token, bad).await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "`{bad}`");
        assert_eq!(
            refused.json_value(),
            json!({"error": "Invalid app"}),
            "the refusal body for `{bad}`"
        );
        let patch_refused = patch_settings(&app, &token, bad, json!({"x": 1})).await;
        assert_eq!(
            patch_refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "PATCH checks the name too"
        );
    }

    let ok = patch_settings(&app, &token, &"x".repeat(64), json!({"fits": true})).await;
    assert_eq!(ok.status, StatusCode::OK, "exactly 64 characters works");
}

/// The whole settings store is capped at 1 MiB of JSON text: the patch
/// that fits to the last byte succeeds, the one byte larger answers 422
/// with the ticket's body and leaves the store as it was — and a second
/// app cannot smuggle past a cap the first app already fills.
#[tokio::test]
async fn the_settings_store_is_capped_at_one_mebibyte() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let app = common::test_app(pool.clone());
    let fits = "x".repeat(MAX_STORE_BYTES - store_overhead());
    let body = json!({"blob": fits.clone()});

    let accepted = patch_settings(&app, &token, FRONTEND, body).await;
    assert_eq!(accepted.status, StatusCode::OK, "exactly 1 MiB is accepted");
    assert_eq!(
        get_settings(&app, &token, FRONTEND).await.json_value(),
        json!({"blob": fits}),
        "and it is the stored value"
    );

    let over = patch_settings(&app, &token, FRONTEND, json!({"blob": format!("y{fits}")})).await;
    assert_eq!(over.status, StatusCode::UNPROCESSABLE_ENTITY, "1 MiB+1");
    assert_eq!(
        over.json_value(),
        json!({"error": "settings_store: too large"}),
        "the ticket's refusal body"
    );

    let crowded = patch_settings(&app, &token, OTHER_APP, json!({"more": true})).await;
    assert_eq!(
        crowded.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a second app cannot join a full store"
    );
    assert_eq!(
        crowded.json_value(),
        json!({"error": "settings_store: too large"}),
        "the cap measures the whole store"
    );

    assert_eq!(
        stored(&pool, alice.id).await.settings_store,
        json!({FRONTEND: {"blob": fits}}),
        "both refusals stored nothing"
    );
}

/// A patch body that is not a JSON object is the extractor's 422, and a
/// store that already holds a `mascot` key round-trips through a merge
/// untouched.
#[tokio::test]
async fn a_non_object_patch_body_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.settings_store = json!({"mascot": {"id": "m"}});
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    let array = patch_settings(&app, &token, FRONTEND, json!([1, 2])).await;
    assert_eq!(
        array.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an array body is not a settings object"
    );
    assert_eq!(
        stored(&pool, alice.id).await.settings_store,
        json!({"mascot": {"id": "m"}}),
        "the refusal changed nothing"
    );

    let merged = patch_settings(&app, &token, FRONTEND, json!({"theme": "light"})).await;
    assert_eq!(merged.json_value(), json!({"theme": "light"}));
    assert_eq!(
        stored(&pool, alice.id).await.settings_store["mascot"],
        json!({"id": "m"}),
        "and the mascot key rode through the write unharmed"
    );
}

/// Without a stored mascot the answer is the instance default: Wired's
/// own mascot, served by the backend under `/media/default`.
#[tokio::test]
async fn the_mascot_default_is_the_instance_mascot() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::get(MASCOT)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the default answers 200");
    assert_eq!(
        response.json_value(),
        default_mascot(),
        "the instance's default mascot"
    );
}

/// The default mascot hangs under `MEDIA_BASE_URL`, wherever that points,
/// and ignores `FRONTEND_BASE_URL`; the test above pins the URL for the
/// default config.
#[tokio::test]
async fn the_mascot_default_follows_the_media_base_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let mut config = common::test_config();
    config.media_base_url = "https://cdn.example/files".to_owned();
    config.frontend_base_url = "https://fe.example".to_owned();
    let app = common::test_app_with_config(pool, config);

    let response = common::TestRequest::get(MASCOT)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the default answers 200");
    let url = response.json_value()["url"].as_str().unwrap().to_owned();
    assert_eq!(
        url, "https://cdn.example/files/default/mascot.png",
        "the default mascot is served by the backend's media tree, not the frontend"
    );
}

/// A PNG upload is stored through the uploader, announced as the
/// attachment entity (`id`, `url`, `type`, `pleroma.mime_type`), recorded
/// under `settings_store["mascot"]` and comes back from `GET`; the bytes
/// are readable at the URL the entity named.
#[tokio::test]
async fn the_mascot_upload_round_trips() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let response = common::TestRequest::put(MASCOT)
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &[MultipartPart::file(
                "file",
                "fox.png",
                "image/png",
                Bytes::from_static(PNG),
            )],
        )
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "a png is a mascot");
    let body = response.json_value();
    let url = body["url"].as_str().expect("the attachment names its url");
    assert_eq!(body["type"], "image", "the type is the entity's image kind");
    assert_eq!(
        body["pleroma"]["mime_type"], "image/png",
        "the detected mime rides in the pleroma object"
    );
    let id = body["id"].as_str().expect("the attachment has an id");
    let parsed = Uuid::parse_str(id).expect("the id is the hyphenated UUID of decision 4");
    assert_eq!(parsed.get_version_num(), 7, "the mascot id is a UUIDv7");
    assert!(
        url.starts_with(&format!("{MEDIA_BASE_URL}/{}", alice.id)),
        "the file lands in the uploader's namespaced key: {url}"
    );

    let stored_bytes = common::test_uploader(dir.path())
        .read(&storage_key(url))
        .await
        .expect("the announced URL is a stored key");
    assert_eq!(stored_bytes, Bytes::from_static(PNG), "the exact bytes");

    assert_eq!(
        stored(&pool, alice.id).await.settings_store["mascot"],
        body,
        "settings_store keeps the attachment verbatim"
    );

    let again = common::TestRequest::get(MASCOT)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(again.json_value(), body, "GET answers the stored mascot");
}

/// Bytes that are not an image — whatever `content-type` the part claims —
/// are the 415 with the ticket's body, a file part without bytes included;
/// no `file` part at all is a 422. Neither touches the stored mascot.
#[tokio::test]
async fn a_mascot_can_only_be_an_image() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for (name, claimed, bytes) in [
        ("notes.pdf", "image/png", PDF),
        ("notes.txt", "image/png", TEXT),
        ("empty.png", "image/png", &[][..]),
    ] {
        let response = common::TestRequest::put(MASCOT)
            .bearer(&token)
            .multipart(
                BOUNDARY,
                &[MultipartPart::file(
                    "file",
                    name,
                    claimed,
                    Bytes::copy_from_slice(bytes),
                )],
            )
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "`{name}` is not an image"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "mascots can only be images"}),
            "the ticket's 415 body for `{name}`"
        );
    }

    let missing = common::TestRequest::put(MASCOT)
        .bearer(&token)
        .multipart(BOUNDARY, &[MultipartPart::text("description", "no file")])
        .send(&app)
        .await;
    assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    assert_eq!(
        missing.json_value(),
        json!({"error": "Missing parameter: file"}),
        "the missing part is named"
    );
    assert_eq!(
        stored(&pool, alice.id).await.settings_store,
        json!({}),
        "no refusal stored a mascot"
    );
}

/// The available list is `AVAILABLE_FRONTENDS`: empty by default, and the
/// configured names in configuration order.
#[tokio::test]
async fn available_frontends_come_from_the_config() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let default_app = common::test_app(pool.clone());
    assert_eq!(
        common::TestRequest::get(AVAILABLE)
            .bearer(&token)
            .send(&default_app)
            .await
            .json_value(),
        json!([]),
        "an unconfigured instance offers nothing"
    );

    let mut config = common::test_config();
    config.available_frontends = vec![FRONTEND.to_owned(), SECOND_FRONTEND.to_owned()];
    let picky = common::test_app_with_config(pool.clone(), config);
    assert_eq!(
        common::TestRequest::get(AVAILABLE)
            .bearer(&token)
            .send(&picky)
            .await
            .json_value(),
        json!([FRONTEND, SECOND_FRONTEND]),
        "the configured list, in order"
    );
}

/// A pick of a name the operator made available is stored in the account's
/// column, echoed back in the year-lived cookie and answered as
/// `{"frontend_name": …}`; a later pick — sent as a form body, the other
/// spelling decision 10 accepts — replaces it.
#[tokio::test]
async fn a_preferred_frontend_pick_is_stored_and_cookied() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let mut config = common::test_config();
    config.available_frontends = vec![FRONTEND.to_owned(), SECOND_FRONTEND.to_owned()];
    let app = common::test_app_with_config(pool.clone(), config);

    let response = common::TestRequest::put(PREFERRED)
        .bearer(&token)
        .json(&json!({"frontend_name": FRONTEND}))
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the pick is accepted");
    assert_eq!(
        response.json_value(),
        json!({"frontend_name": FRONTEND}),
        "the ticket's body"
    );
    assert_eq!(
        response.header("set-cookie"),
        Some("preferred_frontend=pleroma-fe; Path=/; Max-Age=31536000"),
        "the cookie the frontends read, one year, whole origin"
    );
    assert_eq!(
        stored(&pool, alice.id).await.preferred_frontend.as_deref(),
        Some(FRONTEND),
        "the choice survives in the column"
    );

    let replaced = common::TestRequest::put(PREFERRED)
        .bearer(&token)
        .form(&[("frontend_name", SECOND_FRONTEND)])
        .send(&app)
        .await;
    assert_eq!(
        replaced.json_value(),
        json!({"frontend_name": SECOND_FRONTEND}),
        "a form body picks too, and a new name replaces the old"
    );
    assert_eq!(
        replaced.header("set-cookie"),
        Some("preferred_frontend=ghost-fe; Path=/; Max-Age=31536000"),
        "the cookie names the latest pick"
    );
    assert_eq!(
        stored(&pool, alice.id).await.preferred_frontend.as_deref(),
        Some(SECOND_FRONTEND),
        "the column follows the latest pick"
    );
}

/// A name the operator did not make pickable — and any name at all on the
/// default empty list — is 422 `{"error":"Invalid frontend"}` without a
/// write; a body without `frontend_name` is the extractor's 422.
#[tokio::test]
async fn an_unpickable_frontend_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let mut config = common::test_config();
    config.available_frontends = vec![FRONTEND.to_owned()];
    let app = common::test_app_with_config(pool.clone(), config);

    let refused = common::TestRequest::put(PREFERRED)
        .bearer(&token)
        .json(&json!({"frontend_name": SECOND_FRONTEND}))
        .send(&app)
        .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    assert_eq!(
        refused.json_value(),
        json!({"error": "Invalid frontend"}),
        "the ticket's refusal body"
    );

    let empty_body = common::TestRequest::put(PREFERRED)
        .bearer(&token)
        .json(&json!({}))
        .send(&app)
        .await;
    assert_eq!(
        empty_body.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a body without the parameter is unprocessable"
    );
    assert!(
        stored(&pool, alice.id).await.preferred_frontend.is_none(),
        "no refusal wrote the column"
    );

    let default_app = common::test_app(pool);
    assert_eq!(
        common::TestRequest::put(PREFERRED)
            .bearer(&token)
            .json(&json!({"frontend_name": FRONTEND}))
            .send(&default_app)
            .await
            .json_value(),
        json!({"error": "Invalid frontend"}),
        "with the default empty list nothing is pickable at all"
    );
}

/// Every route demands a user token; `GET` settings and the mascot ask the
/// `read:accounts` scope, `PATCH` settings and the mascot upload the
/// `write:accounts` one; the preferred-frontend pair gates on the token
/// alone. A narrower grant, an app-only credential or no credential never
/// reaches the handlers.
#[tokio::test]
async fn every_route_enforces_auth_and_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read_only) = fixture(&pool, "alice", "read:accounts").await;
    let write_only = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let wrong_scope = common::create_app_and_token(&pool, &alice, "read:chats").await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let mut config = common::test_config();
    config.available_frontends = vec![FRONTEND.to_owned()];
    let app = common::test_app_with_config(pool.clone(), config);

    let anonymous = common::TestRequest::get(&settings_path(FRONTEND))
        .send(&app)
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "no token");
    assert_eq!(
        anonymous.json_value(),
        json!({"error": "Invalid credentials"}),
        "the AuthUser no-token body"
    );
    let anonymous_mascot = common::TestRequest::get(MASCOT).send(&app).await;
    assert_eq!(anonymous_mascot.status, StatusCode::UNAUTHORIZED, "401");
    let anonymous_available = common::TestRequest::get(AVAILABLE).send(&app).await;
    assert_eq!(anonymous_available.status, StatusCode::UNAUTHORIZED, "401");
    let anonymous_pick = common::TestRequest::put(PREFERRED).send(&app).await;
    assert_eq!(anonymous_pick.status, StatusCode::UNAUTHORIZED, "401");

    let app_token = common::TestRequest::get(&settings_path(FRONTEND))
        .bearer(&app_only)
        .send(&app)
        .await;
    assert_eq!(app_token.status, StatusCode::FORBIDDEN, "an app token");
    assert_eq!(
        app_token.json_value(),
        json!({"error": "This API requires an authenticated user"}),
        "is refused the user endpoints"
    );

    let read_patches = patch_settings(&app, &read_only, FRONTEND, json!({"x": 1})).await;
    assert_eq!(
        read_patches.json_value(),
        json!({"error": "Insufficient permissions: write:accounts."}),
        "read:accounts may not patch settings"
    );
    let write_gets = get_settings(&app, &write_only, FRONTEND).await;
    assert_eq!(
        write_gets.json_value(),
        json!({"error": "Insufficient permissions: read:accounts."}),
        "write:accounts may not read them"
    );
    let write_reads_mascot = common::TestRequest::get(MASCOT)
        .bearer(&write_only)
        .send(&app)
        .await;
    assert_eq!(
        write_reads_mascot.status,
        StatusCode::FORBIDDEN,
        "and the mascot reads like the settings"
    );

    let narrow = common::TestRequest::get(MASCOT)
        .bearer(&wrong_scope)
        .send(&app)
        .await;
    assert_eq!(
        narrow.json_value(),
        json!({"error": "Insufficient permissions: read:accounts."}),
        "read:chats reaches neither of the account scopes"
    );

    let frontend_needs_no_scope = common::TestRequest::put(PREFERRED)
        .bearer(&wrong_scope)
        .json(&json!({"frontend_name": FRONTEND}))
        .send(&app)
        .await;
    assert_eq!(
        frontend_needs_no_scope.status,
        StatusCode::OK,
        "the preferred-frontend pair gates on the token alone"
    );
    assert_eq!(
        stored(&pool, alice.id).await.preferred_frontend.as_deref(),
        Some(FRONTEND),
        "and still stores the pick"
    );
}
