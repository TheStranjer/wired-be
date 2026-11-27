//! The `display_name` and `note` fields: every body type, sanitizing, note
//! rendering with its links, and the length caps.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::{
    models::user::User,
    util::entities::{hashtag_link_html, mention_html},
};

use super::support::{
    BASE, authed_app, ok_body, part, patch_form, patch_json, patch_multipart, source_of, stored,
};
use crate::common;

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
        note.contains(&hashtag_link_html(BASE, "rust", "Rust")),
        "the hashtag links lowercased but reads as written: {note}"
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

/// With `LOCAL_DOMAIN` split from the web host, a mention spelled with the
/// handle domain or the web host links to the local user; a third domain
/// never does. The bio renderer reads the database only, so no outbound
/// request is made either way.
#[tokio::test]
async fn note_links_mentions_of_either_split_domain_as_local() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _bob: User = common::create_local_user(&pool, "bob").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let mut config = common::test_config();
    config.local_domain = "whatever.com".to_owned();
    let app = common::test_app_with_config(pool.clone(), config);

    let body = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"note": "ping @bob@whatever.com @bob@test.example @bob@other.example"}),
        )
        .await,
    );
    let note = body["note"].as_str().expect("the note is a string");
    assert!(
        note.contains(&mention_html(&format!("{BASE}/users/bob"), "bob")),
        "the handle-domain and web-host spellings name the local user: {note}"
    );
    assert!(
        note.contains("@bob@other.example"),
        "and the foreign one stays plain text: {note}"
    );
    assert_eq!(
        note.matches("h-card").count(),
        2,
        "exactly two of the three mentions linked: {note}"
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
