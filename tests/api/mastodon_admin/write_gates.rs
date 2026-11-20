//! The gate in front of the account writes over HTTP (WBE-213): the five
//! writes are an admin's — where the reads stop at staff — and the scope half
//! is the accounts write scope, with the refusals naming the class root.

use axum::http::StatusCode;
use wired_be::models::user::{Flag, NewUser};

use super::support::{NO_TOKEN, USER_REQUIRED, create};
use super::write_support::{
    NO_ADMIN_WRITE, NOT_ADMIN, logged, row_state, seed_writes, write_request,
};
use crate::common;

/// The five writes are an admin's: no token, an unknown one and an app-only
/// one are the shared refusals on every route, and a moderator — who reads
/// this surface — is told they are not an admin, as is a plain account's
/// token, however well scoped.
#[tokio::test]
async fn the_writes_demand_an_admin_user() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let pool = seeded.pool.clone();
    let moddy = create(&pool, "moddy", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let moderator = common::create_app_and_token(&pool, &moddy, "admin:write").await;
    let stranger = common::create_app_and_token(&pool, &seeded.plain, "admin:write").await;
    let app_only = common::create_app_only_token(&pool, "admin:write").await;
    let id = seeded.plain.id;

    for suffix in ["", "enable", "approve", "reject", "action"] {
        let anonymous = write_request(&seeded.app, None, id, suffix).await;
        common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token at all");
        common::assert_error(
            &anonymous,
            NO_TOKEN,
            &format!("the route's suffix {suffix:?}"),
        );

        let bogus = write_request(&seeded.app, Some("nope"), id, suffix).await;
        common::assert_answer(
            &bogus,
            StatusCode::UNAUTHORIZED,
            "a token that resolves nowhere",
        );

        let unbound = write_request(&seeded.app, Some(&app_only), id, suffix).await;
        common::assert_answer(&unbound, StatusCode::FORBIDDEN, "an app-only token");
        common::assert_error(&unbound, USER_REQUIRED, "asks for a user");

        for (credential, label) in [(&moderator, "a moderator"), (&stranger, "a plain account")] {
            let outsider = write_request(&seeded.app, Some(credential), id, suffix).await;
            common::assert_answer(
                &outsider,
                StatusCode::FORBIDDEN,
                &format!("{label} on {suffix:?}"),
            );
            common::assert_error(&outsider, NOT_ADMIN, "is refused as not an admin");
        }
    }

    let (active, _, _) = row_state(&pool, id).await.expect("the row is there");
    assert!(active, "and every refusal left the flag alone");
    let (_, pending, _) = row_state(&pool, seeded.newbie.id)
        .await
        .expect("the pending row is there");
    assert!(pending, "nor did any of them approve or reject anything");
    assert!(
        logged(&pool, "delete").await.is_empty() && logged(&pool, "activate").await.is_empty(),
        "and no refusal wrote the log"
    );
}

/// The scope half of the gate is `admin:write:accounts`: the class root and
/// the area scope both pass on all five routes, and another area, the read
/// class, an ordinary `write` or no scope at all refuse with the class root
/// named.
#[tokio::test]
async fn the_writes_demand_the_accounts_write_scope() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let pool = seeded.pool.clone();

    let granted = common::create_app_and_token(&pool, &seeded.root, "admin:write").await;
    for (suffix, target, label) in [
        ("", seeded.plain.id, "the delete"),
        ("enable", seeded.asleep.id, "the enable"),
        ("approve", seeded.newbie.id, "the approve"),
        ("reject", seeded.quiet.id, "the reject"),
        ("action", seeded.asleep.id, "the umbrella"),
    ] {
        let answered = write_request(&seeded.app, Some(&granted), target, suffix).await;
        common::assert_answer(
            &answered,
            StatusCode::OK,
            &format!("the `admin:write` root on {label}"),
        );
    }
    let scoped = common::create_app_and_token(&pool, &seeded.root, "admin:write:accounts").await;
    let answered = write_request(&seeded.app, Some(&scoped), seeded.asleep.id, "enable").await;
    common::assert_answer(
        &answered,
        StatusCode::OK,
        "the area scope on the enable route",
    );

    for scopes in [
        "admin:write:reports",
        "admin:read:accounts",
        "admin:read",
        "write",
        "",
    ] {
        let token = common::create_app_and_token(&pool, &seeded.root, scopes).await;
        for suffix in ["", "enable", "approve", "reject", "action"] {
            let refused = write_request(&seeded.app, Some(&token), seeded.asleep.id, suffix).await;
            common::assert_answer(
                &refused,
                StatusCode::FORBIDDEN,
                &format!("`{scopes}` on {suffix:?}"),
            );
            common::assert_error(
                &refused,
                NO_ADMIN_WRITE,
                &format!("`{scopes}` names the class root"),
            );
        }
    }
}
