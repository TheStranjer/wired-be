//! The caps on `fields_attributes`: the `MAX_PROFILE_FIELDS` entry count, its
//! configuration and the per-entry name and value character limits.

use axum::http::StatusCode;
use serde_json::json;

use super::support::{authed_app, ok_body, patch_json, stored};
use crate::common;

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
