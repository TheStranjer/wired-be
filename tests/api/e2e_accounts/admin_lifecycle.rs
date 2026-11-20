//! Scenario 6: admin user management — an admin signed in over HTTP
//! creates an account through the admin API, tags it, promotes it,
//! rewrites its credentials, deactivates and re-activates it, and finally
//! deletes it, every effect read back through the user-facing API.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    ACCOUNTS, ADMIN_USERS, DISABLED, NOT_FOUND, PASSWORD, VERIFY, World, address_of,
    assert_refused, string_field,
};
use crate::common;

// ────────────────────  Scenario 6: admin user management  ────────────────────

/// Admin user management end to end: an admin created as the fixture and
/// signed in over HTTP creates an account through the admin API, tags it,
/// promotes it, rewrites its credentials, deactivates and re-activates it,
/// and finally deletes it — every effect read back through the
/// user-facing API the account itself uses.
#[tokio::test]
async fn an_admin_walks_a_user_from_create_to_delete() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _root = common::create_admin_with_password(&pool, "root", PASSWORD).await;
    let world = World::open(pool).await;
    let admin = world.login("root").await;

    // Step 1: the admin creates a user, which then signs in over HTTP, and
    // a tag the public profile carries.
    let (id, token) = created_then_tagged(&world, &admin).await;

    // Step 2: the role grant the account itself can read, and the
    // credentials rewrite the profile shows.
    promoted_and_rewritten(&world, &admin, &id, &token).await;

    // Step 3: the deactivation closes the account, the activation opens it.
    deactivated_then_activated(&world, &admin, &token).await;

    // Step 4: the delete, swept by the queue, takes the profile with it.
    let deleted = common::api_delete(
        &world.app,
        Some(&admin),
        ADMIN_USERS,
        &json!({"nicknames": ["madeup"]}),
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the admin delete");
    assert_eq!(
        deleted.json_value(),
        json!(["madeup"]),
        "the started nickname"
    );
    let ran = world.drain().await;
    assert!(ran >= 1, "the delete sweep ran, got {ran}");
    let gone = common::api_get(&world.app, None, &format!("{ACCOUNTS}/{id}")).await;
    assert_refused(
        &gone,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the profile of a deleted account",
    );
    let refusal = common::login_password(&world.app, &world.client, "madeup", PASSWORD).await;
    assert_eq!(
        refusal.status,
        StatusCode::BAD_REQUEST,
        "and its password no longer resolves: {}",
        refusal.text()
    );
}

/// The admin creation, answered per item, and the sign-in it makes
/// possible: the new account's id and bearer token, beside the tag the
/// public profile then carries.
///
/// # Panics
///
/// If the create or the sign-in is refused, or an answer carries no id.
async fn created_then_tagged(world: &World, admin: &str) -> (String, String) {
    let created = common::api_post(
        &world.app,
        Some(admin),
        ADMIN_USERS,
        &json!({"users": [{
            "nickname": "madeup",
            "email": address_of("madeup"),
            "password": PASSWORD,
        }]}),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "the admin create");
    assert_eq!(
        created.json_value()[0]["code"],
        200,
        "the per-item success entry: {}",
        created.json_value()
    );
    let token = world.login("madeup").await;
    let signed_in = common::api_get(&world.app, Some(&token), VERIFY).await;
    common::assert_answer(
        &signed_in,
        StatusCode::OK,
        "the created account signs straight in",
    );
    assert_eq!(
        string_field(&signed_in.json_value(), "username"),
        "madeup",
        "as itself"
    );
    let id = common::entity_id(&signed_in);

    let tagged = common::api_put(
        &world.app,
        Some(admin),
        &format!("{ADMIN_USERS}/tag"),
        &json!({"nicknames": ["madeup"], "tags": ["Beta testers"]}),
    )
    .await;
    common::assert_answer(&tagged, StatusCode::NO_CONTENT, "the tag write");
    let shown = common::api_get(&world.app, None, &format!("{ACCOUNTS}/{id}")).await;
    common::assert_answer(&shown, StatusCode::OK, "the tagged profile read");
    assert_eq!(
        shown.json_value()["pleroma"]["tags"],
        json!(["beta testers"]),
        "the public profile carries the stored, lowercased tag"
    );
    (id, token)
}

/// The promotion an account reads on itself, and the credentials write the
/// public profile reads back: staff own both ends of the row.
///
/// # Panics
///
/// If either write or either read is refused.
async fn promoted_and_rewritten(world: &World, admin: &str, id: &str, token: &str) {
    let granted = common::api_post(
        &world.app,
        Some(admin),
        &format!("{ADMIN_USERS}/madeup/permission_group/admin"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&granted, StatusCode::OK, "the admin role grant");
    assert_eq!(granted.json_value(), json!({"is_admin": true}), "one key");
    let promoted = common::api_get(&world.app, Some(token), VERIFY).await;
    common::assert_answer(&promoted, StatusCode::OK, "the promoted account's read");
    assert_eq!(
        promoted.json_value()["pleroma"]["is_admin"],
        json!(true),
        "and the account's own read says so"
    );

    let rewritten = common::api_patch(
        &world.app,
        Some(admin),
        &format!("{ADMIN_USERS}/madeup/credentials"),
        &json!({"name": "Made Up", "bio": "created by staff"}),
    )
    .await;
    common::assert_answer(&rewritten, StatusCode::OK, "the credentials write");
    assert_eq!(
        rewritten.json_value(),
        json!({"status": "success"}),
        "Pleroma's success map"
    );
    let profile = common::api_get(&world.app, None, &format!("{ACCOUNTS}/{id}")).await;
    common::assert_answer(&profile, StatusCode::OK, "the rewritten profile read");
    assert_eq!(
        profile.json_value()["display_name"],
        "Made Up",
        "the display name the admin set"
    );
    assert_eq!(
        profile.json_value()["note"],
        "<p>created by staff</p>",
        "and the bio it wrote"
    );
}

/// The deactivation and the activation, each proved from the account's own
/// side: its bearer is refused with the gate's words, its login carries the
/// `account_is_disabled` identifier, and both come back with the flag.
async fn deactivated_then_activated(world: &World, admin: &str, token: &str) {
    let off = common::api_patch(
        &world.app,
        Some(admin),
        &format!("{ADMIN_USERS}/deactivate"),
        &json!({"nicknames": ["madeup"]}),
    )
    .await;
    common::assert_answer(&off, StatusCode::OK, "the deactivation");
    assert_eq!(
        off.json_value()["users"][0]["is_active"],
        json!(false),
        "answered as deactivated"
    );
    let denied = common::api_get(&world.app, Some(token), VERIFY).await;
    assert_refused(
        &denied,
        StatusCode::FORBIDDEN,
        DISABLED,
        "the bearer gate's refusal of a disabled account",
    );
    let refused = common::login_password(&world.app, &world.client, "madeup", PASSWORD).await;
    assert_refused(
        &refused,
        StatusCode::FORBIDDEN,
        DISABLED,
        "the login of a disabled account",
    );
    assert_eq!(
        refused.json_value()["identifier"],
        "account_is_disabled",
        "with the grant's own reason code"
    );

    let on = common::api_patch(
        &world.app,
        Some(admin),
        &format!("{ADMIN_USERS}/activate"),
        &json!({"nicknames": ["madeup"]}),
    )
    .await;
    common::assert_answer(&on, StatusCode::OK, "the activation");
    assert_eq!(
        on.json_value()["users"][0]["is_active"],
        json!(true),
        "answered as active again"
    );
    let read = common::api_get(&world.app, Some(token), VERIFY).await;
    common::assert_answer(&read, StatusCode::OK, "the released account reads itself");
}
