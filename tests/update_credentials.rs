//! Integration tests for `PATCH /api/v1/accounts/update_credentials`
//! (WBE-67, the profile-text-fields ticket, extended by WBE-69, the
//! profile-metadata ticket): the fields this slice of the endpoint owns —
//! `display_name` (length-capped against `MAX_DISPLAY_NAME_CHARS`,
//! sanitized, empty stored as empty), `note` (length-capped against
//! `MAX_BIO_CHARS`, stored raw in `raw_bio` and rendered to `bio` with
//! URL, hashtag and local-mention links), the `locked` / `bot` /
//! `discoverable` booleans, and `fields_attributes` (the profile
//! metadata: every body shape, the `MAX_PROFILE_FIELDS` count cap, the
//! per-entry name and value character caps, both-empty entries dropped,
//! empty clearing, raw values kept beside the rendered HTML) — each
//! updated individually through JSON, form and multipart bodies, with
//! absent fields left untouched, invalid boolean strings and over-long
//! texts answered 422, the 200 answer the owner entity with its `source`
//! block (the `verify_credentials` render), the route behind `AuthUser`
//! and `write:accounts` (401 unauthenticated, 403 for the wrong
//! credential), and the `POST` alias serving the old clients.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, body::Bytes, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::user::{Flag, User, get_by_id},
    util::entities::{hashtag_html, mention_html},
};

/// The endpoint under test.
const ROUTE: &str = "/api/v1/accounts/update_credentials";
/// The multipart boundary the test bodies are framed with.
const BOUNDARY: &str = "wired-update-credentials-boundary";
/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = TEST_BASE_URL;
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body for a token without the endpoint's scope.
const NO_SCOPE: &str = r#"{"error":"Insufficient permissions: write:accounts."}"#;

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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn display_name_updates_through_every_body_type(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn display_name_is_sanitized_and_empty_is_stored(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn display_name_length_limit_is_422(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn note_updates_through_every_body_type(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn note_links_urls_hashtags_and_local_mentions(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn note_links_own_domain_mentions_as_local(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn note_length_limit_is_422(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn boolean_flags_update_through_every_body_type(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn invalid_boolean_strings_are_422(pool: PgPool) {
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

/// Fields absent from the body keep their stored values — the flags, the
/// note and the name each survive an update of the others, and a body
/// that carries none of the five changes nothing (and is not an error).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn absent_fields_stay_untouched(pool: PgPool) {
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.name = "Ada".to_owned();
        new.raw_bio = "old bio".to_owned();
        new.bio = "<p>old bio</p>".to_owned();
        new.locked = Flag::new(true);
        new.bot = Flag::new(true);
        new.discoverable = Flag::new(true);
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    let flipped = ok_body(&patch_json(&app, &token, &json!({"bot": false})).await);
    assert_eq!(flipped["bot"], false, "the sent flag flipped");
    assert_eq!(flipped["locked"], true, "the other flags stay");
    assert_eq!(flipped["discoverable"], true, "and stay");
    assert_eq!(flipped["display_name"], "Ada", "the name stays");
    assert_eq!(flipped["note"], "<p>old bio</p>", "the note stays");
    assert_eq!(source_of(&flipped)["note"], "old bio", "raw stays too");

    let renamed = ok_body(&patch_json(&app, &token, &json!({"display_name": "Grace"})).await);
    assert_eq!(renamed["display_name"], "Grace", "the name updated");
    assert_eq!(renamed["locked"], true, "flags survive a name change");
    assert_eq!(
        renamed["note"], "<p>old bio</p>",
        "the note survives a name change"
    );

    let nulled = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"display_name": null, "note": null, "locked": null}),
        )
        .await,
    );
    assert_eq!(
        nulled["display_name"], "Grace",
        "a JSON null reads as absent, Pleroma's nil filter"
    );
    assert_eq!(nulled["locked"], true, "and so for the booleans");

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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn response_carries_the_verify_credentials_source(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn auth_and_scope_gate_the_write(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    let no_token = common::TestRequest::patch(ROUTE)
        .json(&json!({"display_name": "Nobody"}))
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
}

/// Old clients `POST` the same body to the same path and get the same
/// answer as the `PATCH`ers.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn post_alias_updates_the_same_fields(pool: PgPool) {
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

/// File parts ride along unexamined for now: an `avatar` upload in the
/// same multipart body does not disturb the text fields (the media side
/// of this endpoint is a later ticket).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn multipart_file_parts_are_ignored_by_the_text_slice(pool: PgPool) {
    let (app, token, _) = authed_app(&pool, "alice").await;
    let body = ok_body(
        &patch_multipart(
            &app,
            &token,
            &[
                part("display_name", "Ada"),
                common::MultipartPart::file(
                    "avatar",
                    "cat.png",
                    "image/png",
                    Bytes::from_static(b"not really a png"),
                ),
            ],
        )
        .await,
    );
    assert_eq!(body["display_name"], "Ada", "the text field applied");
    assert_eq!(
        body["avatar"],
        format!("{BASE}/images/avi.png"),
        "the avatar stays the instance default"
    );
}

/// One profile metadata entry as the entity answers it.
fn field(name: &str, value: &str) -> Value {
    json!({"name": name, "value": value, "verified_at": null})
}

/// `fields_attributes` updates through every body shape: the JSON array,
/// the JSON object keyed by index, and the `fields_attributes[0][name]`
/// spelling of form and multipart bodies. Each round answers the top-level
/// `fields` with the rendered HTML and `source.fields` with the raw text,
/// and stores the pair of jsonb columns.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn fields_attributes_update_through_every_body_shape(pool: PgPool) {
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let via_json = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [
                {"name": "Site", "value": "https://example.com/a"},
                {"name": "Motto", "value": "beep boop"},
            ]}),
        )
        .await,
    );
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn field_names_are_stored_as_plain_text(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn too_many_fields_is_422(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn field_name_and_value_length_limits_are_422(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn empty_fields_attributes_clears_the_metadata(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn entries_with_both_empty_name_and_value_are_dropped(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_all_empty_submission_clears_like_the_empty_array(pool: PgPool) {
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
