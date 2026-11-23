//! The gate in front of the admin account reads over HTTP (WBE-210, WBE-460):
//! every route is an admin's holding the accounts read scope, and the
//! refusals are the shared 401 and 403 bodies.

use axum::http::StatusCode;
use serde_json::json;

use super::support::{
    ENTITY_KEYS, INVALID_TOKEN, LISTED, NO_ADMIN_READ, NO_TOKEN, USER_REQUIRED, V1, V2, keys,
    listed, read, seed,
};
use super::write_support::NOT_ADMIN;
use crate::common::{self, TestRequest};

/// Every route is an admin's: no token, an unknown token and an app-only token
/// are refused, and so are a moderator and a plain account, each holding the
/// accounts read scope — the email and addresses the entity carries are no
/// staff member's to read — while an admin gets the listings and the entity.
#[tokio::test]
async fn the_gate_demands_an_admin_user() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let pool = db.pool().clone();
    let moderator = common::create_app_and_token(&pool, &seeded.moddy, "admin:read:accounts").await;
    let stranger = common::create_app_and_token(&pool, &seeded.plain, "admin:read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "admin:read").await;
    let show = format!("{V1}/{}", seeded.root.id);

    for (credential, who) in [(&moderator, "a moderator"), (&stranger, "a plain account")] {
        for route in [V1, V2, show.as_str()] {
            let refused = TestRequest::get(route)
                .bearer(credential)
                .send(&seeded.app)
                .await;
            common::assert_answer(
                &refused,
                StatusCode::FORBIDDEN,
                &format!("{who} on {route}"),
            );
            common::assert_error(&refused, NOT_ADMIN, &format!("{who} on {route}"));
        }
    }

    let shown = read(&seeded.app, &seeded.token, &show).await;
    assert_eq!(
        keys(&shown),
        ENTITY_KEYS.to_vec(),
        "an admin reads the pinned entity shape"
    );

    for (prefix, label) in [(V1, "the v1 listing"), (V2, "the v2 listing")] {
        let anonymous = TestRequest::get(prefix).send(&seeded.app).await;
        common::assert_error(&anonymous, NO_TOKEN, &format!("no token on {label}"));
        let bogus = TestRequest::get(prefix)
            .bearer("nope")
            .send(&seeded.app)
            .await;
        common::assert_error(&bogus, INVALID_TOKEN, &format!("a bogus token on {label}"));
        let unbound = TestRequest::get(prefix)
            .bearer(&app_only)
            .send(&seeded.app)
            .await;
        common::assert_error(
            &unbound,
            USER_REQUIRED,
            &format!("an app-only token on {label}"),
        );
        let answered = read(&seeded.app, &seeded.token, prefix).await;
        assert_eq!(
            listed(&answered),
            LISTED.to_vec(),
            "an admin reads the whole page on {label}"
        );
    }

    let anonymous = TestRequest::get(&show).send(&seeded.app).await;
    common::assert_error(&anonymous, NO_TOKEN, "no token on the show route");
}

/// The scope half of the gate is `admin:read:accounts`: the broad `admin:read`
/// root and the accounts area each pass, and another area, the write class and
/// an ordinary `read` all refuse with the class root named.
#[tokio::test]
async fn the_gate_demands_the_accounts_read_scope() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let pool = db.pool().clone();
    let show = format!("{V1}/{}", seeded.root.id);

    for (scopes, why) in [
        ("admin:read", "the class root"),
        ("admin:read:accounts", "the area scope"),
    ] {
        let token = common::create_app_and_token(&pool, &seeded.root, scopes).await;
        for route in [V1, V2] {
            let answered = read(&seeded.app, &token, route).await;
            assert_eq!(
                listed(&answered),
                LISTED.to_vec(),
                "{why} reads the {route} listing"
            );
        }
        let shown = read(&seeded.app, &token, &show).await;
        assert_eq!(
            shown["id"],
            json!(seeded.root.id.to_string()),
            "{why} reads one account too"
        );
    }

    for (scopes, why) in [
        ("admin:read:reports", "another admin area"),
        ("admin:write:accounts", "the write class"),
        ("admin:write", "the write root"),
        ("read", "an ordinary read"),
        ("", "no scope at all"),
    ] {
        let token = common::create_app_and_token(&pool, &seeded.root, scopes).await;
        let refused = [V1.to_owned(), V2.to_owned(), show.clone()];
        for route in &refused {
            let response = TestRequest::get(route)
                .bearer(&token)
                .send(&seeded.app)
                .await;
            common::assert_error(&response, NO_ADMIN_READ, &format!("{why} on {route}"));
        }
    }
}
