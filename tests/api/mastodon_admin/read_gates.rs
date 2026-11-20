//! The gate in front of the admin account reads over HTTP (WBE-210): every
//! route is a staff user's holding the accounts read scope, and the refusals
//! are the shared 401 and 403 bodies.

use serde_json::json;

use super::support::{
    INVALID_TOKEN, LISTED, NO_ADMIN_READ, NO_TOKEN, NOT_STAFF, USER_REQUIRED, V1, V2, listed, read,
    seed,
};
use crate::common::{self, TestRequest};

/// Every route is staff's: no token, an unknown token and an app-only token are
/// refused, and so is a valid user token whose account holds neither role —
/// while a moderator gets the very same answers as the admin who seeded them.
#[tokio::test]
async fn the_gate_demands_a_staff_user() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let pool = db.pool().clone();
    let moderator = common::create_app_and_token(&pool, &seeded.moddy, "admin:read").await;
    let stranger = common::create_app_and_token(&pool, &seeded.plain, "admin:read").await;
    let app_only = common::create_app_only_token(&pool, "admin:read").await;

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
        let outsider = TestRequest::get(prefix)
            .bearer(&stranger)
            .send(&seeded.app)
            .await;
        common::assert_error(
            &outsider,
            NOT_STAFF,
            &format!("a non-staff token on {label}"),
        );
        let answered = read(&seeded.app, &moderator, prefix).await;
        assert_eq!(
            listed(&answered),
            LISTED.to_vec(),
            "a moderator reads what an admin reads on {label}"
        );
    }

    let show = format!("{V1}/{}", seeded.root.id);
    let outsider = TestRequest::get(&show)
        .bearer(&stranger)
        .send(&seeded.app)
        .await;
    common::assert_error(&outsider, NOT_STAFF, "a non-staff token on the show route");
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
