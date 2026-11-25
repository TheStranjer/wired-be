//! Scenario 1: the onboarding walk end to end — the client's own
//! credential reading itself, the registration it opens, the multipart
//! profile write with its uploaded avatar, the per-app settings store, the
//! public read of everything stored, and the password change that revokes
//! the account's other session.

use axum::body::Bytes;
use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    ACCOUNTS, Account, BASE, BOUNDARY, CREDENTIALS, INVALID_CREDENTIALS, INVALID_TOKEN,
    NEW_PASSWORD, PASSWORD, SETTINGS, VERIFY, World, assert_refused, string_field,
};
use crate::common;

// ─────────────────────  Scenario 1: the onboarding walk  ─────────────────────

/// The onboarding scenario end to end: a client registers itself, opens an
/// account with the credential that answers, finishes the profile — display
/// name, note, one metadata field, an uploaded avatar — sees the whole
/// result on the public read, changes the password, which revokes the
/// account's *other* credential, and signs in again with the new password.
#[tokio::test]
async fn onboarding_walks_from_app_registration_to_a_new_password() {
    let db = common::test_db().await;
    let world = World::open(db.pool().clone()).await;

    // Step 1: the client's own credential reads itself, without its secret.
    app_reads_itself(&world).await;

    // Step 2: registration over the app-only bearer, and the credential it
    // hands out already authenticates the new account.
    let alice = world.register("alice").await;
    let fresh = common::api_get(&world.app, Some(&alice.token), VERIFY).await;
    assert_eq!(
        string_field(&fresh.json_value(), "username"),
        "alice",
        "the token is the new account's own"
    );
    assert_eq!(
        fresh.json_value()["note"],
        "",
        "and the profile is still blank"
    );

    // Step 3: the profile write, in one multipart body of text and file.
    let written = finish_profile(&world, &alice).await;

    // Step 4: the client's own settings store, which the account carries.
    settings_round_trip(&world, &alice).await;

    // Step 5: the public read shows exactly what the owner stored.
    profile_is_public(&world, &alice, &written).await;

    // Step 6: the password change revokes the other session, keeps this
    // one, and the old password stops opening anything.
    password_change_revokes_the_other_session(&world, &alice).await;

    // Step 7: the new password logs in.
    let again = world.login_with("alice", NEW_PASSWORD).await;
    let read = common::api_get(&world.app, Some(&again), VERIFY).await;
    common::assert_answer(&read, StatusCode::OK, "the login with the new password");
    assert_eq!(
        string_field(&read.json_value(), "username"),
        "alice",
        "the same account, freshly signed in"
    );
}

/// `GET /api/v1/apps/verify_credentials` with the client's own
/// `client_credentials` bearer: the application behind the token, with no
/// secret on the wire.
async fn app_reads_itself(world: &World) {
    let bearer = world.app_token().await;
    let response =
        common::api_get(&world.app, Some(&bearer), "/api/v1/apps/verify_credentials").await;
    common::assert_answer(&response, StatusCode::OK, "the app's own read");
    let body = response.json_value();
    assert_eq!(
        body["name"], "Scenario Client",
        "the entity names the client the registration stated: {body}"
    );
    assert!(
        body.get("client_secret").is_none(),
        "and `verify_credentials` keeps the secret off the wire: {body}"
    );
}

/// The `PATCH /api/v1/accounts/update_credentials` walk: display name,
/// note, one metadata field and an uploaded avatar in a single multipart
/// body, answered as the owner entity that now carries all four.
async fn finish_profile(world: &World, alice: &Account) -> Value {
    let response = common::TestRequest::patch(CREDENTIALS)
        .bearer(&alice.token)
        .multipart(
            BOUNDARY,
            &[
                common::MultipartPart::text("display_name", "Ada Lovelace"),
                common::MultipartPart::text("note", "analysing the engine"),
                common::MultipartPart::text("fields_attributes[0][name]", "Web"),
                common::MultipartPart::text("fields_attributes[0][value]", "https://ada.example"),
                common::MultipartPart::file(
                    "avatar",
                    "ada.png",
                    "image/png",
                    Bytes::from(common::images::png()),
                ),
            ],
        )
        .send(&world.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the profile write");
    let entity = response.json_value();
    assert_eq!(
        entity["display_name"], "Ada Lovelace",
        "the display name lands: {entity}"
    );
    assert_eq!(
        entity["note"], "<p>analysing the engine</p>",
        "the note is stored and rendered"
    );
    assert_eq!(
        entity["fields"][0]["name"], "Web",
        "the metadata field arrives with its label"
    );
    assert_eq!(
        entity["source"]["fields"][0]["value"], "https://ada.example",
        "and its raw text reads back beside the render"
    );
    let avatar = string_field(&entity, "avatar");
    assert!(
        avatar.starts_with(&format!("{BASE}/media/")),
        "the uploaded avatar is served from this instance: {avatar}"
    );
    entity
}

/// The client's own settings store, walked as `PATCH`/`GET
/// /api/v1/pleroma/settings/{app}`: nothing stored at first, the merged
/// object after each write, and the whole per-app store on the account's
/// own `verify_credentials`.
async fn settings_round_trip(world: &World, alice: &Account) {
    let empty = common::api_get(&world.app, Some(&alice.token), SETTINGS).await;
    common::assert_answer(&empty, StatusCode::OK, "the first settings read");
    assert_eq!(
        empty.json_value(),
        json!({}),
        "a new account stores nothing"
    );

    let patched = common::api_patch(
        &world.app,
        Some(&alice.token),
        SETTINGS,
        &json!({"theme": "dark", "timeline": {"show_reblogs": false}}),
    )
    .await;
    common::assert_answer(&patched, StatusCode::OK, "the first settings write");
    assert_eq!(
        patched.json_value(),
        json!({"theme": "dark", "timeline": {"show_reblogs": false}}),
        "the write answers the object it merged"
    );

    let merged = common::api_patch(
        &world.app,
        Some(&alice.token),
        SETTINGS,
        &json!({"timeline": {"muted_words": ["now"]}}),
    )
    .await;
    common::assert_answer(&merged, StatusCode::OK, "the deep merge");
    assert_eq!(
        merged.json_value(),
        json!({"theme": "dark", "timeline": {"show_reblogs": false, "muted_words": ["now"]}}),
        "a nested write joins what is stored instead of replacing it"
    );
    let read = common::api_get(&world.app, Some(&alice.token), SETTINGS).await;
    common::assert_answer(&read, StatusCode::OK, "the settings read back");
    assert_eq!(read.json_value(), merged.json_value(), "the same object");

    let owner = common::api_get(&world.app, Some(&alice.token), VERIFY).await;
    common::assert_answer(&owner, StatusCode::OK, "the owner's account read");
    assert_eq!(
        owner.json_value()["pleroma"]["settings_store"]["mobile"],
        merged.json_value(),
        "and the store rides the account entity under its app"
    );
}

/// What the profile write stored is what anyone reads: the public account
/// route carries the same name, bio and field, and the avatar URL it names
/// serves the uploaded bytes.
async fn profile_is_public(world: &World, alice: &Account, written: &Value) {
    let shown = common::api_get(&world.app, None, &format!("{ACCOUNTS}/{}", alice.id)).await;
    common::assert_answer(&shown, StatusCode::OK, "the public account read");
    let entity = shown.json_value();
    assert_eq!(entity["display_name"], written["display_name"], "the name");
    assert_eq!(entity["note"], written["note"], "the bio");
    assert_eq!(entity["fields"], written["fields"], "the metadata");
    let avatar = string_field(&entity, "avatar");
    let served = common::api_get(
        &world.app,
        None,
        avatar.strip_prefix(BASE).unwrap_or(&avatar),
    )
    .await;
    common::assert_answer(&served, StatusCode::OK, "the stored avatar file");
    assert_eq!(
        served.body.as_ref(),
        common::images::normalized(&common::images::png()),
        "the uploaded image, encoded again"
    );
}

/// The password change and what it costs the account's other credentials:
/// a second session's token dies, the credential that ran the change keeps
/// working (Pleroma's whitelist: that session's password is what the user
/// just typed), and the old password opens nothing.
async fn password_change_revokes_the_other_session(world: &World, alice: &Account) {
    let other = world.login("alice").await;
    let changed = common::api_post(
        &world.app,
        Some(&alice.token),
        "/api/pleroma/change_password",
        &json!({
            "password": PASSWORD,
            "new_password": NEW_PASSWORD,
            "new_password_confirmation": NEW_PASSWORD,
        }),
    )
    .await;
    common::assert_answer(&changed, StatusCode::OK, "the password change");
    assert_eq!(
        changed.json_value()["status"],
        "success",
        "Pleroma's success body"
    );

    let revoked = common::api_get(&world.app, Some(&other), VERIFY).await;
    assert_refused(
        &revoked,
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "the other session's token",
    );
    let keeper = common::api_get(&world.app, Some(&alice.token), VERIFY).await;
    common::assert_answer(&keeper, StatusCode::OK, "the credential the change ran on");
    let stale = common::login_password(&world.app, &world.client, "alice", PASSWORD).await;
    assert_refused(
        &stale,
        StatusCode::BAD_REQUEST,
        INVALID_CREDENTIALS,
        "the old password after the change",
    );
}
