//! The gate matrix of the maintenance, role and credentials routes
//! (WBE-198): every write route listed with the verb, body and success
//! status it answers with, the refusal matrix showing none of them reaches
//! without an admin, and the scope-class split — the write scopes run the
//! whole write set, the read scope runs the GETs, and the classes never
//! grant each other.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::models::user::{Flag, NewUser};

use super::support::{
    CONFIRM, DISABLE_MFA, FOLLOW, FORCE_RESET, INVALID_TOKEN, NO_ADMIN_READ, NO_ADMIN_WRITE,
    NO_TOKEN, NOT_ADMIN, RESEND, SUGGEST, TAG, UNFOLLOW, UNSUGGEST, USER_REQUIRED, credentials,
    group, group_bulk, group_of, issued_reset, mint, seed,
};
use crate::common::{self, TestRequest};

/// One maintenance route: the verb, the path, the body it carries and the
/// status its success answer has.
struct Route {
    /// The request verb.
    verb: &'static str,
    /// The full path.
    path: String,
    /// The JSON body, `None` for the GETs.
    body: Option<Value>,
    /// The status an accepted request answers.
    status: StatusCode,
}

/// The routes of this ticket's section, each with a body that passes its
/// checks against the seeded fixture.
fn maintenance_routes() -> Vec<Route> {
    vec![
        Route {
            verb: "GET",
            path: issued_reset("plain"),
            body: None,
            status: StatusCode::OK,
        },
        Route {
            verb: "PATCH",
            path: FORCE_RESET.to_owned(),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::NO_CONTENT,
        },
        Route {
            verb: "PATCH",
            path: CONFIRM.to_owned(),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::NO_CONTENT,
        },
        Route {
            verb: "PATCH",
            path: RESEND.to_owned(),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::NO_CONTENT,
        },
        Route {
            verb: "PUT",
            path: DISABLE_MFA.to_owned(),
            body: Some(json!({"nickname": "plain"})),
            status: StatusCode::OK,
        },
        Route {
            verb: "POST",
            path: FOLLOW.to_owned(),
            body: Some(json!({"follower": "plain", "followed": "root"})),
            status: StatusCode::OK,
        },
        Route {
            verb: "POST",
            path: UNFOLLOW.to_owned(),
            body: Some(json!({"follower": "plain", "followed": "root"})),
            status: StatusCode::OK,
        },
        Route {
            verb: "PUT",
            path: TAG.to_owned(),
            body: Some(json!({"nicknames": [], "tags": []})),
            status: StatusCode::NO_CONTENT,
        },
        Route {
            verb: "DELETE",
            path: TAG.to_owned(),
            body: Some(json!({"nicknames": [], "tags": []})),
            status: StatusCode::NO_CONTENT,
        },
    ]
}

/// The routes of the profile-tag pair, the permission-group writes and the
/// suggest pair, each answered the way the seeded fixture allows. They ride
/// the same gate as the maintenance routes: a write scope, then a write.
fn role_routes() -> Vec<Route> {
    vec![
        Route {
            verb: "POST",
            path: group_of("plain", "moderator"),
            body: None,
            status: StatusCode::OK,
        },
        Route {
            verb: "DELETE",
            path: group_of("plain", "moderator"),
            body: None,
            status: StatusCode::OK,
        },
        Route {
            verb: "POST",
            path: group_bulk("admin"),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::OK,
        },
        Route {
            verb: "DELETE",
            path: group_bulk("admin"),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::OK,
        },
        Route {
            verb: "PATCH",
            path: SUGGEST.to_owned(),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::OK,
        },
        Route {
            verb: "PATCH",
            path: UNSUGGEST.to_owned(),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::OK,
        },
    ]
}

/// Everything in `maintenance_routes` plus this ticket's write routes.
fn write_routes() -> Vec<Route> {
    let mut routes = maintenance_routes();
    routes.extend(role_routes());
    routes
}

/// The credentials pair joins the set for the ROLE gate: both endpoints
/// answer 401/403 by role and scope before anything else. So do the two
/// permission-group reads, which are this ticket's only GETs.
fn credentials_routes() -> Vec<Route> {
    vec![
        Route {
            verb: "GET",
            path: credentials("plain"),
            body: None,
            status: StatusCode::OK,
        },
        Route {
            verb: "PATCH",
            path: credentials("plain"),
            body: Some(json!({})),
            status: StatusCode::OK,
        },
        Route {
            verb: "GET",
            path: group("plain"),
            body: None,
            status: StatusCode::OK,
        },
        Route {
            verb: "GET",
            path: group_of("plain", "moderator"),
            body: None,
            status: StatusCode::OK,
        },
    ]
}

/// The write class's own share of the pair: the PATCH — the GET reads
/// through the read class and must not be granted by a write token.
fn credentials_patch() -> Route {
    Route {
        verb: "PATCH",
        path: credentials("plain"),
        body: Some(json!({})),
        status: StatusCode::OK,
    }
}

/// Whether `route` is the forced follow or unfollow, the pair gated
/// `admin:write:follows` instead of `admin:write:accounts`.
fn is_follow_route(route: &Route) -> bool {
    route.path == FOLLOW || route.path == UNFOLLOW
}

/// A [`TestRequest`] for one route; `token` is `None` for the anonymous
/// request the gate must refuse with 401.
fn request(route: &Route, token: Option<&str>) -> TestRequest {
    let base = match route.verb {
        "GET" => TestRequest::get(&route.path),
        "PUT" => TestRequest::put(&route.path),
        "POST" => TestRequest::post(&route.path),
        "DELETE" => TestRequest::delete(&route.path),
        _ => TestRequest::patch(&route.path),
    };
    let with_body = match route.body.as_ref() {
        Some(body) => base.json(body),
        None => base,
    };
    match token {
        Some(bearer) => with_body.bearer(bearer),
        None => with_body,
    }
}

/// None of the maintenance routes answers without an admin: no token 401
/// "Invalid credentials", an unknown token 401 "The access token is
/// invalid", an app-only token 403 naming the missing user, and any
/// non-admin — the moderator included — 403 `{"error":"User is not an
/// admin."}`.
#[tokio::test]
async fn only_admins_reach_the_maintenance_routes() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let staff = common::create_local_user_with(&seeded.pool, "moddy3", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let moderator = mint(&seeded.pool, &staff, "admin:write:accounts").await;
    let outsider = common::create_local_user(&seeded.pool, "gawker2").await;
    let regular = mint(&seeded.pool, &outsider, "admin:write").await;
    let app_only = common::create_app_only_token(&seeded.pool, "admin:write:accounts").await;

    for route in write_routes().into_iter().chain(credentials_routes()) {
        let anonymous = request(&route, None).send(&seeded.app).await;
        common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
        common::assert_error(&anonymous, NO_TOKEN, "no token");

        let forged = request(&route, Some("no-such-token"))
            .send(&seeded.app)
            .await;
        common::assert_answer(&forged, StatusCode::UNAUTHORIZED, "an unknown token");
        common::assert_error(&forged, INVALID_TOKEN, "an unknown token");

        let unbound = request(&route, Some(&app_only)).send(&seeded.app).await;
        common::assert_answer(&unbound, StatusCode::FORBIDDEN, "an app-only token");
        common::assert_error(&unbound, USER_REQUIRED, "an app-only token");

        let moddy = request(&route, Some(&moderator)).send(&seeded.app).await;
        common::assert_answer(&moddy, StatusCode::FORBIDDEN, "a moderator");
        common::assert_error(&moddy, NOT_ADMIN, "a moderator");

        let gawker = request(&route, Some(&regular)).send(&seeded.app).await;
        common::assert_answer(&gawker, StatusCode::FORBIDDEN, "a regular user");
        common::assert_error(&gawker, NOT_ADMIN, "a regular user");
    }
}

/// The class half of the gate, both ways: the write scopes run every
/// maintenance route (the issued mint included, Pleroma's `write:accounts`
/// pair), the read scope is refused the whole set with the write class's
/// 403, and the credentials READ is refused to a write-only token with the
/// read class's — the classes never grant each other.
#[tokio::test]
async fn the_scope_class_sorts_the_maintenance_from_the_reads() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    for scopes in ["admin:write:accounts", "admin:write"] {
        let token = mint(&seeded.pool, &seeded.root, scopes).await;
        let skip_follows = scopes == "admin:write:accounts";
        let routes = write_routes()
            .into_iter()
            .chain([credentials_patch()])
            .filter(|route| !(skip_follows && is_follow_route(route)));
        for route in routes {
            let response = request(&route, Some(&token)).send(&seeded.app).await;
            common::assert_answer(
                &response,
                route.status,
                &format!("{scopes} grants {} {}", route.verb, route.path),
            );
        }
    }

    let write_only = mint(&seeded.pool, &seeded.root, "admin:write:accounts").await;
    let wrong_way = TestRequest::get(&credentials("plain"))
        .bearer(&write_only)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &wrong_way,
        StatusCode::FORBIDDEN,
        "the write class must not read credentials",
    );
    common::assert_error(&wrong_way, NO_ADMIN_READ, "in the read class's words");

    for route in write_routes() {
        let refused = request(&route, Some(&seeded.token)).send(&seeded.app).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("the read scope must not run {} {}", route.verb, route.path),
        );
        common::assert_error(&refused, NO_ADMIN_WRITE, "in the write class's words");
    }
}

/// The follow pair's own area: `admin:write:follows` runs both forced
/// follow routes, while `admin:write:accounts` — the account-moderation
/// grant — and `admin:read:follows` are refused with the write class's
/// 403, as Pleroma's `admin:write:follows` plug has it.
#[tokio::test]
async fn the_follows_scope_runs_exactly_the_forced_follow_pair() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let follows = mint(&seeded.pool, &seeded.root, "admin:write:follows").await;
    let accounts = mint(&seeded.pool, &seeded.root, "admin:write:accounts").await;
    let read_follows = mint(&seeded.pool, &seeded.root, "admin:read:follows").await;

    for route in write_routes().into_iter().filter(is_follow_route) {
        let granted = request(&route, Some(&follows)).send(&seeded.app).await;
        common::assert_answer(
            &granted,
            route.status,
            &format!("admin:write:follows grants {} {}", route.verb, route.path),
        );
        for (token, what) in [
            (&accounts, "admin:write:accounts"),
            (&read_follows, "a read"),
        ] {
            let refused = request(&route, Some(token)).send(&seeded.app).await;
            common::assert_answer(
                &refused,
                StatusCode::FORBIDDEN,
                &format!("{what} must not run {} {}", route.verb, route.path),
            );
            common::assert_error(&refused, NO_ADMIN_WRITE, "in the write class's words");
        }
    }

    for route in write_routes().into_iter().filter(|r| !is_follow_route(r)) {
        let refused = request(&route, Some(&follows)).send(&seeded.app).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!(
                "admin:write:follows must not run {} {}",
                route.verb, route.path
            ),
        );
    }
}
