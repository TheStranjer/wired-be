//! The `locked` / `bot` / `discoverable` booleans, absent and null fields,
//! the `source` block of the answer, the auth and scope gate, the `POST`
//! alias, and the files that ride along (or are ignored) in a multipart body.

use axum::{body::Bytes, http::StatusCode};
use serde_json::{Value, json};

use super::support::{
    BOUNDARY, DEFAULT_AVATAR, DEFAULT_HEADER, INVALID_TOKEN, MEDIA, NO_SCOPE, NO_TOKEN, PNG, ROUTE,
    USER_REQUIRED, authed_app, authed_app_with_uploads, extension_of, file_part, ok_body, one_flag,
    part, patch_form, patch_json, patch_multipart, prefilled_app, served, source_of, stored,
    stored_files, uploads_dir, url_of,
};
use crate::common;

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
