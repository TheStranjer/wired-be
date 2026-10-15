//! Integration tests for `PATCH /api/v1/accounts/update_credentials`
//! (WBE-67, the profile-text-fields ticket, extended by WBE-69, the
//! profile-metadata ticket, by the profile-images ticket, and by this
//! ticket's posting defaults and Pleroma settings): the fields this slice
//! of the endpoint owns — `display_name` (length-capped against
//! `MAX_DISPLAY_NAME_CHARS`, sanitized, empty stored as empty),
//! `note` (length-capped against `MAX_BIO_CHARS`, stored raw in `raw_bio`
//! and rendered to `bio` with URL, hashtag and local-mention links), the
//! `locked` / `bot` / `discoverable` booleans, `fields_attributes` (the
//! profile metadata: every body shape, the `MAX_PROFILE_FIELDS` count cap,
//! the per-entry name and value character caps, both-empty entries dropped,
//! empty clearing, raw values kept beside the rendered HTML), the three
//! profile images — `avatar`, `header` and `pleroma_background_image` file
//! parts stored through the uploader and served over `/media`, replaced
//! images whose file goes away, the empty-string and JSON-`null` clears,
//! the `AVATAR_LIMIT_BYTES` / `BANNER_LIMIT_BYTES` caps and the
//! `png`/`jpeg`/`gif`/`webp` allowlist, both 422s — and, from this ticket,
//! the posting defaults of the `source` block (`source[privacy]` as the
//! nested object or Pleroma's flat `default_scope`, `source[sensitive]`,
//! `source[language]` with its length cap and its clearing empty string),
//! the eleven Pleroma preference flags, `birthday` (set, cleared, and both
//! 422s), `pleroma_settings_store` (the shallow key-by-key merge, the
//! JSON-text spelling of a form body, the non-object and over-1-MiB
//! refusals) and `actor_type` (the `Person`/`Service` pair that keeps `bot`
//! in step) — each updated individually through JSON, form and multipart
//! bodies, with absent fields left untouched, invalid boolean strings and
//! over-long texts answered 422, the 200 answer the owner entity with its
//! `source` block and its `pleroma` block reflecting the new values (the
//! `verify_credentials` render), the route behind `AuthUser` and
//! `write:accounts` (401 unauthenticated, 403 for the wrong credential),
//! and the `POST` alias serving the old clients.

#![cfg(test)]

use axum::{Router, body::Bytes, http::StatusCode};
use chrono::{NaiveDate, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::user::{Flag, User, get_by_id},
    util::entities::{hashtag_html, mention_html},
};

use crate::common;

/// The endpoint under test.
const ROUTE: &str = "/api/v1/accounts/update_credentials";
/// The multipart boundary the test bodies are framed with.
const BOUNDARY: &str = "wired-update-credentials-boundary";
/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = TEST_BASE_URL;
/// Where the test instance serves stored files from.
const MEDIA: &str = "https://test.example/media";
/// The entity's default avatar, `Config::for_tests`' instance asset.
const DEFAULT_AVATAR: &str = "https://test.example/images/avi.png";
/// The entity's default banner, likewise.
const DEFAULT_HEADER: &str = "https://test.example/images/banner.png";
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body for a token without the endpoint's scope.
const NO_SCOPE: &str = r#"{"error":"Insufficient permissions: write:accounts."}"#;
/// A `PNG`: the signature the detector matches on. A profile image is never
/// decoded, so the header is the whole fixture (20 bytes).
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
/// A `JPEG`: the `SOI` and `APP0` markers the detector matches on (16 bytes).
const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01\x01\x00\x00\x01";
/// A `GIF`: the versioned header `GIF89a` (14 bytes).
const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00";
/// The bare 8-byte `PNG` signature: the smallest file the detector still
/// names a `png`, which the size-cap tests measure against a cap.
const SMALL_PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
/// A `WebP`: the `RIFF` container, its size and the `WEBP`/`VP8` tags
/// (16 bytes).
const WEBP: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";
/// A `BMP`: an image the detector names, and one the endpoint still
/// refuses.
const BMP: &[u8] = b"BM\x36\x00\x00\x00\x00\x00\x36\x00\x00\x00";
/// A `PDF`: bytes with a type that is no image.
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
/// Text: bytes no matcher can name at all.
const TEXT: &[u8] = b"a plain text file, definitely not an image\n";

/// `PATCH` the endpoint with `body` as JSON.
async fn patch_json(app: &Router, token: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::patch(ROUTE)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// `PATCH` the endpoint with `fields` as an urlencoded form body.
async fn patch_form(app: &Router, token: &str, fields: &[(&str, &str)]) -> common::TestResponse {
    common::TestRequest::patch(ROUTE)
        .bearer(token)
        .form(fields)
        .send(app)
        .await
}

/// `PATCH` the endpoint with a `multipart/form-data` body of text parts.
async fn patch_multipart(
    app: &Router,
    token: &str,
    parts: &[common::MultipartPart],
) -> common::TestResponse {
    common::TestRequest::patch(ROUTE)
        .bearer(token)
        .multipart(BOUNDARY, parts)
        .send(app)
        .await
}

/// A multipart text part.
fn part(name: &'static str, value: &'static str) -> common::MultipartPart {
    common::MultipartPart::text(name, value)
}

/// Assert a 200 and hand back the body.
fn ok_body(response: &common::TestResponse) -> Value {
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the update is accepted: {}",
        response.text()
    );
    response.json_value()
}

/// A user bound `write:accounts` token and the app to send it to, plus the
/// fixture user whose row the updates change.
async fn authed_app(pool: &PgPool, nickname: &str) -> (Router, String, User) {
    let alice = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &alice, "write:accounts").await;
    (common::test_app(pool.clone()), token, alice)
}

/// The stored row behind `id`, for asserting what the write left behind.
async fn stored(pool: &PgPool, id: Uuid) -> User {
    get_by_id(pool, id)
        .await
        .expect("the row read runs")
        .expect("the fixture row exists")
}

/// `display_name` updates through JSON, form and multipart bodies, each
/// time replacing the stored name and never disturbing anything else.
#[tokio::test]
async fn display_name_updates_through_every_body_type() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let via_json = ok_body(&patch_json(&app, &token, &json!({"display_name": "Ada"})).await);
    assert_eq!(
        via_json["display_name"], "Ada",
        "the JSON body answers the name"
    );
    assert_eq!(via_json["username"], "alice", "the nickname is untouched");

    let via_form = ok_body(&patch_form(&app, &token, &[("display_name", "Grace")]).await);
    assert_eq!(
        via_form["display_name"], "Grace",
        "the form body answers the name"
    );

    let via_multipart =
        ok_body(&patch_multipart(&app, &token, &[part("display_name", "Alan")]).await);
    assert_eq!(
        via_multipart["display_name"], "Alan",
        "the multipart body answers"
    );
    assert_eq!(
        source_of(&via_multipart)["note"],
        "",
        "the note was not in the body and stays as it was"
    );
    let stored = stored(&pool, alice.id).await;
    assert_eq!(stored.name, "Alan", "the write is the answer's name");
}

/// A submitted `display_name` is stored through the display-name
/// sanitizer — markup collapses to one line of plain text — and the empty
/// string is stored empty, not reset to the nickname.
#[tokio::test]
async fn display_name_is_sanitized_and_empty_is_stored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, _) = authed_app(&pool, "alice").await;
    let stripped = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"display_name": "  <b>Bob</b><br>the Builder "}),
        )
        .await,
    );
    assert_eq!(
        stripped["display_name"], "Bob the Builder",
        "tags become text and the value is trimmed"
    );

    let emptied = ok_body(&patch_json(&app, &token, &json!({"display_name": ""})).await);
    assert_eq!(
        emptied["display_name"], "",
        "the empty string is stored empty, not reset to alice"
    );
    assert_eq!(
        emptied["acct"], "alice",
        "the nickname stands beside the blank name on its own"
    );
}

/// `display_name` is capped at `MAX_DISPLAY_NAME_CHARS` characters,
/// counted as characters not bytes: 51 scalars are the 422 with the
/// ticket's wording, 50 fit, and a multibyte 50-character name fits too.
#[tokio::test]
async fn display_name_length_limit_is_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, _) = authed_app(&pool, "alice").await;
    let response = patch_json(&app, &token, &json!({"display_name": "x".repeat(51)})).await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "51 characters is over the 50-character default"
    );
    assert_eq!(
        response.text(),
        r#"{"error":"Name is too long (maximum is 50 characters)"}"#,
        "the pinned wording"
    );

    let body = ok_body(&patch_json(&app, &token, &json!({"display_name": "é".repeat(50)})).await);
    assert_eq!(
        body["display_name"],
        "é".repeat(50),
        "fifty multibyte characters count as fifty, not as one hundred and fifty bytes"
    );
}

/// A `note` updates through JSON, form and multipart bodies: the public
/// `note` renders the formatted HTML and `source.note` round-trips the
/// raw text the client sent.
#[tokio::test]
async fn note_updates_through_every_body_type() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, _) = authed_app(&pool, "alice").await;

    let via_json = ok_body(&patch_json(&app, &token, &json!({"note": "hello there"})).await);
    assert_eq!(
        via_json["note"], "<p>hello there</p>",
        "the JSON body renders"
    );
    assert_eq!(
        source_of(&via_json)["note"],
        "hello there",
        "the source block hands back the raw text"
    );

    let via_form = ok_body(&patch_form(&app, &token, &[("note", "form bio")]).await);
    assert_eq!(via_form["note"], "<p>form bio</p>", "the form body renders");

    let via_multipart = ok_body(&patch_multipart(&app, &token, &[part("note", "multi bio")]).await);
    assert_eq!(
        via_multipart["note"], "<p>multi bio</p>",
        "the multipart body renders"
    );

    let emptied = ok_body(&patch_json(&app, &token, &json!({"note": ""})).await);
    assert_eq!(emptied["note"], "", "the empty note renders empty");
    assert_eq!(source_of(&emptied)["note"], "", "and round-trips empty");
}

/// The rendered note linkifies bare URLs and hashtags, links the mentions
/// naming local users (bare or spelled with this instance's domain, case
/// folded) and leaves unknown and remote mentions as plain text.
#[tokio::test]
async fn note_links_urls_hashtags_and_local_mentions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _bob: User = common::create_local_user(&pool, "bob").await;
    let _carol: User = common::create_remote_user(&pool, "carol", "remote.example").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    let raw = "hi @bob @ALICE @nobody @carol@remote.example https://example.com/a now #Rust";
    let body = ok_body(&patch_json(&app, &token, &json!({"note": raw})).await);
    let note = body["note"].as_str().expect("the note is a string");
    assert_eq!(
        source_of(&body)["note"],
        raw,
        "the source keeps the text exactly as typed"
    );
    assert!(
        note.contains(&mention_html(&format!("{BASE}/users/bob"), "bob")),
        "the bare mention of a local user becomes its h-card: {note}"
    );
    assert!(
        note.contains(&mention_html(&format!("{BASE}/users/alice"), "ALICE")),
        "the case-folded self mention finds alice, displayed as typed: {note}"
    );
    assert!(
        !note.contains("remote.example/users/carol"),
        "a remote mention is never linked to its actor: {note}"
    );
    assert!(
        note.contains("@carol@remote.example"),
        "and the remote mention stays plain text: {note}"
    );
    assert!(
        note.contains("@nobody") && !note.contains("users/nobody"),
        "an unknown mention stays plain text: {note}"
    );
    assert!(
        note.contains("href=\"https://example.com/a\""),
        "the bare URL is linkified: {note}"
    );
    assert!(
        note.contains(&hashtag_html(BASE, "rust")),
        "the hashtag links lowercased: {note}"
    );
}

/// A mention spelled with this instance's own domain links to the local
/// user it names; a different domain never does.
#[tokio::test]
async fn note_links_own_domain_mentions_as_local() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _bob: User = common::create_local_user(&pool, "bob").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    let body = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"note": "ping @bob@test.example @bob@other.example"}),
        )
        .await,
    );
    let note = body["note"].as_str().expect("the note is a string");
    assert!(
        note.contains(&mention_html(&format!("{BASE}/users/bob"), "bob")),
        "the own-domain spelling names a local user: {note}"
    );
    assert!(
        note.contains("@bob@other.example"),
        "and the foreign one stays plain text: {note}"
    );
    assert_eq!(
        note.matches("h-card").count(),
        1,
        "exactly one of the two mentions linked: {note}"
    );
}

/// A `note` over `MAX_BIO_CHARS` characters is the 422, and the limit
/// moves with the configuration.
#[tokio::test]
async fn note_length_limit_is_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, _) = authed_app(&pool, "alice").await;
    let too_long = patch_json(&app, &token, &json!({"note": "x".repeat(5001)})).await;
    assert_eq!(
        too_long.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "5001 characters is over the 5000-character default"
    );
    assert_eq!(
        too_long.text(),
        r#"{"error":"Bio is too long (maximum is 5000 characters)"}"#,
        "the message mirrors the name's wording"
    );
    let _exactly_max = ok_body(&patch_json(&app, &token, &json!({"note": "x".repeat(5000)})).await);

    let mut config = common::test_config();
    config.max_display_name_chars = 5;
    config.max_bio_chars = 10;
    let ada = common::create_local_user(&pool, "ada").await;
    let small_token = common::create_app_and_token(&pool, &ada, "write:accounts").await;
    let small_app = common::test_app_with_config(pool.clone(), config);
    let small_name = patch_json(&small_app, &small_token, &json!({"display_name": "six!!!"})).await;
    assert_eq!(
        small_name.text(),
        r#"{"error":"Name is too long (maximum is 5 characters)"}"#,
        "MAX_DISPLAY_NAME_CHARS moves the cap and the message"
    );
    let small_bio = patch_json(&small_app, &small_token, &json!({"note": "eleven chars"})).await;
    assert_eq!(
        small_bio.text(),
        r#"{"error":"Bio is too long (maximum is 10 characters)"}"#,
        "MAX_BIO_CHARS moves the cap and the message"
    );
}

/// One field of the JSON body each boolean round sends.
fn one_flag(field: &str, value: bool) -> Value {
    let mut body = serde_json::Map::new();
    let _old: Option<Value> = body.insert(field.to_owned(), json!(value));
    Value::Object(body)
}

/// Assert the entity's three flag keys against `expect`: the running
/// stored state, so every round also proves the fields it did not send
/// stayed untouched.
fn assert_flags(body: &Value, expect: &Value, why: &str) {
    for field in ["locked", "bot", "discoverable"] {
        assert_eq!(body[field], expect[field], "{why}: {field}");
    }
}

/// Each of the three booleans updates individually through JSON, form and
/// multipart bodies — set, cleared and set again — reaching both the
/// entity and the `source.pleroma` block, and never moving the others.
#[tokio::test]
async fn boolean_flags_update_through_every_body_type() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let mut expect = json!({"locked": false, "bot": false, "discoverable": false});

    for field in ["locked", "bot", "discoverable"] {
        expect[field] = json!(true);
        let via_json = ok_body(&patch_json(&app, &token, &one_flag(field, true)).await);
        assert_flags(&via_json, &expect, "a JSON true did not stick");

        expect[field] = json!(false);
        let via_form = ok_body(&patch_form(&app, &token, &[(field, "false")]).await);
        assert_flags(&via_form, &expect, "a form false did not stick");

        expect[field] = json!(true);
        let via_multipart = ok_body(
            &patch_multipart(
                &app,
                &token,
                &[part(field, "on"), part("display_name", "Ada")],
            )
            .await,
        );
        assert_flags(&via_multipart, &expect, "a multipart on did not stick");
        assert_eq!(
            via_multipart["display_name"], "Ada",
            "the name rode along with the flag"
        );
        assert_eq!(
            source_of(&via_multipart)["pleroma"]["discoverable"],
            expect["discoverable"],
            "the source block's pleroma settings read the stored flags"
        );
    }

    let row = stored(&pool, alice.id).await;
    assert_eq!(
        (
            row.locked.get(),
            row.bot.get(),
            row.discoverable.get(),
            row.name.as_str(),
        ),
        (true, true, true, "Ada"),
        "every round left its stored mark"
    );
}

/// An unrecognized boolean string is the extractor's 422, and a body
/// carrying one writes nothing.
#[tokio::test]
async fn invalid_boolean_strings_are_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, _) = authed_app(&pool, "alice").await;
    for (field, value) in [
        ("locked", "maybe"),
        ("bot", "truthy"),
        ("discoverable", "yes-please"),
    ] {
        let response = patch_form(&app, &token, &[(field, value)]).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{field}={value}` is no boolean"
        );
        assert!(
            response
                .text()
                .starts_with(r#"{"error":"Invalid parameters"#),
            "the 422 is the extractor's wording: {}",
            response.text()
        );
    }
    let json_response = patch_json(&app, &token, &json!({"locked": "maybe"})).await;
    assert_eq!(
        json_response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the JSON spelling of a bad boolean is the same 422"
    );
    let body = ok_body(&patch_json(&app, &token, &json!({"display_name": "Ada"})).await);
    assert_eq!(
        body["locked"], false,
        "the refused bodies wrote nothing to the flags"
    );
}

/// The account every present-only assertion writes against: a name, the note
/// in both forms, the flags on, the posting defaults and Pleroma preferences
/// set, a birthday and a settings blob.
async fn prefilled(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user_with(pool, nickname, |mut new| {
        new.name = "Ada".to_owned();
        new.raw_bio = "old bio".to_owned();
        new.bio = "<p>old bio</p>".to_owned();
        new.locked = Flag::new(true);
        new.bot = Flag::new(true);
        new.discoverable = Flag::new(true);
        new.default_scope = "unlisted".to_owned();
        new.sensitive_default = Flag::new(true);
        new.language = Some("pt-BR".to_owned());
        new.hide_follows = Flag::new(true);
        new.no_rich_text = Flag::new(true);
        new.show_birthday = Flag::new(true);
        new.birthday = NaiveDate::from_ymd_opt(1990, 1, 31);
        new.settings_store = json!({"theme": "dark"});
        new
    })
    .await
}

/// The account and the router every `PATCH` of these tests writes through.
async fn prefilled_app(pool: &PgPool) -> (Router, String, User) {
    let alice = prefilled(pool, "alice").await;
    let token = common::create_app_and_token(pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());
    (app, token, alice)
}

/// Fields absent from the body keep their stored values — the flags, the
/// note, the name, the posting defaults, the Pleroma preferences, the
/// birthday and the settings store each survive an update of the others.
#[tokio::test]
async fn absent_fields_stay_untouched() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, _alice) = prefilled_app(&pool).await;

    let flipped = ok_body(&patch_json(&app, &token, &json!({"bot": false})).await);
    assert_eq!(flipped["bot"], false, "the sent flag flipped");
    assert_eq!(flipped["locked"], true, "the other flags stay");
    assert_eq!(flipped["discoverable"], true, "and stay");
    assert_eq!(flipped["display_name"], "Ada", "the name stays");
    assert_eq!(flipped["note"], "<p>old bio</p>", "the note stays");
    assert_eq!(source_of(&flipped)["note"], "old bio", "raw stays too");
    assert_eq!(
        source_of(&flipped)["privacy"],
        "unlisted",
        "the posting default the body never named stays"
    );
    assert_eq!(
        source_of(&flipped)["sensitive"],
        true,
        "and so does the sensitive default"
    );
    assert_eq!(source_of(&flipped)["language"], "pt-BR", "and the language");
    assert_eq!(
        source_of(&flipped)["pleroma"]["no_rich_text"],
        true,
        "and the pleroma flags"
    );
    assert_eq!(
        flipped["pleroma"]["hide_follows"], true,
        "the follow-list flag included"
    );
    assert_eq!(
        flipped["pleroma"]["birthday"], "1990-01-31",
        "the birthday survives a flag write"
    );
    assert_eq!(
        flipped["pleroma"]["settings_store"],
        json!({"theme": "dark"}),
        "and the stored client settings"
    );

    let renamed = ok_body(&patch_json(&app, &token, &json!({"display_name": "Grace"})).await);
    assert_eq!(renamed["display_name"], "Grace", "the name updated");
    assert_eq!(renamed["locked"], true, "flags survive a name change");
    assert_eq!(
        renamed["note"], "<p>old bio</p>",
        "the note survives a name change"
    );
    assert_eq!(
        renamed["pleroma"]["show_birthday"], true,
        "the preference flags survive a name change"
    );
}

/// The other two spellings of absence: a body of JSON nulls (Pleroma's nil
/// filter reads them as absent) and a bodyless `PATCH`, neither of which
/// changes anything — not the answer, not the row.
#[tokio::test]
async fn nulls_and_an_empty_body_write_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = prefilled_app(&pool).await;
    let flipped = ok_body(&patch_json(&app, &token, &json!({"bot": false})).await);
    assert_eq!(flipped["bot"], false, "the sent flag flipped");
    let renamed = ok_body(&patch_json(&app, &token, &json!({"display_name": "Grace"})).await);
    assert_eq!(renamed["display_name"], "Grace", "the name updated");

    let nulled = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({
                "display_name": null,
                "note": null,
                "locked": null,
                "source": null,
                "birthday": null,
                "pleroma_settings_store": null,
                "actor_type": null,
                "hide_followers": null,
                "default_scope": null,
            }),
        )
        .await,
    );
    assert_eq!(
        nulled["display_name"], "Grace",
        "a JSON null reads as absent, Pleroma's nil filter"
    );
    assert_eq!(nulled["locked"], true, "and so for the booleans");
    assert_eq!(
        nulled["pleroma"]["birthday"], "1990-01-31",
        "a null birthday clears nothing"
    );
    assert_eq!(
        nulled["pleroma"]["settings_store"],
        json!({"theme": "dark"}),
        "and a null settings store merges nothing"
    );
    assert_eq!(
        nulled["pleroma"]["hide_followers"], false,
        "and a null flag writes nothing"
    );
    assert_eq!(
        source_of(&nulled)["pleroma"]["actor_type"],
        "Person",
        "and a null actor type keeps the stored kind"
    );

    let empty = common::TestRequest::patch(ROUTE)
        .bearer(&token)
        .send(&app)
        .await;
    let noop = ok_body(&empty);
    assert_eq!(
        noop["display_name"], "Grace",
        "a bodyless PATCH is the no-op"
    );
    assert_eq!(noop["bot"], false, "and changes nothing");

    let row = stored(&pool, alice.id).await;
    assert_eq!(row.raw_bio, "old bio", "the stored raw note never moved");
    assert!(
        row.discoverable.get(),
        "the stored flags are the answered ones"
    );
}

/// The 200 is the full owner entity: the `source` block
/// `verify_credentials` answers (raw note, posting defaults, the pending
/// queue, the Pleroma settings) and the top-level `also_known_as`.
#[tokio::test]
async fn response_carries_the_verify_credentials_source() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.default_scope = "private".to_owned();
        new.language = Some("la".to_owned());
        new.also_known_as = vec!["https://old.example/users/alice".to_owned()];
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool);
    let body = ok_body(&patch_json(&app, &token, &json!({"note": "hi"})).await);
    assert_eq!(
        source_of(&body),
        json!({
            "privacy": "private",
            "sensitive": false,
            "language": "la",
            "note": "hi",
            "fields": [],
            "follow_requests_count": 0,
            "pleroma": {
                "show_role": true,
                "no_rich_text": false,
                "discoverable": false,
                "actor_type": "Person",
            },
        }),
        "the source block is the verify_credentials builder over the fresh row"
    );
    assert_eq!(
        body["also_known_as"],
        json!(["https://old.example/users/alice"]),
        "the owner-view top-level keys are present as they are on verify_credentials"
    );
}

/// The route runs `AuthUser` with `write:accounts`: no or invalid token is
/// the 401, an app-only credential and a scope-less user token the 403,
/// and none of them touches the row.
#[tokio::test]
async fn auth_and_scope_gate_the_write() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    let no_token = common::TestRequest::patch(ROUTE)
        .json(&json!({
            "display_name": "Nobody",
            "source": {"privacy": "private", "language": "de"},
            "birthday": "1990-01-31",
            "pleroma_settings_store": {"theme": "light"},
            "hide_followers": true,
        }))
        .send(&app)
        .await;
    assert_eq!(no_token.status, StatusCode::UNAUTHORIZED, "no token");
    assert_eq!(no_token.text(), NO_TOKEN, "the 401 body");

    let bad_token = common::TestRequest::patch(ROUTE)
        .bearer("not-a-token")
        .json(&json!({"display_name": "Nobody"}))
        .send(&app)
        .await;
    assert_eq!(bad_token.status, StatusCode::UNAUTHORIZED, "invalid token");
    assert_eq!(bad_token.text(), INVALID_TOKEN, "the 401 body");

    let app_only = common::create_app_only_token(&pool, "write:accounts").await;
    let app_only_response = patch_json(&app, &app_only, &json!({"display_name": "Nobody"})).await;
    assert_eq!(
        app_only_response.status,
        StatusCode::FORBIDDEN,
        "app-only token"
    );
    assert_eq!(app_only_response.text(), USER_REQUIRED, "the 403 body");

    let reader = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let wrong_scope = patch_json(&app, &reader, &json!({"display_name": "Nobody"})).await;
    assert_eq!(
        wrong_scope.status,
        StatusCode::FORBIDDEN,
        "a user token without write:accounts"
    );
    assert_eq!(wrong_scope.text(), NO_SCOPE, "the 403 names the scope");

    let row = stored(&pool, alice.id).await;
    assert_eq!(row.name, "", "no rejection wrote anything");
    assert_eq!(
        row.default_scope, "public",
        "and none of the new columns either"
    );
    assert!(row.birthday.is_none(), "no birthday");
    assert_eq!(
        row.settings_store,
        json!({}),
        "no settings store from the unauthenticated body"
    );
}

/// Old clients `POST` the same body to the same path and get the same
/// answer as the `PATCH`ers.
#[tokio::test]
async fn post_alias_updates_the_same_fields() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, _) = authed_app(&pool, "alice").await;
    let form_response = common::TestRequest::post(ROUTE)
        .bearer(&token)
        .form(&[("display_name", "Ada"), ("discoverable", "true")])
        .send(&app)
        .await;
    let form_body = ok_body(&form_response);
    assert_eq!(form_body["display_name"], "Ada", "POST carries a form body");
    assert_eq!(form_body["discoverable"], true, "and the flags ride along");

    let multi_response = common::TestRequest::post(ROUTE)
        .bearer(&token)
        .multipart(BOUNDARY, &[part("note", "posted bio")])
        .send(&app)
        .await;
    let multi_body = ok_body(&multi_response);
    assert_eq!(
        multi_body["note"], "<p>posted bio</p>",
        "POST takes multipart bodies too"
    );
    assert_eq!(
        multi_body["display_name"], "Ada",
        "and the merge rules are shared"
    );
}

/// A profile image and the text fields ride along in one multipart body:
/// the upload writes its column and the name beside it, and nothing else
/// moves. The earlier slice of this endpoint ignored file parts altogether;
/// the image ticket took them over, and this is the test that pinned that
/// behaviour, rewritten to the one that holds now.
#[tokio::test]
async fn an_avatar_upload_rides_along_with_the_text_fields() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let body = ok_body(
        &patch_multipart(
            &app,
            &token,
            &[
                part("display_name", "Ada"),
                file_part("avatar", PNG),
                part("avatar_description", "alt text the endpoint ignores"),
                part("header_description", "and so this one"),
            ],
        )
        .await,
    );
    assert_eq!(body["display_name"], "Ada", "the text field applied");
    assert_eq!(body["note"], "", "the note stayed as it was");
    let avatar = url_of(&body["avatar"]);
    assert!(
        avatar.starts_with(&format!("{MEDIA}/{}/", alice.id)),
        "the avatar is the caller's own stored file: {avatar}"
    );
    assert!(
        extension_of(&avatar) == "png",
        "stored as the png it is: {avatar}"
    );
    assert_eq!(
        body["header"], DEFAULT_HEADER,
        "the banner the body did not send stays the default"
    );
    assert_eq!(
        body["pleroma"]["background_image"],
        Value::Null,
        "and so does the background"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.avatar_url.as_deref(), Some(avatar.as_str()), "stored");
    assert_eq!(
        served(&dir, &avatar),
        Some(PNG.to_vec()),
        "the announced URL serves the uploaded bytes"
    );
}

/// A file part under a field this endpoint does not read is still ignored,
/// and stores nothing.
#[tokio::test]
async fn a_file_part_under_another_name_is_ignored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, _) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let body = ok_body(
        &patch_multipart(
            &app,
            &token,
            &[
                part("display_name", "Ada"),
                common::MultipartPart::file(
                    "file",
                    "cat.png",
                    "image/png",
                    Bytes::from_static(PNG),
                ),
            ],
        )
        .await,
    );
    assert_eq!(body["display_name"], "Ada", "the text field applied");
    assert_eq!(
        body["avatar"], DEFAULT_AVATAR,
        "the avatar stays the instance default"
    );
    assert_eq!(stored_files(&dir), 0, "and nothing was stored");
}

/// One profile metadata entry as the entity answers it.
fn field(name: &str, value: &str) -> Value {
    json!({"name": name, "value": value, "verified_at": null})
}

/// `fields_attributes` updates through the JSON array, the JSON object keyed
/// by index, and the `fields_attributes[0][name]` spelling of a form body.
/// Each round answers the top-level `fields` with the rendered HTML and
/// `source.fields` with the raw text; `fields_attributes_arrive_through_multipart`
/// covers the multipart spelling and the pair of jsonb columns left behind.
#[tokio::test]
async fn fields_attributes_update_through_every_body_shape() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, _alice) = authed_app(&pool, "alice").await;

    let section = json!({"fields_attributes": [
        {"name": "Site", "value": "https://example.com/a"},
        {"name": "Motto", "value": "beep boop"},
    ]});
    let via_json = ok_body(&patch_json(&app, &token, &section).await);
    assert_eq!(
        via_json["fields"],
        json!([
            field(
                "Site",
                "<p><a href=\"https://example.com/a\" rel=\"ugc nofollow noopener noreferrer\">example.com/a</a></p>"
            ),
            field("Motto", "<p>beep boop</p>"),
        ]),
        "the JSON array renders: bare URL linked with the standard rel, plain text paragraphed"
    );
    assert_eq!(
        source_of(&via_json)["fields"],
        json!([
            field("Site", "https://example.com/a"),
            field("Motto", "beep boop"),
        ]),
        "the source block hands back exactly what was typed"
    );

    let via_index_map = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": {
                "1": {"name": "second", "value": "two"},
                "0": {"name": "first", "value": "one"},
            }}),
        )
        .await,
    );
    assert_eq!(
        via_index_map["fields"],
        json!([field("first", "<p>one</p>"), field("second", "<p>two</p>")]),
        "the index-object shape orders by the index as a number"
    );

    let via_form = ok_body(
        &patch_form(
            &app,
            &token,
            &[
                ("fields_attributes[0][name]", "Form"),
                ("fields_attributes[0][value]", "https://form.example/x"),
                ("fields_attributes[1][name]", "Bare"),
                ("fields_attributes[1][value]", "just words"),
            ],
        )
        .await,
    );
    assert_eq!(
        via_form["fields"],
        json!([
            field(
                "Form",
                "<p><a href=\"https://form.example/x\" rel=\"ugc nofollow noopener noreferrer\">form.example/x</a></p>"
            ),
            field("Bare", "<p>just words</p>"),
        ]),
        "the form's bracketed index keys fold into the same list"
    );
    assert_eq!(
        source_of(&via_form)["fields"],
        json!([
            field("Form", "https://form.example/x"),
            field("Bare", "just words")
        ]),
        "and the raw text round-trips the same"
    );
}

/// The multipart spelling of the same write, and the pair of jsonb columns it
/// leaves behind: `fields` rendered, `raw_fields` verbatim.
#[tokio::test]
async fn fields_attributes_arrive_through_multipart() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let via_multipart = ok_body(
        &patch_multipart(
            &app,
            &token,
            &[
                part("fields_attributes[0][name]", "Multi"),
                part("fields_attributes[0][value]", "hello"),
            ],
        )
        .await,
    );
    assert_eq!(
        via_multipart["fields"],
        json!([field("Multi", "<p>hello</p>")]),
        "multipart text parts structure like urlencoded ones"
    );

    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.fields,
        json!([{"name": "Multi", "value": "<p>hello</p>"}]),
        "the `fields` column holds the rendered pair"
    );
    assert_eq!(
        row.raw_fields,
        json!([{"name": "Multi", "value": "hello"}]),
        "the `raw_fields` column holds the typed pair"
    );
}

/// A submitted field name is stored as plain text — markup is stripped —
/// while `raw_fields` keeps the spelling as typed, and `source.fields`
/// round-trips that raw spelling so the client's textarea sees what it
/// wrote.
#[tokio::test]
async fn field_names_are_stored_as_plain_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let body = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [{"name": "<b>Site</b>", "value": "https://example.com"}]}),
        )
        .await,
    );
    assert_eq!(
        body["fields"][0]["name"], "Site",
        "the rendered name is stripped to text"
    );
    assert_eq!(
        source_of(&body)["fields"][0]["name"],
        "<b>Site</b>",
        "the source keeps the name as typed"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.raw_fields,
        json!([{"name": "<b>Site</b>", "value": "https://example.com"}]),
        "raw_fields round-trips both halves verbatim"
    );
}

/// At most `MAX_PROFILE_FIELDS` entries are stored: the fourth fits, the
/// fifth is the 422 "Too many fields" that writes nothing, entries dropped
/// as empty do not count toward the cap, and the cap moves with the
/// configuration.
#[tokio::test]
async fn too_many_fields_is_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let fourth = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [
                {"name": "a", "value": "1"},
                {"name": "b", "value": "2"},
                {"name": "c", "value": "3"},
                {"name": "d", "value": "4"},
            ]}),
        )
        .await,
    );
    assert_eq!(
        fourth["fields"].as_array().map(Vec::len),
        Some(4),
        "four entries fit the default cap"
    );

    let fifth = patch_json(
        &app,
        &token,
        &json!({"fields_attributes": [
            {"name": "a", "value": "1"},
            {"name": "b", "value": "2"},
            {"name": "c", "value": "3"},
            {"name": "d", "value": "4"},
            {"name": "e", "value": "5"},
        ]}),
    )
    .await;
    assert_eq!(
        fifth.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "five is over the default cap"
    );
    assert_eq!(
        fifth.text(),
        r#"{"error":"Too many fields"}"#,
        "the pinned wording"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.fields,
        json!([
            {"name": "a", "value": "<p>1</p>"},
            {"name": "b", "value": "<p>2</p>"},
            {"name": "c", "value": "<p>3</p>"},
            {"name": "d", "value": "<p>4</p>"},
        ]),
        "the refused write changed nothing"
    );

    let with_empties = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [
                {"name": "a", "value": "1"},
                {"name": "", "value": ""},
                {},
                {"name": "b", "value": "2"},
                {"name": "c", "value": "3"},
                {"name": "d", "value": "4"},
            ]}),
        )
        .await,
    );
    assert_eq!(
        with_empties["fields"].as_array().map(Vec::len),
        Some(4),
        "dropped empties do not count toward the cap"
    );
}

/// The cap is a setting: `MAX_PROFILE_FIELDS` moves it, and a body over the
/// smaller cap is the same 422 that writes nothing.
#[tokio::test]
async fn the_field_cap_follows_the_configuration() {
    let db = common::test_db().await;
    let pool = db.pool().clone();

    let mut config = common::test_config();
    config.max_profile_fields = 2;
    let ada = common::create_local_user(&pool, "ada").await;
    let small_token = common::create_app_and_token(&pool, &ada, "write:accounts").await;
    let small_app = common::test_app_with_config(pool.clone(), config);
    let third = patch_json(
        &small_app,
        &small_token,
        &json!({"fields_attributes": [
            {"name": "a", "value": "1"},
            {"name": "b", "value": "2"},
            {"name": "c", "value": "3"},
        ]}),
    )
    .await;
    assert_eq!(
        third.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "MAX_PROFILE_FIELDS moves the cap"
    );
    assert_eq!(
        third.text(),
        r#"{"error":"Too many fields"}"#,
        "same wording"
    );
}

/// A field name over 255 characters and a field value over 2047 are the
/// 422s, counted in characters not bytes; exactly the maxima fit, and a
/// refused body writes nothing.
#[tokio::test]
async fn field_name_and_value_length_limits_are_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let long_name = patch_json(
        &app,
        &token,
        &json!({"fields_attributes": [{"name": "x".repeat(256), "value": "ok"}]}),
    )
    .await;
    assert_eq!(
        long_name.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "256 characters is over the name cap"
    );
    assert_eq!(
        long_name.text(),
        r#"{"error":"Field name is too long (maximum is 255 characters)"}"#,
        "the message mirrors the profile wording"
    );

    let long_value = patch_json(
        &app,
        &token,
        &json!({"fields_attributes": [{"name": "ok", "value": "é".repeat(2048)}]}),
    )
    .await;
    assert_eq!(
        long_value.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "2048 characters is over the value cap, in characters not bytes"
    );
    assert_eq!(
        long_value.text(),
        r#"{"error":"Field value is too long (maximum is 2047 characters)"}"#,
        "the value message spells the cap out"
    );

    let at_max = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [{"name": "x".repeat(255), "value": "y".repeat(2047)}]}),
        )
        .await,
    );
    assert_eq!(
        at_max["fields"].as_array().map(Vec::len),
        Some(1),
        "the maxima themselves fit"
    );

    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.fields,
        json!([{"name": "x".repeat(255), "value": format!("<p>{}</p>", "y".repeat(2047))}]),
        "only the accepted write is stored"
    );
    assert_eq!(
        row.raw_fields[0]["value"],
        "y".repeat(2047),
        "raw kept as typed"
    );
}

/// An empty `fields_attributes` — the JSON array and the form index map
/// alike — clears every stored field from both columns and both renders;
/// its absence leaves them alone.
#[tokio::test]
async fn empty_fields_attributes_clears_the_metadata() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let filled = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [{"name": "Site", "value": "https://example.com"}]}),
        )
        .await,
    );
    assert_eq!(filled["fields"].as_array().map(Vec::len), Some(1));

    let cleared = ok_body(&patch_json(&app, &token, &json!({"fields_attributes": []})).await);
    assert_eq!(cleared["fields"], json!([]), "the entity answers none");
    assert_eq!(
        source_of(&cleared)["fields"],
        json!([]),
        "and neither does the source block"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.fields, json!([]), "the rendered column is empty");
    assert_eq!(row.raw_fields, json!([]), "and the raw column");

    let refilled = ok_body(
        &patch_form(
            &app,
            &token,
            &[
                ("fields_attributes[0][name]", "Back"),
                ("fields_attributes[0][value]", "again"),
            ],
        )
        .await,
    );
    assert_eq!(
        refilled["fields"],
        json!([field("Back", "<p>again</p>")]),
        "the metadata can be written again after clearing"
    );
    let empty_map = ok_body(&patch_json(&app, &token, &json!({"fields_attributes": {}})).await);
    assert_eq!(
        empty_map["fields"],
        json!([]),
        "the empty index-object clears like the empty array"
    );

    let untouched = ok_body(&patch_json(&app, &token, &json!({"display_name": "Ada"})).await);
    assert_eq!(
        untouched["fields"],
        json!([]),
        "a body without fields_attributes leaves the (now empty) metadata alone"
    );
}

/// An entry whose name and value are both empty is dropped rather than
/// stored; one with only a name or only a value is kept, so a form that
/// half-fills a row still writes it.
#[tokio::test]
async fn entries_with_both_empty_name_and_value_are_dropped() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let body = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [
                {"name": "", "value": ""},
                {},
                "not even an object",
                {"name": "NameOnly", "value": ""},
                {"value": "value only"},
                {"name": "Kept", "value": "x"},
            ]}),
        )
        .await,
    );
    assert_eq!(
        body["fields"],
        json!([
            field("NameOnly", ""),
            field("", "<p>value only</p>"),
            field("Kept", "<p>x</p>"),
        ]),
        "both-empty entries (and non-objects, which read as empty) go, half-entries stay in order"
    );
    assert_eq!(
        source_of(&body)["fields"],
        json!([
            field("NameOnly", ""),
            field("", "value only"),
            field("Kept", "x"),
        ]),
        "the source block sees the same list raw"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.raw_fields,
        json!([
            {"name": "NameOnly", "value": ""},
            {"name": "", "value": "value only"},
            {"name": "Kept", "value": "x"},
        ]),
        "and it is exactly what the two columns store"
    );
}

/// An all-empty submission drops to nothing, which is the clear: the
/// stored metadata is replaced by the empty pair, not left behind.
#[tokio::test]
async fn an_all_empty_submission_clears_like_the_empty_array() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let filled = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [{"name": "Site", "value": "https://example.com"}]}),
        )
        .await,
    );
    assert_eq!(filled["fields"].as_array().map(Vec::len), Some(1));

    let cleared = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [{"name": "", "value": ""}, {}]}),
        )
        .await,
    );
    assert_eq!(
        cleared["fields"],
        json!([]),
        "every entry dropped leaves the empty list"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.fields, json!([]), "stored rendered");
    assert_eq!(row.raw_fields, json!([]), "stored raw");
}

/// The `source` object of a rendered account, present or panicked over:
/// the tests that ask for it are the ones asserting its shape.
fn source_of(body: &Value) -> Value {
    body.get("source")
        .cloned()
        .expect("update_credentials renders the source block")
}

/// A throwaway uploads directory, removed when the returned guard drops:
/// profile-image tests store real files and must not write into the
/// repository's own `./uploads`.
fn uploads_dir() -> TempDir {
    TempDir::new().expect("temp dir for uploads")
}

/// A user bound `write:accounts` token, the app that serves it out of
/// `dir`, and the fixture user whose row the updates change.
async fn authed_app_with_uploads(
    pool: &PgPool,
    nickname: &str,
    dir: &TempDir,
) -> (Router, String, User) {
    let alice = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &alice, "write:accounts").await;
    (
        common::test_app_with_uploads_dir(pool.clone(), dir.path()),
        token,
        alice,
    )
}

/// An image file part uploaded under `field`. The client always claims
/// `image/png` and names the part `picture.png`: what is stored comes from
/// the bytes, so a test that wants to prove that (and one that wants to see
/// the claim refused) can reuse this one builder.
fn file_part(field: &'static str, bytes: &[u8]) -> common::MultipartPart {
    common::MultipartPart::file(
        field,
        "picture.png",
        "image/png",
        Bytes::copy_from_slice(bytes),
    )
}

/// A `PATCH` multipart body of one image part and nothing else.
async fn upload(
    app: &Router,
    token: &str,
    field: &'static str,
    bytes: &[u8],
) -> common::TestResponse {
    patch_multipart(app, token, &[file_part(field, bytes)]).await
}

/// The URL of a string value, for comparing against what the row stored.
///
/// # Panics
///
/// If the value is not a string (an entity that carried no URL at all).
fn url_of(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or_else(|| panic!("{value} is not a URL string"))
        .to_owned()
}

/// The uploads key a media URL names: the part below [`MEDIA`].
///
/// # Panics
///
/// If the URL is not served from [`MEDIA`].
fn key_of(url: &str) -> String {
    url.strip_prefix(&format!("{MEDIA}/"))
        .unwrap_or_else(|| panic!("{url} is not served from {MEDIA}"))
        .to_owned()
}

/// Where the file behind a media URL lives inside `dir`.
fn path_of(dir: &TempDir, url: &str) -> PathBuf {
    dir.path().join(key_of(url))
}

/// The bytes the uploads directory holds behind a media URL, or `None` when
/// no file is there (anymore).
fn served(dir: &TempDir, url: &str) -> Option<Vec<u8>> {
    std::fs::read(path_of(dir, url)).ok()
}

/// Every file under `dir`, counted: zero is an empty uploads directory, and
/// a profile-image write that stored one file moves it by one.
fn stored_files(dir: &TempDir) -> usize {
    count_files(dir.path())
}

/// The files under `path`, recursively (the uploader namespaces them by
/// user id, so they are never directly in the root).
fn count_files(path: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let child = entry.path();
            if child.is_dir() {
                count_files(&child)
            } else {
                1
            }
        })
        .sum()
}

/// Assert a 422 with the exact error body, and name it for the caller.
fn assert_422(response: &common::TestResponse, message: &str, why: &str) {
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{why}: {}",
        response.text()
    );
    assert_eq!(
        response.text(),
        format!(r#"{{"error":"{message}"}}"#),
        "{why}"
    );
}

/// The three profile images upload through one multipart body: each column
/// holds the public URL of its own stored file, the entity renders them in
/// `avatar`, `header` and `pleroma.background_image`, and `/media` serves
/// every one of them with the bytes as sent.
#[tokio::test]
async fn the_three_profile_images_upload_and_are_served() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let body = ok_body(
        &patch_multipart(
            &app,
            &token,
            &[
                file_part("avatar", PNG),
                file_part("header", JPEG),
                file_part("pleroma_background_image", GIF),
            ],
        )
        .await,
    );
    let avatar = url_of(&body["avatar"]);
    let header = url_of(&body["header"]);
    let background = url_of(&body["pleroma"]["background_image"]);
    assert_eq!(body["avatar_static"], body["avatar"], "the static pair");
    assert_eq!(body["header_static"], body["header"], "is the same URL");
    assert_ne!(avatar, header, "and each slot got its own file");

    let stored_rows = [
        ("avatar", &avatar, "png", PNG, "image/png"),
        ("header", &header, "jpg", JPEG, "image/jpeg"),
        ("background", &background, "gif", GIF, "image/gif"),
    ];
    for (name, url, extension, bytes, mime) in stored_rows {
        assert!(
            url.starts_with(&format!("{MEDIA}/{}/", alice.id)),
            "{name} is namespaced to the uploader: {url}"
        );
        assert_eq!(extension_of(url), extension, "{name} keeps its type");
        assert_eq!(
            served(&dir, url).as_deref(),
            Some(bytes),
            "{name} is on disk under its key"
        );
        let response = common::TestRequest::get(&format!("/media/{}", key_of(url)))
            .send(&app)
            .await;
        assert_eq!(response.status, StatusCode::OK, "{name} is served");
        assert_eq!(
            response.header("content-type"),
            Some(mime),
            "{name} serves as the type its bytes are"
        );
        assert_eq!(response.body.as_ref(), bytes, "the served {name} bytes");
    }
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.avatar_url.as_deref(), Some(avatar.as_str()), "stored");
    assert_eq!(row.banner_url.as_deref(), Some(header.as_str()), "stored");
    assert_eq!(
        row.background_image_url.as_deref(),
        Some(background.as_str()),
        "stored"
    );
}

/// `webp` is the fourth accepted image type, and the endpoint accepts it for
/// the avatar: the stored key carries the extension the detected type gives.
#[tokio::test]
async fn a_webp_avatar_uploads_like_the_others() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let body = ok_body(&upload(&app, &token, "avatar", WEBP).await);
    let avatar = url_of(&body["avatar"]);
    assert_eq!(extension_of(&avatar), "webp", "stored as a webp");
    assert_eq!(served(&dir, &avatar).as_deref(), Some(WEBP), "the bytes");
    assert_eq!(
        stored(&pool, alice.id).await.banner_url,
        None,
        "the slot the body did not send stays empty"
    );
}

/// A replacement stores a new key and removes the file the column named
/// before it; the other two images are untouched, file and all.
#[tokio::test]
async fn replacing_an_image_deletes_the_previous_file() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let first = url_of(&ok_body(&upload(&app, &token, "avatar", PNG).await)["avatar"]);
    let header = url_of(&ok_body(&upload(&app, &token, "header", JPEG).await)["header"]);
    assert_eq!(stored_files(&dir), 2, "avatar and banner on disk");

    let second = url_of(&ok_body(&upload(&app, &token, "avatar", WEBP).await)["avatar"]);
    assert_ne!(first, second, "a replacement mints a fresh key");
    assert_eq!(served(&dir, &second).as_deref(), Some(WEBP), "the new file");
    assert_eq!(served(&dir, &first), None, "the replaced file is gone");
    assert_eq!(
        served(&dir, &header).as_deref(),
        Some(JPEG),
        "and the image nobody touched stays where it was"
    );
    assert_eq!(stored_files(&dir), 2, "one in, one out");
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.avatar_url.as_deref(), Some(second.as_str()), "stored");
    assert_eq!(row.banner_url.as_deref(), Some(header.as_str()), "stored");
}

/// Both spellings of the reset clear their column, answer the instance
/// default again, and remove the file the column had named: the empty
/// string through a form and a multipart text part, the `null` through
/// JSON.
#[tokio::test]
async fn clearing_an_image_resets_the_entity_and_deletes_the_file() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let avatar = url_of(&ok_body(&upload(&app, &token, "avatar", PNG).await)["avatar"]);
    let header = url_of(&ok_body(&upload(&app, &token, "header", JPEG).await)["header"]);
    let background = url_of(
        &ok_body(&upload(&app, &token, "pleroma_background_image", GIF).await)["pleroma"]["background_image"],
    );
    assert_eq!(stored_files(&dir), 3, "all three images on disk");

    let cleared = ok_body(&patch_form(&app, &token, &[("avatar", "")]).await);
    assert_eq!(
        cleared["avatar"], DEFAULT_AVATAR,
        "the empty form string resets the entity"
    );
    assert_eq!(served(&dir, &avatar), None, "and its file goes with it");

    let nulled = ok_body(&patch_json(&app, &token, &json!({"header": null})).await);
    assert_eq!(
        nulled["header"], DEFAULT_HEADER,
        "a JSON null resets the banner"
    );
    assert_eq!(served(&dir, &header), None, "and deletes its file");

    let text_part =
        ok_body(&patch_multipart(&app, &token, &[part("pleroma_background_image", "")]).await);
    assert_eq!(
        text_part["pleroma"]["background_image"],
        Value::Null,
        "an empty multipart text part clears the background too"
    );
    assert_eq!(served(&dir, &background), None, "and deletes that file");

    assert_eq!(stored_files(&dir), 0, "every replaced file is gone");
    let row = stored(&pool, alice.id).await;
    assert!(
        row.avatar_url.is_none() && row.banner_url.is_none() && row.background_image_url.is_none(),
        "all three columns are NULL again: {row:?}"
    );

    // And the slot can be filled once more after the clear.
    let again = url_of(&ok_body(&upload(&app, &token, "avatar", GIF).await)["avatar"]);
    assert_eq!(
        served(&dir, &again).as_deref(),
        Some(GIF),
        "a new upload works"
    );
}

/// Clearing an image that was never set is the accepted no-op, not an
/// error, and it leaves a stored image of another slot alone.
#[tokio::test]
async fn clearing_an_unset_image_changes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, _) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let header = url_of(&ok_body(&upload(&app, &token, "header", JPEG).await)["header"]);
    let body = ok_body(&patch_json(&app, &token, &json!({"avatar": null})).await);
    assert_eq!(body["avatar"], DEFAULT_AVATAR, "the avatar stays unset");
    assert_eq!(
        body["header"], header,
        "an absent slot keeps the stored image"
    );
    assert_eq!(stored_files(&dir), 1, "and no file was deleted");
}

/// The two byte caps: the avatar is measured by `AVATAR_LIMIT_BYTES`, the
/// banner and the background by `BANNER_LIMIT_BYTES`, and passing either is
/// the 422 "File is too large" that stores nothing.
#[tokio::test]
async fn an_oversize_profile_image_is_the_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    // The fixtures are 12 to 20 bytes, so the caps below are within reach of
    // every one of them and the general `UPLOAD_LIMIT_BYTES` never fires.
    let mut config = common::test_config_with_uploads_dir(dir.path());
    config.avatar_limit_bytes = 16;
    config.banner_limit_bytes = 14;
    let app = common::test_app_with_config(pool.clone(), config);

    // [`PNG`] is 20 bytes, over both caps; [`GIF`] is 14, within the banner's.
    for field in ["avatar", "header", "pleroma_background_image"] {
        let response = upload(&app, &token, field, PNG).await;
        assert_422(
            &response,
            "File is too large",
            &format!("{field} is over its cap"),
        );
    }
    assert_eq!(stored_files(&dir), 0, "a refused upload stores nothing");
    let row = stored(&pool, alice.id).await;
    assert!(
        row.avatar_url.is_none() && row.banner_url.is_none() && row.background_image_url.is_none(),
        "and writes nothing: {row:?}"
    );

    let banner = ok_body(&upload(&app, &token, "header", GIF).await);
    assert_eq!(
        extension_of(&url_of(&banner["header"])),
        "gif",
        "BANNER_LIMIT_BYTES is exactly what measures the banner"
    );

    // The avatar's own cap is the other number: the 14-byte [`GIF`] the
    // banner above accepted is over an 8-byte avatar cap, while the 8-byte
    // PNG signature fits it exactly.
    let mut strict = common::test_config_with_uploads_dir(dir.path());
    strict.avatar_limit_bytes = SMALL_PNG.len();
    let strict_app = common::test_app_with_config(pool.clone(), strict);
    let response = upload(&strict_app, &token, "avatar", GIF).await;
    assert_422(
        &response,
        "File is too large",
        "AVATAR_LIMIT_BYTES caps the avatar on its own number",
    );
    let avatar = ok_body(&upload(&strict_app, &token, "avatar", SMALL_PNG).await);
    assert_eq!(
        extension_of(&url_of(&avatar["avatar"])),
        "png",
        "a file exactly at the cap is accepted"
    );
}

/// The type allowlist: a `BMP` and a `PDF` are both bytes a matcher names and
/// neither is a profile image, so both are the 422 "Unsupported file type"
/// that stores nothing.
#[tokio::test]
async fn a_wrong_image_type_is_the_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    for bytes in [BMP, PDF] {
        for field in ["avatar", "header", "pleroma_background_image"] {
            let response = upload(&app, &token, field, bytes).await;
            assert_422(
                &response,
                "Unsupported file type",
                &format!("{field} cannot be {}", hex_head(bytes)),
            );
        }
    }
    assert_eq!(stored_files(&dir), 0, "a refused upload stores nothing");
    assert!(
        stored(&pool, alice.id).await.avatar_url.is_none(),
        "and writes nothing"
    );
}

/// The extension of a stored media URL's key: the part after the last dot of
/// its name.
///
/// # Panics
///
/// If the key has no dot, which a generated key always has.
fn extension_of(url: &str) -> String {
    let key = key_of(url);
    let (_, extension) = key
        .rsplit_once('.')
        .unwrap_or_else(|| panic!("{url} is not a named key"));
    extension.to_owned()
}

/// The first bytes of a fixture, spelled out for a failure message.
fn hex_head(bytes: &[u8]) -> String {
    let head: Vec<String> = bytes
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02X}"))
        .collect();
    format!("{}-byte fixture ({})", bytes.len(), head.join(" "))
}

/// Non-image bytes under an `image/png` part header: the claim is what the
/// client says, the answer is what the bytes are, so this is the same 422 as
/// any other non-image — and an empty part with it.
#[tokio::test]
async fn non_image_bytes_with_an_image_content_type_are_the_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, _) = authed_app_with_uploads(&pool, "alice", &dir).await;
    for bytes in [TEXT, PDF, b""] {
        // Every fixture from [`file_part`] arrives as `image/png`, named
        // `.png`: neither the claim nor the name carries the decision.
        let response = upload(&app, &token, "avatar", bytes).await;
        assert_422(
            &response,
            "Unsupported file type",
            &format!("{} bytes claimed image/png", bytes.len()),
        );
    }
    assert_eq!(stored_files(&dir), 0, "nothing reached storage");
    let response = upload(&app, &token, "header", TEXT).await;
    assert_422(&response, "Unsupported file type", "the banner checks too");
}

/// One bad image in a body with a good one refuses the whole request: every
/// slot is checked before any file is stored, so the good avatar never
/// reaches disk and the row never moves.
#[tokio::test]
async fn a_body_with_one_bad_image_stores_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let response = patch_multipart(
        &app,
        &token,
        &[
            part("display_name", "Ada"),
            file_part("avatar", PNG),
            file_part("header", PDF),
        ],
    )
    .await;
    assert_422(
        &response,
        "Unsupported file type",
        "the bad header sinks the body",
    );
    assert_eq!(
        stored_files(&dir),
        0,
        "and the good avatar was never stored"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.name, "", "no column of the same body was written");
    assert!(row.avatar_url.is_none(), "and no image either");
}

/// The image slots are absent from a text-only body, so an account with a
/// stored avatar keeps it (and its file) through a name or note update.
#[tokio::test]
async fn a_text_update_leaves_a_stored_image_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, token, alice) = authed_app_with_uploads(&pool, "alice", &dir).await;
    let avatar = url_of(&ok_body(&upload(&app, &token, "avatar", PNG).await)["avatar"]);
    let body = ok_body(&patch_json(&app, &token, &json!({"display_name": "Grace"})).await);
    assert_eq!(
        body["avatar"], avatar,
        "an absent avatar parameter keeps the stored image"
    );
    assert_eq!(served(&dir, &avatar).as_deref(), Some(PNG), "and its file");
    assert_eq!(
        stored(&pool, alice.id).await.avatar_url.as_deref(),
        Some(avatar.as_str()),
        "stored through the unrelated write"
    );
}

/// The `pleroma` block of an answered owner entity.
fn pleroma_of(body: &Value) -> Value {
    body.get("pleroma")
        .cloned()
        .expect("update_credentials renders the pleroma block")
}

/// The eleven Pleroma preference flags `update_credentials` writes, in the
/// order the entity renders them.
const PREF_FLAGS: [&str; 11] = [
    "hide_followers",
    "hide_follows",
    "hide_followers_count",
    "hide_follows_count",
    "hide_favorites",
    "skip_thread_containment",
    "allow_following_move",
    "accepts_chat_messages",
    "show_birthday",
    "show_role",
    "no_rich_text",
];

/// The two of [`PREF_FLAGS`] Pleroma renders inside `source.pleroma` rather
/// than the top-level `pleroma` block.
const SOURCE_FLAGS: [&str; 2] = ["show_role", "no_rich_text"];

/// The value the owner entity answers for one preference flag, wherever that
/// key lives.
fn flag_of(body: &Value, field: &str) -> Value {
    if SOURCE_FLAGS.contains(&field) {
        return source_of(body)["pleroma"][field].clone();
    }
    pleroma_of(body)[field].clone()
}

/// The stored column behind one preference flag, for the assertions that
/// read the row rather than the answer.
fn stored_flag(row: &User, field: &str) -> bool {
    match field {
        "hide_followers" => row.hide_followers.get(),
        "hide_follows" => row.hide_follows.get(),
        "hide_followers_count" => row.hide_followers_count.get(),
        "hide_follows_count" => row.hide_follows_count.get(),
        "hide_favorites" => row.hide_favorites.get(),
        "skip_thread_containment" => row.skip_thread_containment.get(),
        "allow_following_move" => row.allow_following_move.get(),
        "accepts_chat_messages" => row.accepts_chat_messages.get(),
        "show_birthday" => row.show_birthday.get(),
        "show_role" => row.show_role.get(),
        "no_rich_text" => row.no_rich_text.get(),
        other => panic!("`{other}` is not one of the eleven preference flags"),
    }
}

/// `source[privacy]` sets the posting default, in all three spellings the
/// endpoint takes it in — the nested JSON object, the bracketed form key and
/// Pleroma's flat `default_scope` — with the block winning over the flat
/// name, as `AccountController.update_credentials` applies them.
#[tokio::test]
async fn source_privacy_updates_the_posting_default() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let via_json =
        ok_body(&patch_json(&app, &token, &json!({"source": {"privacy": "private"}})).await);
    assert_eq!(
        source_of(&via_json)["privacy"],
        "private",
        "the nested JSON object answers the new default"
    );

    let via_form = ok_body(&patch_form(&app, &token, &[("source[privacy]", "unlisted")]).await);
    assert_eq!(
        source_of(&via_form)["privacy"],
        "unlisted",
        "the bracketed form key is the same parameter"
    );

    let via_flat = ok_body(&patch_form(&app, &token, &[("default_scope", "direct")]).await);
    assert_eq!(
        source_of(&via_flat)["privacy"],
        "direct",
        "and so is Pleroma's flat name"
    );

    let both = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"default_scope": "public", "source": {"privacy": "private"}}),
        )
        .await,
    );
    assert_eq!(
        source_of(&both)["privacy"],
        "private",
        "the block wins over the flat name"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.default_scope, "private",
        "the column is the answered privacy"
    );
}

/// A privacy outside the four visibilities is the 422, in either spelling,
/// and the refused body writes nothing at all.
#[tokio::test]
async fn an_unknown_privacy_is_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    for body in [
        json!({"source": {"privacy": "internal"}}),
        json!({"default_scope": "protected"}),
        json!({"source": {"privacy": ""}}),
        json!({"source": {"privacy": "PUBLIC"}}),
    ] {
        let response = patch_json(&app, &token, &body).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{body}` names no visibility"
        );
        assert_eq!(
            response.text(),
            r#"{"error":"privacy: is not included in the list"}"#,
            "the 422 names the parameter"
        );
    }
    let mixed = patch_json(
        &app,
        &token,
        &json!({"display_name": "Ada", "source": {"privacy": "internal"}}),
    )
    .await;
    assert_eq!(
        mixed.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a good field riding with a bad privacy is refused whole"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.default_scope, "public", "nothing was written");
    assert_eq!(row.name, "", "and neither was the name");
}

/// `source[sensitive]` writes the sensitive posting default through every
/// body type, flex booleans and all, and never moves its sibling.
#[tokio::test]
async fn source_sensitive_updates_the_sensitive_default() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let via_json =
        ok_body(&patch_json(&app, &token, &json!({"source": {"sensitive": true}})).await);
    assert_eq!(
        source_of(&via_json)["sensitive"],
        true,
        "the JSON boolean answers the new default"
    );

    let via_form = ok_body(&patch_form(&app, &token, &[("source[sensitive]", "on")]).await);
    assert_eq!(
        source_of(&via_form)["sensitive"],
        true,
        "a form `on` is the same true"
    );

    let via_multipart =
        ok_body(&patch_multipart(&app, &token, &[part("source[sensitive]", "0")]).await);
    assert_eq!(
        source_of(&via_multipart)["sensitive"],
        false,
        "and a multipart `0` is a false"
    );
    assert_eq!(
        source_of(&via_multipart)["privacy"],
        "public",
        "the privacy the body never named stayed"
    );

    let bad = patch_form(&app, &token, &[("source[sensitive]", "maybe")]).await;
    assert_eq!(
        bad.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unknown boolean string is the extractor's 422"
    );
    let row = stored(&pool, alice.id).await;
    assert!(
        !row.sensitive_default.get(),
        "the stored flag is the answered one"
    );
    assert_eq!(
        row.default_scope, "public",
        "and the refusals wrote nothing"
    );
}

/// `source[language]` writes the language posting default, the empty string
/// clears it back to `null`, and a tag over the ten-character cap is the 422.
#[tokio::test]
async fn source_language_is_capped_and_the_empty_tag_clears() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let via_json =
        ok_body(&patch_json(&app, &token, &json!({"source": {"language": "pt-BR"}})).await);
    assert_eq!(
        source_of(&via_json)["language"],
        "pt-BR",
        "the JSON object answers the tag"
    );

    let via_form = ok_body(&patch_form(&app, &token, &[("source[language]", "de")]).await);
    assert_eq!(
        source_of(&via_form)["language"],
        "de",
        "the bracketed form key is the same parameter"
    );

    let at_cap = "é".repeat(10);
    let capped = ok_body(&patch_form(&app, &token, &[("source[language]", &at_cap)]).await);
    assert_eq!(
        source_of(&capped)["language"],
        at_cap,
        "ten characters fit, counted as characters, not bytes"
    );

    let over = patch_json(
        &app,
        &token,
        &json!({"source": {"language": "0123456789a"}}),
    )
    .await;
    assert_eq!(
        over.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "eleven characters are over the cap"
    );
    assert_eq!(
        over.text(),
        r#"{"error":"Language is too long (maximum is 10 characters)"}"#,
        "the length message names the cap"
    );

    let cleared = ok_body(&patch_form(&app, &token, &[("source[language]", "")]).await);
    assert_eq!(
        source_of(&cleared)["language"],
        Value::Null,
        "the empty string clears the preference"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.language, None, "the column is NULL again");
    assert_eq!(
        row.default_scope, "public",
        "the language write left the privacy alone"
    );
}

/// Each of the eleven preference flags updates on its own, through JSON,
/// form and multipart bodies, reaching the entity wherever Pleroma renders
/// that key and never moving any of the other ten.
#[tokio::test]
async fn each_pleroma_flag_updates_on_its_own() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let initial = ok_body(&patch_json(&app, &token, &json!({"display_name": "Ada"})).await);
    let mut expect = json!({});
    for field in PREF_FLAGS {
        expect[field] = flag_of(&initial, field);
    }

    for field in PREF_FLAGS {
        expect[field] = json!(true);
        let via_json = ok_body(&patch_json(&app, &token, &one_flag(field, true)).await);
        assert_pref_flags(&via_json, &expect, &format!("a JSON true for {field}"));

        expect[field] = json!(false);
        let via_form = ok_body(&patch_form(&app, &token, &[(field, "false")]).await);
        assert_pref_flags(&via_form, &expect, &format!("a form false for {field}"));

        expect[field] = json!(true);
        let via_multipart = ok_body(
            &patch_multipart(
                &app,
                &token,
                &[part(field, "yes"), part("source[privacy]", "direct")],
            )
            .await,
        );
        assert_pref_flags(
            &via_multipart,
            &expect,
            &format!("a multipart yes for {field}"),
        );
        assert_eq!(
            source_of(&via_multipart)["privacy"],
            "direct",
            "the privacy rode along with the flag"
        );
    }

    let row = stored(&pool, alice.id).await;
    for field in PREF_FLAGS {
        assert!(
            stored_flag(&row, field),
            "{field} is the stored true the answer stated"
        );
    }
    assert_eq!(row.default_scope, "direct", "the stored privacy too");
}

/// Assert every preference flag of the answer against `expect` — the running
/// stored state, so each round also proves the ten flags it did not send
/// stayed where they were.
fn assert_pref_flags(body: &Value, expect: &Value, why: &str) {
    for field in PREF_FLAGS {
        assert_eq!(flag_of(body, field), expect[field], "{why}: {field}");
    }
}

/// `birthday` is stored from a JSON body and a form alike, the empty string
/// clears the column, and the answer renders the date back in the owner's
/// `pleroma` block.
#[tokio::test]
async fn birthday_updates_and_the_empty_string_clears_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let via_json = ok_body(&patch_json(&app, &token, &json!({"birthday": "1990-01-31"})).await);
    assert_eq!(
        pleroma_of(&via_json)["birthday"],
        "1990-01-31",
        "the owner view answers the stored date"
    );

    let via_form = ok_body(&patch_form(&app, &token, &[("birthday", "2000-02-29")]).await);
    assert_eq!(
        pleroma_of(&via_form)["birthday"],
        "2000-02-29",
        "a form body is the same parameter"
    );
    assert_eq!(
        stored(&pool, alice.id).await.birthday,
        NaiveDate::from_ymd_opt(2000, 2, 29),
        "the leap day is what the row holds"
    );

    let shown = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"show_birthday": true, "birthday": "1990-01-31"}),
        )
        .await,
    );
    assert_eq!(
        pleroma_of(&shown)["show_birthday"],
        true,
        "the flag a client sets beside the date sticks"
    );

    let cleared = ok_body(&patch_form(&app, &token, &[("birthday", "")]).await);
    assert_eq!(
        pleroma_of(&cleared)["birthday"],
        Value::Null,
        "the empty string clears the date"
    );
    assert_eq!(
        stored(&pool, alice.id).await.birthday,
        None,
        "and stores NULL behind it"
    );
}

/// A birthday that is not `YYYY-MM-DD` text or one after today is the 422,
/// and neither writes anything.
#[tokio::test]
async fn an_invalid_or_future_birthday_is_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let stored_date = ok_body(&patch_json(&app, &token, &json!({"birthday": "1990-01-31"})).await);
    assert_eq!(
        pleroma_of(&stored_date)["birthday"],
        "1990-01-31",
        "the fixture date is stored"
    );

    for text in [
        "31-01-1990",
        "1990-1-31",
        "1990-13-01",
        "yesterday",
        "1990-01-31T00:00:00Z",
    ] {
        let response = patch_json(&app, &token, &json!({"birthday": text})).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{text}` is no `YYYY-MM-DD` date"
        );
        assert_eq!(
            response.text(),
            r#"{"error":"birthday: is not a valid date"}"#,
            "the parse 422 names the parameter"
        );
    }

    let tomorrow = Utc::now()
        .date_naive()
        .succ_opt()
        .expect("tomorrow exists")
        .format("%Y-%m-%d")
        .to_string();
    let future = patch_form(&app, &token, &[("birthday", &tomorrow)]).await;
    assert_eq!(
        future.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "`{tomorrow}` is in the future"
    );
    assert_eq!(
        future.text(),
        r#"{"error":"birthday: must not be in the future"}"#,
        "the future 422 names the parameter"
    );

    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.birthday,
        NaiveDate::from_ymd_opt(1990, 1, 31),
        "no refusal moved the stored date"
    );
}

/// `pleroma_settings_store` merges into the stored blob key by key: a
/// submitted key wins, an untouched key stays, a nested object is replaced
/// whole, and the owner's `pleroma.settings_store` answers the result.
#[tokio::test]
async fn the_settings_store_merges_key_by_key() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.settings_store = json!({"theme": "dark", "locale": "pt", "fe": {"a": 1, "b": 2}});
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    let merged = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"pleroma_settings_store": {"locale": "de", "custom": {"nested": true}}}),
        )
        .await,
    );
    assert_eq!(
        pleroma_of(&merged)["settings_store"],
        json!({"theme": "dark", "locale": "de", "fe": {"a": 1, "b": 2}, "custom": {"nested": true}}),
        "the submitted keys win, the stored ones stay"
    );

    let replaced = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"pleroma_settings_store": {"fe": {"c": 3}}}),
        )
        .await,
    );
    assert_eq!(
        pleroma_of(&replaced)["settings_store"],
        json!({"theme": "dark", "locale": "de", "custom": {"nested": true}, "fe": {"c": 3}}),
        "a nested object replaces the stored one: the merge is shallow, key by key"
    );

    let nulled = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"pleroma_settings_store": {"theme": null}}),
        )
        .await,
    );
    assert_eq!(
        pleroma_of(&nulled)["settings_store"]["theme"],
        Value::Null,
        "unlike the settings endpoint's deep merge, a null is a stored null"
    );

    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.settings_store,
        json!({"custom": {"nested": true}, "fe": {"c": 3}, "locale": "de", "theme": null}),
        "the row is what the answer stated"
    );
    assert_eq!(row.name, "", "the settings write left the profile alone");
}

/// A form body carries `pleroma_settings_store` as JSON *text*; so does a
/// multipart text part. Both parse into the object they name and merge like
/// the JSON spelling.
#[tokio::test]
async fn a_form_settings_store_arrives_as_json_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let patch = r#"{"theme": "light", "fe": {"v": 1}}"#;

    let via_form = ok_body(&patch_form(&app, &token, &[("pleroma_settings_store", patch)]).await);
    assert_eq!(
        pleroma_of(&via_form)["settings_store"],
        json!({"theme": "light", "fe": {"v": 1}}),
        "the form's JSON text parses"
    );

    let via_multipart = ok_body(
        &patch_multipart(
            &app,
            &token,
            &[
                part("pleroma_settings_store", "{\"bookmarks\": false}"),
                part("display_name", "Ada"),
            ],
        )
        .await,
    );
    assert_eq!(
        pleroma_of(&via_multipart)["settings_store"],
        json!({"theme": "light", "fe": {"v": 1}, "bookmarks": false}),
        "a multipart text part merges over what the form left"
    );
    assert_eq!(
        via_multipart["display_name"], "Ada",
        "and the text field rode along"
    );

    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.settings_store,
        json!({"theme": "light", "fe": {"v": 1}, "bookmarks": false}),
        "stored as answered"
    );
}

/// A settings store that is no object, and a merged blob past 1 MiB, are both
/// the 422 with nothing written.
#[tokio::test]
async fn a_bad_or_oversize_settings_store_is_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.settings_store = json!({"theme": "dark"});
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    for patch in [json!([1, 2]), json!(5), json!("plain text")] {
        let response = patch_json(&app, &token, &json!({"pleroma_settings_store": patch})).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{patch}` has no keys to merge"
        );
        assert_eq!(
            response.text(),
            r#"{"error":"pleroma_settings_store: expected a JSON object"}"#,
            "the shape 422 names the parameter"
        );
    }

    let blob = "x".repeat(1024 * 1024 + 1);
    let too_large = patch_json(
        &app,
        &token,
        &json!({"pleroma_settings_store": {"blob": blob}}),
    )
    .await;
    assert_eq!(
        too_large.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a merged store past one mebibyte is refused"
    );
    assert_eq!(
        too_large.text(),
        r#"{"error":"settings_store: too large"}"#,
        "the same wording the settings endpoint uses"
    );

    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.settings_store,
        json!({"theme": "dark"}),
        "and every refusal left the stored blob alone"
    );
}

/// `actor_type` and `bot` are one pair: the stated kind decides the flag, the
/// flag picks the kind when no kind is stated, and a kind outside `Person`
/// and `Service` is the 422.
#[tokio::test]
async fn actor_type_and_bot_are_written_as_one_pair() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let service = ok_body(&patch_json(&app, &token, &json!({"actor_type": "Service"})).await);
    assert_eq!(
        source_of(&service)["pleroma"]["actor_type"],
        "Service",
        "the stated kind is the stored kind"
    );
    assert_eq!(service["bot"], true, "and a Service is a bot");

    let human = ok_body(&patch_form(&app, &token, &[("actor_type", "Person")]).await);
    assert_eq!(
        source_of(&human)["pleroma"]["actor_type"],
        "Person",
        "a form body states it the same way"
    );
    assert_eq!(human["bot"], false, "and a Person is not a bot");

    let automated = ok_body(&patch_json(&app, &token, &json!({"bot": true})).await);
    assert_eq!(
        source_of(&automated)["pleroma"]["actor_type"],
        "Service",
        "the bot flag alone picks the actor kind, as Pleroma does"
    );
    assert_eq!(automated["bot"], true, "and stays set");

    let both = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"bot": false, "actor_type": "Service"}),
        )
        .await,
    );
    assert_eq!(
        source_of(&both)["pleroma"]["actor_type"],
        "Service",
        "an explicit kind wins over the flag"
    );
    assert_eq!(
        both["bot"], true,
        "and the answered flag reads back from the kind"
    );

    for kind in ["Group", "Application", "person", ""] {
        let response = patch_json(&app, &token, &json!({"actor_type": kind})).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{kind}` is no kind this endpoint sets"
        );
        assert_eq!(
            response.text(),
            r#"{"error":"actor_type: is not included in the list"}"#,
            "the 422 names the parameter"
        );
    }

    let row = stored(&pool, alice.id).await;
    assert_eq!(row.actor_type, "Service", "the refusals wrote nothing");
    assert!(row.bot.get(), "and so for the bot flag");
}

/// The new parameters are the owner's own settings: the answer carries them
/// in the `source` block and the `pleroma` block, and a stranger's `show`
/// view keeps Pleroma's restricted render of the same row.
#[tokio::test]
async fn the_response_reflects_every_new_value() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let body = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({
                "source": {"privacy": "private", "sensitive": true, "language": "eo"},
                "birthday": "1990-01-31",
                "show_birthday": true,
                "hide_followers": true,
                "no_rich_text": true,
                "pleroma_settings_store": {"theme": "dark"},
                "actor_type": "Service",
            }),
        )
        .await,
    );
    assert_eq!(
        source_of(&body),
        json!({
            "privacy": "private",
            "sensitive": true,
            "language": "eo",
            "note": "",
            "fields": [],
            "follow_requests_count": 0,
            "pleroma": {
                "show_role": true,
                "no_rich_text": true,
                "discoverable": false,
                "actor_type": "Service",
            },
        }),
        "the source block carries the posting defaults and the pleroma settings"
    );
    assert_eq!(body["bot"], true, "the answered bot flag follows the kind");
    let pleroma = pleroma_of(&body);
    assert_eq!(pleroma["birthday"], "1990-01-31", "the owner sees the date");
    assert_eq!(pleroma["show_birthday"], true, "and the flag");
    assert_eq!(pleroma["hide_followers"], true, "and the hidden list");
    assert_eq!(
        pleroma["settings_store"],
        json!({"theme": "dark"}),
        "and the client settings"
    );

    let other = common::create_local_user(&pool, "bob").await;
    let viewer = common::create_app_and_token(&pool, &other, "read:accounts").await;
    let shown = common::TestRequest::get(&format!("/api/v1/accounts/{}", alice.id))
        .bearer(&viewer)
        .send(&app)
        .await;
    let shown_body = shown.json_value();
    assert_eq!(
        shown_body.get("source"),
        None,
        "a stranger gets no source block"
    );
    assert_eq!(
        shown_body["pleroma"]["birthday"], "1990-01-31",
        "and sees the birthday the account chose to show"
    );
    assert_eq!(
        shown_body["pleroma"]["hide_followers"], true,
        "the privacy flags are public information"
    );
    assert_eq!(
        shown_body["pleroma"].get("settings_store"),
        None,
        "but the client settings are not"
    );
}
