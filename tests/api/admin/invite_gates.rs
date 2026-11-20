//! The invite routes' gates: the scope half asking `admin:read:invites`
//! for the listing and `admin:write:invites` for the three POSTs, each
//! accepting its narrow alias and refusing the other class, and the role
//! half refusing a moderator, a regular user, an app-only token and a
//! request without any token at all.

use axum::http::StatusCode;
use serde_json::json;

use super::invites::{
    INVITE_CREATE, INVITE_EMAIL, INVITE_LIST, INVITE_REVOKE, INVITES_DISABLED, post_invite,
};
use super::support::{
    INVALID_TOKEN, NO_ADMIN_READ, NO_ADMIN_WRITE, NO_TOKEN, NOT_ADMIN, USER_REQUIRED, admin,
    moderator, regular,
};
use crate::common::{self, TestRequest};

/// The invite routes' scope half: the listing asks `admin:read:invites`
/// and the writes `admin:write:invites`, each accepting its narrow alias
/// and refusing the other class.
#[tokio::test]
async fn invite_routes_answer_the_scope_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, read_only) = admin(&pool, "inviteread", "admin:read:invites").await;
    let (_, write_only) = admin(&pool, "invitewrite", "admin:write:invites").await;
    let (_, unscoped) = admin(&pool, "inviteplain", "read write").await;

    let created = post_invite(&app, &write_only, INVITE_CREATE, json!({})).await;
    common::assert_answer(&created, StatusCode::OK, "the narrow write scope creates");
    let emailed = post_invite(
        &app,
        &write_only,
        INVITE_EMAIL,
        json!({"email": "a@b.example"}),
    )
    .await;
    common::assert_all_refused(
        &[emailed],
        StatusCode::BAD_REQUEST,
        INVITES_DISABLED,
        "the write scope reaches the gate on the default instance",
    );

    let listed = TestRequest::get(INVITE_LIST)
        .bearer(&read_only)
        .send(&app)
        .await;
    common::assert_answer(&listed, StatusCode::OK, "the narrow read scope lists");

    let revoke_with_read =
        post_invite(&app, &read_only, INVITE_REVOKE, json!({"token": "x"})).await;
    let create_with_read = post_invite(&app, &read_only, INVITE_CREATE, json!({})).await;
    common::assert_all_refused(
        &[revoke_with_read, create_with_read],
        StatusCode::FORBIDDEN,
        NO_ADMIN_WRITE,
        "the read scope never writes",
    );

    let list_with_write = TestRequest::get(INVITE_LIST)
        .bearer(&write_only)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[list_with_write],
        StatusCode::FORBIDDEN,
        NO_ADMIN_READ,
        "the write scope never lists",
    );

    let unscoped_post = post_invite(&app, &unscoped, INVITE_CREATE, json!({})).await;
    common::assert_all_refused(
        &[unscoped_post],
        StatusCode::FORBIDDEN,
        NO_ADMIN_WRITE,
        "an admin with no admin:write grant",
    );
}

/// The invite routes' role and credential half: a moderator or a regular
/// user holding the invite scopes is refused the role, an app-only token
/// the user, and no token at all the credentials.
#[tokio::test]
async fn invite_routes_answer_the_role_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let scopes = "admin:read:invites admin:write:invites";
    let (_, mod_invites) = moderator(&pool, "invitemod", scopes).await;
    let (_, user_invites) = regular(&pool, "inviteuser", scopes).await;
    let app_only = common::create_app_only_token(&pool, scopes).await;

    for (token, who) in [
        (&mod_invites, "a moderator"),
        (&user_invites, "a regular user"),
    ] {
        let refused = TestRequest::get(INVITE_LIST).bearer(token).send(&app).await;
        common::assert_all_refused(&[refused], StatusCode::FORBIDDEN, NOT_ADMIN, who);
        let posted = post_invite(&app, token, INVITE_CREATE, json!({})).await;
        common::assert_all_refused(&[posted], StatusCode::FORBIDDEN, NOT_ADMIN, who);
    }

    let app_only_refused = TestRequest::get(INVITE_LIST)
        .bearer(&app_only)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[app_only_refused],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "an app-only token",
    );

    let anonymous = TestRequest::get(INVITE_LIST).send(&app).await;
    common::assert_all_refused(
        &[anonymous],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "an anonymous GET",
    );
    let bogus = post_invite(&app, "no-such-token", INVITE_CREATE, json!({})).await;
    common::assert_all_refused(
        &[bogus],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "an unknown token on the POST",
    );
}
