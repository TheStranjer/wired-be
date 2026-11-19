//! The writing half of the credentials PATCH (WBE-198): the password
//! change that replaces the hash and revokes the tokens in one stroke, the
//! empty and all-null bodies that write nothing and still land, the form
//! body that reaches the same rules, the submitted `raw_fields` rendered
//! and refused, and the shallow-merging settings blob.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    INVALID_TOKEN, credentials, mint, one_entry, patch_json, pending, read, seed, write_token,
};
use crate::common::{self, TestRequest};

/// A credentials password replaces the hash and revokes the account's
/// tokens in one stroke: the old bearer is 401, the new password signs in
/// and the old one does not, and — unlike Pleroma's async force behind the
/// same write — the account is NOT put on pending reset.
#[tokio::test]
async fn a_credentials_password_replaces_hash_and_tokens() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let writer =
        common::create_user_with_password(&seeded.pool, "writer", "first secret pass").await;
    let admin_token = write_token(&seeded).await;
    let old_bearer = mint(&seeded.pool, &writer, "read").await;

    let response = patch_json(
        &seeded.app,
        &admin_token,
        &credentials("writer"),
        &json!({ "password": "second secret pass" }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the password write");

    let stale = TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&old_bearer)
        .send(&seeded.app)
        .await;
    common::assert_answer(&stale, StatusCode::UNAUTHORIZED, "the revoked token");
    common::assert_error(&stale, INVALID_TOKEN, "the revoked token");

    assert!(
        wired_be::models::user::register::authenticate(
            &seeded.pool,
            "writer",
            "second secret pass"
        )
        .await
        .is_some(),
        "the new password signs in"
    );
    assert!(
        wired_be::models::user::register::authenticate(&seeded.pool, "writer", "first secret pass")
            .await
            .is_none(),
        "and the old one does not"
    );
    assert!(
        !pending(&seeded.pool, &writer).await,
        "the credentials write does not force a reset"
    );
    let (message, subjects) = one_entry(&seeded.pool, "updated_users").await;
    assert_eq!(message, "@root updated users: @writer", "one log entry");
    assert_eq!(
        subjects,
        vec!["writer".to_owned()],
        "for the written account"
    );
}

/// A body that names nothing to write is the accepted no-op — 200, the
/// success map, nothing changed — and Pleroma's log clause runs all the
/// same. An empty patch on a mirror or a miss is the 404 before anything
/// else.
#[tokio::test]
async fn an_empty_credentials_patch_changes_nothing_but_lands() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let response = patch_json(&seeded.app, &token, &credentials("plain"), &json!({})).await;
    common::assert_answer(&response, StatusCode::OK, "the accepted no-op");
    assert_eq!(
        response.json_value(),
        json!({ "status": "success" }),
        "still the success map"
    );

    let after = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(after["name"], json!("Plain Person"), "the row stands");
    let (message, subjects) = one_entry(&seeded.pool, "updated_users").await;
    assert_eq!(
        message, "@root updated users: @plain",
        "Pleroma's clause logs the empty write too"
    );
    assert_eq!(subjects, vec!["plain".to_owned()], "and names its subject");

    let mirror = patch_json(
        &seeded.app,
        &token,
        &credentials("faraway@remote.example"),
        &json!({}),
    )
    .await;
    common::assert_answer(
        &mirror,
        StatusCode::NOT_FOUND,
        "a mirror has no credentials",
    );
}

/// The JSON `null` reads as absent on every credentials parameter (the
/// changeset drops nils before casting): a null email, name and store
/// write nothing and refuse nothing.
#[tokio::test]
async fn a_credentials_null_reads_as_absent() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let response = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "email": null, "name": null, "pleroma_settings_store": null, "password": null }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the all-null body");
    let after = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(
        after["email"],
        json!("plain@example.org"),
        "nothing cleared"
    );
    assert_eq!(after["name"], json!("Plain Person"), "nothing blanked");
}

/// A form body carries the same write: the bracketed-free `name` and the
/// `pleroma_settings_store` as the JSON TEXT of an object, the way a form
/// client sends it.
#[tokio::test]
async fn a_credentials_form_body_writes_through_the_same_rules() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let response = TestRequest::patch(&credentials("plain"))
        .bearer(&token)
        .form(&[
            ("name", "Form Filler"),
            ("default_scope", "direct"),
            (
                "pleroma_settings_store",
                "{\"config\":{\"fe\":\"pleroma-fe\"}}",
            ),
        ])
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the form write");
    let after = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(after["name"], json!("Form Filler"), "the name landed");
    assert_eq!(after["default_scope"], json!("direct"), "and the scope");
    assert_eq!(
        after["pleroma_settings_store"],
        json!({"config": {"fe": "pleroma-fe"}}),
        "and the text-parsed blob merged"
    );
}

/// The metadata pair runs the user endpoint's own validator: a submitted
/// `raw_fields` array stores the raw entry beside its rendered twin, and
/// one entry past the configured cap is refused under `raw_fields`.
#[tokio::test]
async fn submitted_raw_fields_land_rendered_and_refused() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let response = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "raw_fields": [{"name": "web", "value": "https://example.org"}] }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the metadata write");
    let after = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(
        after["raw_fields"],
        json!([{"name": "web", "value": "https://example.org"}]),
        "the typed pair is stored as submitted"
    );
    assert_eq!(
        after["fields"][0]["name"],
        json!("web"),
        "and the rendered twin came along"
    );

    let config = common::test_config();
    let too_many: Vec<Value> = (0..=config.max_profile_fields)
        .map(|index| json!({"name": format!("f{index}"), "value": "v"}))
        .collect();
    let refused = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "raw_fields": too_many }),
    )
    .await;
    common::assert_answer(&refused, StatusCode::BAD_REQUEST, "the over-cap metadata");
    assert_eq!(
        refused.json_value(),
        json!({ "errors": { "raw_fields": ["Too many fields"] } }),
        "the user endpoint's own message, under the admin's parameter name"
    );
}

/// A blob that is too large or no object at all is refused with the Ecto
/// shape; a valid patch only ever ADDS to the stored blob — the shallow
/// merge Pleroma's `Map.merge` runs.
#[tokio::test]
async fn the_settings_store_merges_and_measures() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let stored = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "pleroma_settings_store": {"config": {"a": 1}, "other": true} }),
    )
    .await;
    common::assert_answer(&stored, StatusCode::OK, "the first store write");

    let merged = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "pleroma_settings_store": {"config": {"b": 2}} }),
    )
    .await;
    common::assert_answer(&merged, StatusCode::OK, "the second store write");
    let after = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(
        after["pleroma_settings_store"],
        json!({"config": {"b": 2}, "other": true}),
        "the inner config was replaced whole, the top-level entry stayed"
    );

    let scalar = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "pleroma_settings_store": 42 }),
    )
    .await;
    common::assert_answer(&scalar, StatusCode::BAD_REQUEST, "a scalar store");
    assert_eq!(
        scalar.json_value(),
        json!({ "errors": { "pleroma_settings_store": ["is invalid"] } }),
        "no object, no keys to merge"
    );
}
