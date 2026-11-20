//! Scenario 5: the admin/staff boundary. The staff surface (reports and
//! statuses) accepts a moderator bearer and the admin surface refuses it
//! with the admin sentence in both directions; a plain registered user is
//! refused the staff and admin surfaces alike, and real admin scopes on a
//! non-staff account still fail the role gate; scope-bearing tokens answer
//! their exact sentences — `admin:read` never writes, `admin:write` never
//! reads and an area scope never crosses areas; and the anonymous request
//! and the app-only token are refused before the gates are even weighed.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::support::{
    Account, CONFIG, LOG, M_ADMIN_REPORTS, NO_ADMIN_READ, NO_ADMIN_WRITE, NOT_ADMIN, NOT_STAFF,
    P_ADMIN, PASSWORD, REPORTS, USERS, World, id_of, post_status,
};
use crate::common;

// ────────────────────  Scenario 5: the admin/staff boundary  ────────────────────

/// The staff half of the admin API: reports and statuses, both directions —
/// a moderator bearer walks it end to end.
async fn staff_half(world: &World, target: &Account, status_id: &str) {
    let base = format!("{P_ADMIN}/reports");
    let filed = common::api_post(
        &world.app,
        Some(&target.token),
        REPORTS,
        &json!({"account_id": world.admin_user.id.to_string(), "comment": "staff probe"}),
    )
    .await;
    common::assert_answer(&filed, StatusCode::OK, "the staff-probe filing");
    let report_id = id_of(&filed.json_value());
    for (what, response) in [
        (
            "the report listing",
            common::api_get(&world.app, Some(&world.moderator), &base).await,
        ),
        (
            "the status listing",
            common::api_get(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/statuses"),
            )
            .await,
        ),
        (
            "a status edit",
            common::api_put(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/statuses/{status_id}"),
                &json!({"sensitive": true}),
            )
            .await,
        ),
        (
            "a note",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                &format!("{base}/{report_id}/notes"),
                &json!({"content": "staff note"}),
            )
            .await,
        ),
    ] {
        assert!(
            response.status.is_success(),
            "a moderator was refused {what}: {}",
            response.text(),
        );
    }
}

/// The admin half of the admin API: a moderator is refused every one of
/// these with `403` and the admin sentence, whatever direction the call
/// goes in.
async fn admin_half_denied(world: &World) {
    let refusals = [
        (
            "the config read",
            common::api_get(&world.app, Some(&world.moderator), CONFIG).await,
        ),
        (
            "the config write",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                CONFIG,
                &json!({"configs": [{
                    "group": ":pleroma",
                    "key": ":instance",
                    "value": [{"tuple": ["registrations_open", false]}],
                }]}),
            )
            .await,
        ),
        (
            "the user list",
            common::api_get(&world.app, Some(&world.moderator), USERS).await,
        ),
        (
            "a deactivation",
            common::api_patch(
                &world.app,
                Some(&world.moderator),
                &format!("{USERS}/deactivate"),
                &json!({"nicknames": ["root"]}),
            )
            .await,
        ),
        (
            "the log",
            common::api_get(&world.app, Some(&world.moderator), LOG).await,
        ),
        (
            "the stats",
            common::api_get(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/stats"),
            )
            .await,
        ),
    ];
    assert_refused(&refusals);
    admin_half_denied_writes(world).await;
}

/// The write side of the admin half: relay, webhooks, rules,
/// announcements and the emoji reload are all `403` for a moderator.
async fn admin_half_denied_writes(world: &World) {
    let refusals = [
        (
            "a relay follow",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/relay"),
                &json!({"relay_url": "https://relay.example/inbox"}),
            )
            .await,
        ),
        (
            "a webhook",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/webhooks"),
                &json!({"url": "https://hooks.example/night", "events": ["report.created"]}),
            )
            .await,
        ),
        (
            "a rule",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/rules"),
                &json!({"text": "moderators may not"}),
            )
            .await,
        ),
        (
            "an announcement",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/announcements"),
                &json!({"content": "moderators may not"}),
            )
            .await,
        ),
        (
            "an emoji reload",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/reload_emoji"),
                &Value::Null,
            )
            .await,
        ),
    ];
    assert_refused(&refusals);
}

/// Every refusal answers `403` and the admin sentence.
fn assert_refused(refusals: &[(&str, common::TestResponse)]) {
    for (what, response) in refusals {
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "a moderator was allowed {what}: {}",
            response.text(),
        );
        common::assert_error(response, NOT_ADMIN, &format!("the refusal of {what}"));
    }
}

/// Scenario 5: the staff surface accepts moderators, the admin surface
/// refuses them, scope-bearing tokens answer their exact sentences, an
/// app-only token cannot reach the admin API at all, and an anonymous
/// request is refused before the gate is even considered.
#[tokio::test]
async fn the_admin_surface_refuses_moderators_scopes_and_the_anonymous() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool.clone()).await;
    let reporter = world.register("reporter").await;
    let post = post_status(&world, &reporter, "in scope").await;
    let post_id = id_of(&post);

    staff_half(&world, &reporter, &post_id).await;
    admin_half_denied(&world).await;

    let plain = common::register_via_api(&world.app, &world.client, "plainstaff", PASSWORD).await;
    role_gate_refusals(&world, &pool, &plain).await;
    scope_gate_refusals(&world, &pool).await;
    gate_refusals_for_the_anonymous_and_the_machine(&world, &pool).await;
}

/// A registered user's bearer comes back with the client's own scopes, so
/// this plain account is refused the staff surface with the staff sentence
/// and the admin surface with the admin one — and the same user with real
/// admin scopes still fails the role gate both ways.
async fn role_gate_refusals(world: &World, pool: &PgPool, plain: &str) {
    let staff_refusal = common::api_get(&world.app, Some(plain), M_ADMIN_REPORTS).await;
    assert_eq!(
        staff_refusal.status,
        StatusCode::FORBIDDEN,
        "a plain user is refused the staff reports surface: {}",
        staff_refusal.text(),
    );
    common::assert_error(&staff_refusal, NOT_STAFF, "the plain staff refusal");
    let admin_refusal = common::api_get(&world.app, Some(plain), CONFIG).await;
    assert_eq!(
        admin_refusal.status,
        StatusCode::FORBIDDEN,
        "a plain user is refused the admin config surface: {}",
        admin_refusal.text(),
    );
    assert!(
        admin_refusal.text().contains(NOT_ADMIN) || admin_refusal.text().contains("admin:read"),
        "the config refusal names the admin role or scope: {}",
        admin_refusal.text(),
    );
    let outsider_row = wired_be::models::user::get_by_nickname(pool, "plainstaff")
        .await
        .expect("the outsider reads")
        .expect("the outsider stands");
    let scoped = common::create_app_and_token(pool, &outsider_row, "admin:read admin:write").await;
    let still_staff = common::api_get(&world.app, Some(&scoped), M_ADMIN_REPORTS).await;
    assert_eq!(
        still_staff.status,
        StatusCode::FORBIDDEN,
        "scopes do not mint staff"
    );
    common::assert_error(&still_staff, NOT_STAFF, "the staff refusal by role");
    let still_admin = common::api_get(&world.app, Some(&scoped), CONFIG).await;
    assert_eq!(
        still_admin.status,
        StatusCode::FORBIDDEN,
        "scopes do not mint admins"
    );
    common::assert_error(&still_admin, NOT_ADMIN, "the admin refusal by role");
}

/// Scope-bearing tokens answer their exact sentences in both directions:
/// `admin:read` writes are refused, `admin:write` reads are refused, and a
/// per-area scope never crosses into another area.
async fn scope_gate_refusals(world: &World, pool: &PgPool) {
    let root = &world.admin_user;
    let read_only = common::create_app_and_token(pool, root, "admin:read").await;
    let write_only = common::create_app_and_token(pool, root, "admin:write").await;
    let reports_only = common::create_app_and_token(pool, root, "admin:read:reports").await;

    let denied_write = common::api_post(
        &world.app,
        Some(&read_only),
        &format!("{P_ADMIN}/reports/1/notes"),
        &json!({"content": "no"}),
    )
    .await;
    assert_eq!(
        denied_write.status,
        StatusCode::FORBIDDEN,
        "the read-only write is refused",
    );
    common::assert_error(&denied_write, NO_ADMIN_WRITE, "the read-only refusal");

    let denied_read =
        common::api_get(&world.app, Some(&write_only), &format!("{P_ADMIN}/reports")).await;
    assert_eq!(
        denied_read.status,
        StatusCode::FORBIDDEN,
        "the write-only read is refused",
    );
    common::assert_error(&denied_read, NO_ADMIN_READ, "the write-only refusal");

    let area_read = common::api_get(
        &world.app,
        Some(&reports_only),
        &format!("{P_ADMIN}/reports"),
    )
    .await;
    assert!(
        area_read.status.is_success(),
        "the reports scope reads its area: {}",
        area_read.text(),
    );
    let other_area = common::api_get(
        &world.app,
        Some(&reports_only),
        &format!("{P_ADMIN}/statuses"),
    )
    .await;
    assert_eq!(
        other_area.status,
        StatusCode::FORBIDDEN,
        "the reports scope cannot read statuses",
    );
    common::assert_error(&other_area, NO_ADMIN_READ, "the cross-area refusal");
}

/// The anonymous request is refused before the admin gate, and the
/// app-only token — no user behind it — never reaches the admin surface.
async fn gate_refusals_for_the_anonymous_and_the_machine(world: &World, pool: &PgPool) {
    let anon = common::api_get(&world.app, None, &format!("{P_ADMIN}/reports")).await;
    assert_eq!(
        anon.status,
        StatusCode::UNAUTHORIZED,
        "the anonymous admin read is a 401",
    );
    common::assert_error(&anon, "Invalid credentials", "the anonymous refusal");

    let app_only = common::create_app_only_token(pool, "admin").await;
    let machine = common::api_get(&world.app, Some(&app_only), &format!("{P_ADMIN}/reports")).await;
    assert_eq!(
        machine.status,
        StatusCode::FORBIDDEN,
        "the app-only admin read is refused",
    );
    common::assert_error(
        &machine,
        "This API requires an authenticated user",
        "the app-only refusal",
    );
}
