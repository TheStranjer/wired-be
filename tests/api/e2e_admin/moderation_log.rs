//! Scenario 4: the moderation log's completeness. A clean walk of one
//! write per admin surface — `POST /config`, a rule
//! create/update/delete, announcement, tag and emoji reload — reads back
//! through `GET /admin/moderation_log` as exactly one entry each, newest
//! first, worded with Pleroma's sentences, and nothing else.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{FUTURE, LOG, P_ADMIN, PAST, USERS, World, post_configs, wire};
use crate::common;

// ────────────────────────  Scenario 4: log completeness  ────────────────────────

/// Scenario 4: a clean walk of one write per admin surface — `POST /config`, a rule
/// create/update/delete, announcement, tag and emoji reload — reads back
/// through `GET /admin/moderation_log` as exactly one entry each, in the
/// expected counts and worded with Pleroma's sentences, and nothing else.
#[tokio::test]
async fn every_admin_write_records_exactly_one_moderation_log_entry() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let reporter = world.register("witness").await;

    let config_write = post_configs(
        &world.app,
        &world.admin,
        json!([{"group": ":pleroma", "key": ":instance", "value": wire(&[("name", json!("Logged"))])}]),
    )
    .await;
    common::assert_answer(&config_write, StatusCode::OK, "the config write");
    let rule = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules"),
        &json!({"text": "Only this"}),
    )
    .await;
    let rule_id = common::id_of(&rule.json_value());
    let rule_update = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules/{rule_id}"),
        &json!({"priority": 1}),
    )
    .await;
    common::assert_answer(&rule_update, StatusCode::OK, "the rule update");
    let rule_delete = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules/{rule_id}"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&rule_delete, StatusCode::OK, "the rule delete");
    let notice = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/announcements"),
        &json!({"content": "One notice", "starts_at": PAST, "ends_at": FUTURE}),
    )
    .await;
    let notice_id = common::id_of(&notice.json_value());
    let tag_write = common::api_put(
        &world.app,
        Some(&world.admin),
        &format!("{USERS}/tag"),
        &json!({"nicknames": [reporter.nickname], "tags": ["vip"]}),
    )
    .await;
    common::assert_answer(&tag_write, StatusCode::NO_CONTENT, "the tag write");
    let reloaded = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/reload_emoji"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&reloaded, StatusCode::OK, "the emoji reload");

    assert_log_contents(
        &world,
        &[
            "@root reloaded emoji",
            "@root added tags: vip to users: @witness",
            &format!("@root deleted rule: {rule_id}"),
            &format!("@root updated rule: {rule_id}"),
            &format!("@root created announcement: {notice_id}"),
            &format!("@root created rule: {rule_id}"),
            "@root changed config: :pleroma/:instance",
        ],
    )
    .await;
}

/// Reads the whole moderation log back newest-first and compares its
/// messages, sorted, to `expected`.
async fn assert_log_contents(world: &World, expected: &[&str]) {
    let log = common::api_get(
        &world.app,
        Some(&world.admin),
        &format!("{LOG}?page_size=200"),
    )
    .await;
    common::assert_answer(&log, StatusCode::OK, "the moderation-log listing");
    let items = log.json_value()["items"]
        .as_array()
        .expect("the log answers its items")
        .clone();
    let dates: Vec<i64> = items
        .iter()
        .map(|entry| {
            entry["time"]
                .as_i64()
                .unwrap_or_else(|| panic!("an entry carries its time: {entry}"))
        })
        .collect();
    let mut newest_first = dates.clone();
    newest_first.sort_by(|a, b| b.cmp(a));
    assert_eq!(dates, newest_first, "the log lists newest first");
    let mut lines: Vec<String> = items
        .iter()
        .map(|entry| {
            entry["data"]["message"]
                .as_str()
                .unwrap_or_else(|| panic!("an entry carries its message: {entry}"))
                .to_owned()
        })
        .collect();
    lines.sort();
    let mut want: Vec<String> = expected.iter().map(|line| (*line).to_owned()).collect();
    want.sort();
    assert_eq!(lines, want, "exactly one entry per admin write");
}
