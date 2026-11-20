//! The gates over the admin config surface: the store, its descriptions
//! and `need_reboot` asking `admin:read` and the `POST /config` and the
//! restart request asking `admin:write`, each accepting its narrow
//! `admin:*:config` alias and refusing the other class, the moderator
//! and the token-less alike.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::services::runtime_config::RuntimeConfig;

use super::config_writes::{
    CONFIG, CONFIG_DESCRIPTIONS, NEED_REBOOT, PROCESS_FLAGS_LOCK, RESTART, post_configs, wire,
};
use super::support::{
    INVALID_TOKEN, NO_ADMIN_READ, NO_ADMIN_WRITE, NO_TOKEN, NOT_ADMIN, admin, moderator,
};
use crate::common::{self, TestRequest};

/// The config store's own gate, and the scope class every config route
/// asks for: the reads (the store, its descriptions and `need_reboot`)
/// for `admin:read`, the POST and the restart request for
/// `admin:write`, each accepting its narrow `admin:*:config` alias; the
/// write scope never grants the read and vice versa, a moderator is
/// refused the role, and no token is 401. The reboot pair's own
/// successful paths are the gate test below it.
#[tokio::test]
async fn config_routes_answer_the_admin_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, no_scope) = admin(&pool, "plainadmin", "read write").await;
    let (_, read_only) = admin(&pool, "configread", "admin:read:config").await;
    let (_, write_only) = admin(&pool, "configwrite", "admin:write:config").await;
    let (_, mod_write) = moderator(&pool, "configmod", "admin:write:config").await;

    let mut refused = Vec::new();
    for route in [CONFIG, CONFIG_DESCRIPTIONS, NEED_REBOOT] {
        refused.push(TestRequest::get(route).bearer(&no_scope).send(&app).await);
    }
    common::assert_all_refused(
        &refused,
        StatusCode::FORBIDDEN,
        NO_ADMIN_READ,
        "an admin without any admin:read grant",
    );

    let posted = post_configs(
        &app,
        &write_only,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("name", json!("WriteOnly Works"))]),
        }]),
    )
    .await;
    common::assert_answer(&posted, StatusCode::OK, "the narrow write scope posts");

    let mut read_with_write = Vec::new();
    for route in [CONFIG, CONFIG_DESCRIPTIONS, NEED_REBOOT] {
        read_with_write.push(TestRequest::get(route).bearer(&write_only).send(&app).await);
    }
    common::assert_all_refused(
        &read_with_write,
        StatusCode::FORBIDDEN,
        NO_ADMIN_READ,
        "the write scope never grants the read",
    );

    let read_ok = TestRequest::get(CONFIG).bearer(&read_only).send(&app).await;
    common::assert_answer(&read_ok, StatusCode::OK, "the narrow read scope reads");

    let post_with_read = post_configs(
        &app,
        &read_only,
        json!([{ "group": ":pleroma", "key": ":instance", "delete": true }]),
    )
    .await;
    common::assert_all_refused(
        &[post_with_read],
        StatusCode::FORBIDDEN,
        NO_ADMIN_WRITE,
        "the read scope never grants the write",
    );

    let not_admin = post_configs(
        &app,
        &mod_write,
        json!([{ "group": ":pleroma", "key": ":instance", "delete": true }]),
    )
    .await;
    common::assert_all_refused(
        &[not_admin],
        StatusCode::FORBIDDEN,
        NOT_ADMIN,
        "a moderator with the write scope",
    );

    let anonymous = TestRequest::get(CONFIG).send(&app).await;
    common::assert_all_refused(
        &[anonymous],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "an anonymous GET",
    );
    let anonymous_post = post_configs(&app, "no-such-token", json!([])).await;
    common::assert_all_refused(
        &[anonymous_post],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "an unknown token on the POST",
    );
}

// ---------------------------------------------------------------------------
// The admin config descriptions and the reboot pair (WBE-207): the
// documentation payload `GET /config/descriptions` over the registry
// metadata (Pleroma's `ConfigController.descriptions`), the process-wide
// `GET /need_reboot` flag and the `GET /restart` request
// (`AdminAPIController`'s pair).
// ---------------------------------------------------------------------------

/// The descriptions and reboot pair's gate: the reads take the narrow
/// `admin:read:config`, the restart request takes `admin:write:config` —
/// and refuses the read scope, a moderator and an anonymous request
/// before it raises anything. The successful request answers `{}`.
#[tokio::test]
async fn the_reboot_pair_answers_the_admin_gate() {
    let _flag_guard = PROCESS_FLAGS_LOCK.lock().await;
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, no_scope) = admin(&pool, "rebootplain", "read write").await;
    let (_, read_only) = admin(&pool, "rebootread", "admin:read:config").await;
    let (_, write_only) = admin(&pool, "rebootwrite", "admin:write:config").await;
    let (_, mod_write) = moderator(&pool, "rebootmod", "admin:write:config").await;

    let write_denied = TestRequest::get(RESTART).bearer(&no_scope).send(&app).await;
    common::assert_all_refused(
        &[write_denied],
        StatusCode::FORBIDDEN,
        NO_ADMIN_WRITE,
        "an admin without any admin:write grant on the restart route",
    );

    let mut reads = Vec::new();
    for route in [CONFIG_DESCRIPTIONS, NEED_REBOOT] {
        reads.push(TestRequest::get(route).bearer(&read_only).send(&app).await);
    }
    for read in &reads {
        common::assert_answer(read, StatusCode::OK, "the narrow read scope reads");
    }

    let read_denied = TestRequest::get(RESTART)
        .bearer(&read_only)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[read_denied],
        StatusCode::FORBIDDEN,
        NO_ADMIN_WRITE,
        "the read scope never grants the restart",
    );
    let mod_denied = TestRequest::get(RESTART)
        .bearer(&mod_write)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[mod_denied],
        StatusCode::FORBIDDEN,
        NOT_ADMIN,
        "a moderator asking for the restart",
    );
    let anonymous = TestRequest::get(RESTART).send(&app).await;
    common::assert_all_refused(
        &[anonymous],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "an anonymous restart request",
    );

    let restart_ok = TestRequest::get(RESTART)
        .bearer(&write_only)
        .send(&app)
        .await;
    common::assert_answer(
        &restart_ok,
        StatusCode::OK,
        "the narrow write scope restarts",
    );
    assert_eq!(
        restart_ok.json_value(),
        json!({}),
        "Pleroma's empty answer, and the shutdown flag is what the request raises"
    );
    assert!(
        RuntimeConfig::restart_requested(),
        "the accepted request is the flag's only origin"
    );
    RuntimeConfig::reset_process_flags();
}
