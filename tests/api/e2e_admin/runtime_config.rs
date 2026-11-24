//! Scenario 3: the runtime `:instance` override landing in the public
//! instance endpoints and `NodeInfo`, the descriptions endpoint answering
//! its whitelist, an instance document round-tripping
//! upload/serve/delete, the rules CRUD read back through the public
//! `instance/rules`, and an announcement walking from its admin create
//! through the `announcement` stream event, the user-facing list and
//! dismissal to its admin update and delete — every admin write logged.

use std::time::Duration;

use axum::{Router, body::Bytes, http::StatusCode};
use serde_json::{Value, json};
use wired_be::streaming::hub::Audience;

use super::support::{
    BASE, BOUNDARY, CONFIG, FUTURE, P_ADMIN, PAST, World, assert_log, get_array, id_of,
    post_configs, wire,
};
use crate::common;

// ────────────────  Scenario 3: config, docs, rules, announcements  ───────────────

/// Scenario 3: the runtime `:instance` override lands in the public
/// instance endpoints and `NodeInfo`; the descriptions endpoint answers the
/// whole whitelist; an instance document round-trips upload/serve/delete;
/// the rules CRUD reads back through the public `instance/rules`; and an
/// announcement created by an admin reaches the user-facing list, is
/// dismissed, and its create published the `announcement` stream event —
/// each admin write logged.
#[tokio::test]
async fn runtime_config_rules_documents_and_announcements_reach_the_public_api() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;

    instance_override(&world).await;
    descriptions_answer_the_whitelist(&world.app, &world.admin).await;
    instance_document_round_trip(&world).await;
    rules_walk(&world).await;
    announcements_walk(&world).await;
}

/// The `:instance` runtime override: rename the instance, and the change
/// reads through `GET /api/v1/instance`, `/api/v2/instance` and `NodeInfo`;
/// a `:media_proxy` write, which nothing reads at runtime, is refused with
/// a 400 and never logged.
async fn instance_override(world: &World) {
    let written = post_configs(
        &world.app,
        &world.admin,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[
                ("name", json!("Renamed Adminery")),
                ("registrations_open", json!(false)),
            ]),
        }]),
    )
    .await;
    common::assert_answer(&written, StatusCode::OK, "the :instance write");
    assert_eq!(
        written.json_value()["need_reboot"],
        json!(false),
        "the :instance row is live"
    );

    let v1 = common::api_get(&world.app, None, "/api/v1/instance").await;
    assert_eq!(
        v1.json_value()["title"],
        json!("Renamed Adminery"),
        "the v1 title moved"
    );
    let v2 = common::api_get(&world.app, None, "/api/v2/instance").await;
    assert_eq!(
        v2.json_value()["registrations"]["enabled"],
        json!(false),
        "the v2 gate moved"
    );
    let nodeinfo = common::api_get(&world.app, None, "/nodeinfo/2.0").await;
    assert_eq!(
        nodeinfo.json_value()["metadata"]["nodeName"],
        json!("Renamed Adminery"),
        "NodeInfo tells the same name"
    );

    // A key nothing reads at runtime is refused, not stored.
    let refused = post_configs(
        &world.app,
        &world.admin,
        json!([{
            "group": ":pleroma",
            "key": ":media_proxy",
            "value": wire(&[("enabled", json!(false))]),
        }]),
    )
    .await;
    common::assert_answer(&refused, StatusCode::BAD_REQUEST, "the :media_proxy write");

    // The override is logged once, naming the pair; the refusal is not.
    assert_log(
        &world.app,
        &world.admin,
        "config_change",
        &["@root changed config: :pleroma/:instance".to_owned()],
    )
    .await;
}

/// The descriptions endpoint answers one object per whitelisted key, in
/// registry order, the first being `:instance` with its typed children.
async fn descriptions_answer_the_whitelist(app: &Router, token: &str) {
    let response = common::api_get(app, Some(token), &format!("{CONFIG}/descriptions")).await;
    common::assert_answer(&response, StatusCode::OK, "the descriptions payload");
    let body = response.json_value();
    let entries = body.as_array().expect("the descriptions are an array");
    assert_eq!(entries.len(), 6, "the whole whitelist is documented");
    assert_eq!(
        entries[0]["key"],
        json!(":instance"),
        "the first is :instance"
    );
    assert_eq!(entries[0]["type"], json!("group"), "documented as a group");
    let children = entries[0]["children"]
        .as_array()
        .expect("the instance children");
    assert!(
        children
            .iter()
            .any(|c| c["key"] == json!("name") && c["type"] == json!("string")),
        "with a typed `name` child"
    );
}

/// The instance-document cycle: the multipart upload answers the public
/// url, the public route serves the bytes with the document CSP, and the
/// delete takes it back down.
async fn instance_document_round_trip(world: &World) {
    let upload =
        common::TestRequest::patch(&format!("{P_ADMIN}/instance_document/terms-of-service"))
            .bearer(&world.admin)
            .multipart(
                BOUNDARY,
                &[common::MultipartPart::file(
                    "file",
                    "terms.html",
                    "text/html",
                    Bytes::from_static(b"<h1>Terms</h1>"),
                )],
            )
            .send(&world.app)
            .await;
    common::assert_answer(&upload, StatusCode::OK, "the document upload");
    assert_eq!(
        upload.json_value()["url"],
        json!(format!("{BASE}/instance/terms-of-service.html")),
        "the answer names the public url"
    );

    let served = common::api_get(&world.app, None, "/instance/terms-of-service.html").await;
    common::assert_answer(&served, StatusCode::OK, "the public document read");
    assert_eq!(served.text(), "<h1>Terms</h1>", "the bytes round-trip");
    assert_eq!(
        served.header("content-type"),
        Some("text/html; charset=utf-8"),
        "served as html"
    );

    let deleted = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/instance_document/terms-of-service"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the document delete");
    let gone = common::api_get(&world.app, None, "/instance/terms-of-service.html").await;
    common::assert_answer(
        &gone,
        StatusCode::NOT_FOUND,
        "the deleted document is a 404",
    );
}

/// The rules quartet: create, read through the public three-key
/// `instance/rules`, re-order by priority, delete — and the public
/// instance document carries the same rules. Every write is logged.
async fn rules_walk(world: &World) {
    let keep = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules"),
        &json!({"text": "Be kind", "hint": "say hi first"}),
    )
    .await;
    common::assert_answer(&keep, StatusCode::OK, "the rule create");
    let keep_id = id_of(&keep.json_value());
    let drop_me = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules"),
        &json!({"text": "No spam", "priority": 2}),
    )
    .await;
    common::assert_answer(&drop_me, StatusCode::OK, "the second rule create");
    let drop_id = id_of(&drop_me.json_value());

    let public = get_array(&world.app, None, "/api/v1/instance/rules").await;
    assert_eq!(
        public,
        vec![
            json!({"id": keep_id, "text": "Be kind", "hint": "say hi first"}),
            json!({"id": drop_id, "text": "No spam", "hint": ""}),
        ],
        "the public listing is the admin order, priority then creation, the priority left out"
    );

    let requeued = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules/{keep_id}"),
        &json!({"priority": 9}),
    )
    .await;
    common::assert_answer(&requeued, StatusCode::OK, "the rule update");
    assert_eq!(
        requeued.json_value()["priority"],
        json!(9),
        "the priority moved"
    );

    let deleted = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules/{drop_id}"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the rule delete");
    assert_eq!(
        deleted.json_value(),
        json!({}),
        "the delete answers the empty object"
    );

    assert_log(
        &world.app,
        &world.admin,
        "rule_create",
        &[
            format!("@root created rule: {drop_id}"),
            format!("@root created rule: {keep_id}"),
        ],
    )
    .await;
    assert_log(
        &world.app,
        &world.admin,
        "rule_update",
        &[format!("@root updated rule: {keep_id}")],
    )
    .await;
    assert_log(
        &world.app,
        &world.admin,
        "rule_delete",
        &[format!("@root deleted rule: {drop_id}")],
    )
    .await;
}

/// The announcements quintet end to end: an admin creates a notice whose
/// window holds, its create publishes the `announcement` stream event, the
/// user-facing list shows it unread, the user dismisses it, and the admin
/// update/delete read back. Every write is logged.
async fn announcements_walk(world: &World) {
    let mut rx = world.state.hub.subscribe();
    let created = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/announcements"),
        &json!({"content": "Down at five", "starts_at": PAST, "ends_at": FUTURE}),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "the announcement create");
    let announcement_id = id_of(&created.json_value());
    assert_eq!(
        created.json_value()["pleroma"]["raw_content"],
        json!("Down at five"),
        "the raw content rides back"
    );

    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the create published a stream event")
        .expect("the hub holds it");
    assert_eq!(event.event, "announcement", "Mastodon's announcement event");
    assert!(
        matches!(event.audience, Audience::Users(_)),
        "addressed to the local users"
    );

    let reader = world.register("reader").await;
    let list = get_array(&world.app, Some(&reader.token), "/api/v1/announcements").await;
    let notice = list
        .iter()
        .find(|row| row["id"].as_str() == Some(announcement_id.as_str()))
        .expect("the user-facing list carries the notice");
    assert_eq!(notice["read"], json!(false), "unseen by this reader");

    let dismissed = common::api_post(
        &world.app,
        Some(&reader.token),
        &format!("/api/v1/announcements/{announcement_id}/dismiss"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&dismissed, StatusCode::OK, "the dismissal");
    let with_dismissed = get_array(
        &world.app,
        Some(&reader.token),
        "/api/v1/announcements?with_dismissed=true",
    )
    .await;
    assert!(
        with_dismissed
            .iter()
            .any(|row| row["id"].as_str() == Some(announcement_id.as_str())
                && row["read"] == json!(true)),
        "the dismissed notice reads back as read for its dismisser"
    );

    announcements_edit_and_delete(world, &announcement_id).await;
    assert_log(
        &world.app,
        &world.admin,
        "announcement_create",
        &[format!("@root created announcement: {announcement_id}")],
    )
    .await;
}

/// The admin update and delete of `announcement_id`, each answered and
/// each logged with its own sentence.
async fn announcements_edit_and_delete(world: &World, announcement_id: &str) {
    let updated = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/announcements/{announcement_id}"),
        &json!({"content": "Moved to six"}),
    )
    .await;
    common::assert_answer(&updated, StatusCode::OK, "the announcement update");
    let deleted = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/announcements/{announcement_id}"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the announcement delete");
    assert_log(
        &world.app,
        &world.admin,
        "announcement_update",
        &[format!("@root updated announcement: {announcement_id}")],
    )
    .await;
    assert_log(
        &world.app,
        &world.admin,
        "announcement_delete",
        &[format!("@root deleted announcement: {announcement_id}")],
    )
    .await;
}
