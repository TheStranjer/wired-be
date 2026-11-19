//! The show route and the read gate of the admin users API (WBE-195): the
//! fifteen-key admin account entity, the handle and id lookups including
//! the hidden instance actor and the 404 for a segment that names nobody,
//! the admin-and-scope matrix in front of both reads, and the static
//! invite routes that keep their share of the `/users` prefix.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::{
    config::TEST_BASE_URL,
    models::user::{Flag, NewUser},
    util::time::mastodon_time,
};

use super::support::{
    DEFAULT_AVATAR, INVALID_TOKEN, NO_ADMIN_READ, NO_TOKEN, NOT_ADMIN, NOT_FOUND, USER_REQUIRED,
    USERS, mint, page, read, seed, show,
};
use crate::common::{self, TestRequest};

/// The invites listing, whose static route shares the `/users` prefix with
/// the show route's `{nickname}` segment.
const INVITES: &str = "/api/v1/pleroma/admin/users/invites";

/// The show endpoint answers the admin entity: the fifteen keys the ticket
/// pins, the moderation columns verbatim, the roles object, the defaults
/// for the unset ones, and the stored timestamp in the Mastodon format.
#[tokio::test]
async fn the_show_renders_the_admin_account_entity() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let config = common::test_config();
    let entity = read(&seeded.app, &seeded.token, &show("root")).await;
    assert_eq!(
        entity,
        json!({
            "id": seeded.root.id.to_string(),
            "email": "root@example.org",
            "avatar": format!("{}{DEFAULT_AVATAR}", config.frontend_base_url),
            "nickname": "root",
            "display_name": "root",
            "is_active": true,
            "local": true,
            "roles": { "admin": true, "moderator": false },
            "tags": [],
            "is_confirmed": true,
            "is_approved": true,
            "is_suggested": false,
            "url": format!("{TEST_BASE_URL}/users/root"),
            "registration_reason": null,
            "actor_type": "Person",
            "created_at": mastodon_time(&seeded.root.inserted_at),
        }),
        "no counters, no note, nothing else"
    );

    let listed = page(&seeded.app, &seeded.token, "query=beeper").await;
    let bot = &listed["users"].as_array().expect("one account")[0];
    assert_eq!(
        bot["avatar"],
        json!(format!("{TEST_BASE_URL}/uploads/bot.png")),
        "a local account's stored avatar passes through"
    );
    assert_eq!(bot["actor_type"], json!("Service"), "and its actor type");
    assert_eq!(bot["tags"], json!(["bots"]), "and its profile tags");

    let mirror = read(&seeded.app, &seeded.token, &show("faraway@remote.example")).await;
    assert_eq!(mirror["local"], json!(false), "the mirror is not ours");
    assert_eq!(
        mirror["url"],
        json!(seeded.mirror.ap_id),
        "so its ap_id is its url"
    );
    assert_eq!(
        mirror["nickname"],
        json!("faraway@remote.example"),
        "the stored handle"
    );
    assert_eq!(mirror["display_name"], json!("faraway"), "its name");
    assert_eq!(mirror["email"], Value::Null, "and no email at all");
}

/// The show route resolves the path segment as a nickname (citext, so in
/// any case), as a remote handle written plainly or percent-encoded, and as
/// the account's id.
#[tokio::test]
async fn the_show_route_resolves_a_handle_or_an_id() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let plain = read(&seeded.app, &seeded.token, &show("PLAIN")).await;
    assert_eq!(plain["nickname"], json!("plain"), "citext folds the case");
    let handle = read(&seeded.app, &seeded.token, &show("faraway@remote.example")).await;
    let encoded = read(
        &seeded.app,
        &seeded.token,
        &show("faraway%40remote.example"),
    )
    .await;
    assert_eq!(
        encoded, handle,
        "the encoded and the plain handle are the same segment"
    );
    assert_eq!(
        handle["nickname"],
        json!("faraway@remote.example"),
        "and both find the mirror"
    );
    let by_id = read(
        &seeded.app,
        &seeded.token,
        &show(&seeded.mirror.id.to_string()),
    )
    .await;
    assert_eq!(
        by_id["nickname"],
        json!("faraway@remote.example"),
        "an id reaches the same row"
    );
    let by_local_id = read(
        &seeded.app,
        &seeded.token,
        &show(&seeded.root.id.to_string()),
    )
    .await;
    assert_eq!(
        by_local_id["nickname"],
        json!("root"),
        "local accounts included"
    );
}

/// An account the listing hides is still readable by name, and text that is
/// no nickname and no id — including a malformed one — is the 404
/// `{"error":"Not found"}`.
#[tokio::test]
async fn the_show_route_reads_the_hidden_actor_and_refuses_a_miss() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let actor = read(&seeded.app, &seeded.token, &show("internal.fetch")).await;
    assert_eq!(
        actor["nickname"],
        json!("internal.fetch"),
        "naming the instance actor is a lookup, not a listing"
    );
    assert_eq!(
        actor["display_name"],
        json!(seeded.actor.name),
        "and it renders like any other row, named after the instance"
    );
    assert_eq!(
        actor["url"],
        json!(seeded.actor.ap_id),
        "with its own actor id as its url"
    );
    assert_eq!(
        actor["actor_type"],
        json!("Application"),
        "and its Application type"
    );

    for segment in ["nobody", "01934567", "not-a-uuid-at-all"] {
        let response = TestRequest::get(&show(segment))
            .bearer(&seeded.token)
            .send(&seeded.app)
            .await;
        common::assert_answer(
            &response,
            StatusCode::NOT_FOUND,
            "a segment that names nobody",
        );
        common::assert_error(&response, NOT_FOUND, "a segment that names nobody");
    }
    let empty = TestRequest::get(USERS)
        .bearer(&seeded.token)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &empty,
        StatusCode::OK,
        "the bare listing route is not a show",
    );
}

/// The gate: no token is 401, an unusable one 401, an app-only token 403
/// naming the missing user, and a valid non-admin token — a moderator
/// included — 403 `{"error":"User is not an admin."}`. The role is checked
/// before the scope, on both routes.
#[tokio::test]
async fn only_an_admin_reads_the_users_api() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let moddy = common::create_local_user_with(&seeded.pool, "moddy2", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let moddy_token = mint(&seeded.pool, &moddy, "admin:read:accounts").await;
    let regular = common::create_local_user(&seeded.pool, "gawker").await;
    let regular_token = common::create_app_and_token(&seeded.pool, &regular, "admin:read").await;
    let app_only = common::create_app_only_token(&seeded.pool, "admin:read:accounts").await;

    for path in [USERS.to_owned(), show("root")] {
        let anonymous = TestRequest::get(&path).send(&seeded.app).await;
        common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
        common::assert_error(&anonymous, NO_TOKEN, "no token");

        let forged = TestRequest::get(&path)
            .bearer("no-such-token")
            .send(&seeded.app)
            .await;
        common::assert_answer(&forged, StatusCode::UNAUTHORIZED, "an unknown token");
        common::assert_error(&forged, INVALID_TOKEN, "an unknown token");

        let unbound = TestRequest::get(&path)
            .bearer(&app_only)
            .send(&seeded.app)
            .await;
        common::assert_answer(&unbound, StatusCode::FORBIDDEN, "an app-only token");
        common::assert_error(&unbound, USER_REQUIRED, "an app-only token");

        let moderator = TestRequest::get(&path)
            .bearer(&moddy_token)
            .send(&seeded.app)
            .await;
        common::assert_answer(&moderator, StatusCode::FORBIDDEN, "a moderator");
        common::assert_error(&moderator, NOT_ADMIN, "a moderator");

        let outsider = TestRequest::get(&path)
            .bearer(&regular_token)
            .send(&seeded.app)
            .await;
        common::assert_answer(&outsider, StatusCode::FORBIDDEN, "a regular user");
        common::assert_error(&outsider, NOT_ADMIN, "a regular user");
    }
}

/// The scope half of the gate: `admin:read` and `admin:read:accounts` both
/// grant the two reads, and every other grant — another admin area, the
/// write class, an ordinary scope — is the 403 naming `admin:read`.
#[tokio::test]
async fn the_accounts_read_scope_grants_both_endpoints() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let granted = [
        ("broad root", "admin:read"),
        ("the area scope", "admin:read:accounts"),
    ];
    let refused = [
        ("another admin area", "admin:read:reports"),
        ("the write class", "admin:write:accounts"),
        ("an ordinary scope", "read"),
        ("nothing at all", ""),
    ];
    for (what, scopes) in granted {
        let token = mint(&seeded.pool, &seeded.root, scopes).await;
        for path in [USERS.to_owned(), show("plain")] {
            let response = TestRequest::get(&path)
                .bearer(&token)
                .send(&seeded.app)
                .await;
            common::assert_answer(&response, StatusCode::OK, &format!("{what} grants {path}"));
        }
    }
    for (what, scopes) in refused {
        let token = mint(&seeded.pool, &seeded.root, scopes).await;
        for path in [USERS.to_owned(), show("plain")] {
            let response = TestRequest::get(&path)
                .bearer(&token)
                .send(&seeded.app)
                .await;
            common::assert_answer(
                &response,
                StatusCode::FORBIDDEN,
                &format!("{what} must not grant {path}"),
            );
            common::assert_error(&response, NO_ADMIN_READ, what);
        }
    }
}

/// The `/users` prefix holds static invite routes beside the show route's
/// `{nickname}` segment: the router prefers the static path, so the invites
/// listing still answers its own `{"invites": …}` key, and an accounts-scoped
/// token is refused by the *invites* scope gate (403) rather than by a show
/// lookup that found no account named `invites` (which would have been 404).
#[tokio::test]
async fn the_static_invite_routes_keep_their_prefix() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let invites_token = mint(&seeded.pool, &seeded.root, "admin:read:invites").await;
    let invites = read(&seeded.app, &invites_token, INVITES).await;
    assert_eq!(
        invites,
        json!({ "invites": [] }),
        "the invites listing answers its own key, not the show entity"
    );
    let wrong_area = TestRequest::get(INVITES)
        .bearer(&seeded.token)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &wrong_area,
        StatusCode::FORBIDDEN,
        "the accounts scope is not the invites scope",
    );
    common::assert_error(
        &wrong_area,
        NO_ADMIN_READ,
        "the accounts scope is not invites",
    );
}
