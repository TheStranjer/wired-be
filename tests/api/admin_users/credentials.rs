//! The credentials read and the validating half of the write (WBE-198):
//! the pinned `"credentials.json"` form the seeded account answers with,
//! the changeset PATCH that writes every field it carries, the refusals it
//! reports all at once, the length rules mirroring the user endpoint and
//! the taken-address rule that reads the table, not the request.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::config::TEST_BASE_URL;

use super::support::{
    DEFAULT_AVATAR, NOT_FOUND, credentials, flag_of, logged, one_entry, patch_json, plain, read,
    seed, write_token,
};
use crate::common::{self, TestRequest};

/// The `GET …/credentials` answer of the seeded `plain` carries exactly
/// the twenty-four keys Pleroma's `"credentials.json"` renders, the stored
/// columns as they are and the images as the entity's own defaults — and
/// the same form is reachable by id as by nickname.
#[tokio::test]
async fn the_credentials_read_answers_the_pinned_form() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let body = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(
        body,
        json!({
            "id": plain(&seeded.pool).await.id.to_string(),
            "bio": "",
            "email": "plain@example.org",
            "fields": [],
            "name": "Plain Person",
            "nickname": "plain",
            "is_locked": false,
            "no_rich_text": false,
            "default_scope": "public",
            "hide_follows": false,
            "hide_followers_count": false,
            "hide_follows_count": false,
            "hide_followers": false,
            "hide_favorites": true,
            "allow_following_move": true,
            "show_role": true,
            "skip_thread_containment": false,
            "pleroma_settings_store": {},
            "raw_fields": [],
            "is_discoverable": false,
            "actor_type": "Person",
            "avatar": format!("{TEST_BASE_URL}{DEFAULT_AVATAR}"),
            "banner": format!("{TEST_BASE_URL}/media/default/banner.png"),
            "background": null,
        }),
        "the moderation form, key for key"
    );

    let by_id = read(
        &seeded.app,
        &seeded.token,
        &credentials(&plain(&seeded.pool).await.id.to_string()),
    )
    .await;
    assert_eq!(
        by_id["nickname"],
        json!("plain"),
        "the id spelling resolves to the same form"
    );

    for segment in ["nobody", "faraway@remote.example"] {
        let response = TestRequest::get(&credentials(segment))
            .bearer(&seeded.token)
            .send(&seeded.app)
            .await;
        common::assert_answer(
            &response,
            StatusCode::NOT_FOUND,
            "a credentials miss or a remote mirror",
        );
        common::assert_error(
            &response,
            NOT_FOUND,
            "a credentials miss or a remote mirror",
        );
    }
}

/// The full credentials write: the profile columns, the rendered note, the
/// merged settings blob, the actor-kind-pinned `bot`, the new address and
/// the stated booleans all land, the answer is Pleroma's success map and
/// the entry reads `@root updated users: @plain`.
#[tokio::test]
async fn a_credentials_patch_writes_every_field_it_carries() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let response = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({
            "name": "Ada Byron",
            "bio": "hello world",
            "default_scope": "private",
            "actor_type": "Service",
            "email": "replained@example.org",
            "pleroma_settings_store": {"config": {"fe": "mochi-pub"}},
            "is_locked": true,
            "is_discoverable": true,
            "hide_favorites": false,
        }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "an accepted credentials write");
    assert_eq!(
        response.json_value(),
        json!({ "status": "success" }),
        "Pleroma's success map"
    );

    let after = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(after["name"], json!("Ada Byron"), "the display name");
    assert_eq!(
        after["bio"],
        json!("<p>hello world</p>"),
        "the rendered note"
    );
    assert_eq!(after["default_scope"], json!("private"), "the scope");
    assert_eq!(after["actor_type"], json!("Service"), "the actor kind");
    assert_eq!(
        after["email"],
        json!("replained@example.org"),
        "the new address"
    );
    assert_eq!(
        after["pleroma_settings_store"],
        json!({"config": {"fe": "mochi-pub"}}),
        "the merged blob"
    );
    assert_eq!(after["is_locked"], json!(true), "the locked flag");
    assert_eq!(after["is_discoverable"], json!(true), "the discovery flag");
    assert_eq!(
        after["hide_favorites"],
        json!(false),
        "the favourite list reopened"
    );
    assert!(
        flag_of(
            &seeded.pool,
            "SELECT bot FROM users WHERE id = $1",
            &plain(&seeded.pool).await
        )
        .await,
        "the `Service` kind re-derived bot"
    );

    let (message, subjects) = one_entry(&seeded.pool, "updated_users").await;
    assert_eq!(message, "@root updated users: @plain", "Pleroma's sentence");
    assert_eq!(subjects, vec!["plain".to_owned()], "the named subject");
}

/// One body that fails six rules reports ALL of them at once, Ecto-style,
/// and changes nothing: the entity still reads as stored, the blob kept its
/// old shape and no `updated_users` entry exists.
#[tokio::test]
async fn a_credentials_patch_reports_every_refusal_at_once() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let response = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({
            "email": "not-an-address",
            "default_scope": "leaked",
            "actor_type": "Group",
            "pleroma_settings_store": [1, 2],
            "password": "short",
            "password_confirmation": "different",
        }),
    )
    .await;
    common::assert_answer(&response, StatusCode::BAD_REQUEST, "the changeset refusal");
    assert_eq!(
        response.json_value(),
        json!({ "errors": {
            "email": ["has invalid format"],
            "default_scope": ["is not included in the list"],
            "actor_type": ["is not included in the list"],
            "pleroma_settings_store": ["is invalid"],
            "password": ["should be at least 8 character(s)"],
            "password_confirmation": ["does not match password"],
        } }),
        "the whole field map, one entry per refusing field"
    );

    let still = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(
        still["email"],
        json!("plain@example.org"),
        "the address stands"
    );
    assert_eq!(still["default_scope"], json!("public"), "and the scope");
    assert!(
        logged(&seeded.pool, "updated_users").await.is_empty(),
        "nothing logged"
    );
}

/// The length rules of the user-facing endpoint apply under the admin's
/// parameter names too: an over-long name and bio are refused under
/// `name`/`bio` with the same messages.
#[tokio::test]
async fn the_credentials_checks_mirror_the_user_endpoint() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let config = common::test_config();
    let long_name = "x".repeat(config.max_display_name_chars + 1);
    let long_bio = "y".repeat(config.max_bio_chars + 1);
    let response = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "name": long_name, "bio": long_bio }),
    )
    .await;
    common::assert_answer(&response, StatusCode::BAD_REQUEST, "the length refusals");
    assert_eq!(
        response.json_value(),
        json!({ "errors": {
            "name": [format!("Name is too long (maximum is {} characters)", config.max_display_name_chars)],
            "bio": [format!("Bio is too long (maximum is {} characters)", config.max_bio_chars)],
        } }),
        "both fields name their own limit"
    );
}

/// The taken-address rule reads the table, not the request: the same
/// address on the same account rewrites happily, another account's is the
/// `has already been taken` refusal.
#[tokio::test]
async fn an_email_held_by_another_account_is_taken() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;

    let own = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "email": "plain@example.org" }),
    )
    .await;
    common::assert_answer(&own, StatusCode::OK, "the account's own address");

    let refusal = patch_json(
        &seeded.app,
        &token,
        &credentials("root"),
        &json!({ "email": "plain@example.org" }),
    )
    .await;
    common::assert_answer(&refusal, StatusCode::BAD_REQUEST, "the taken address");
    assert_eq!(
        refusal.json_value(),
        json!({ "errors": { "email": ["has already been taken"] } }),
        "Ecto's unique_constraint wording"
    );
}
