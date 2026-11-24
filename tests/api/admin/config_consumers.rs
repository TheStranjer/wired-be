//! The live consumers of the admin config store: the `:mrf*` chain the
//! MRF loader reads back off the instance document the moment a write
//! clears the cache, the `:instance` overrides riding the public v1, v2
//! and `NodeInfo` documents, the `need_reboot` flag staying down for the
//! live keys, and the two keys no runtime consumer reads
//! (`:media_proxy`, `:restrict_unauthenticated`) refused outright.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::services::runtime_config::RuntimeConfig;

use super::config_writes::{CONFIG, PROCESS_FLAGS_LOCK, config_admin, post_configs, wire};
use crate::common::{self, TestRequest};

/// The MRF chain is a live consumer of the store: after a first read has
/// filled the pipeline's cache, a `POST /config` setting `:mrf` policies
/// and `:mrf_simple` lists takes effect at once — the instance document's
/// federation block names `SimplePolicy` and publishes the stored list.
#[tokio::test]
async fn setting_mrf_policies_through_the_api_activates_simple_policy() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configmrf").await;

    let before = TestRequest::get("/api/v1/instance").send(&app).await;
    assert_eq!(
        before.json_value()["pleroma"]["metadata"]["federation"]["mrf_policies"],
        json!([]),
        "the fresh instance runs no policy, and this read fills the cache"
    );

    let written = post_configs(
        &app,
        &token,
        json!([
            {
                "group": ":pleroma",
                "key": ":mrf",
                "value": wire(&[("policies", json!(["simple"]))]),
            },
            {
                "group": ":pleroma",
                "key": ":mrf_simple",
                "value": wire(&[("reject", json!([["evil.example", "bots"]]))]),
            },
        ]),
    )
    .await;
    common::assert_answer(&written, StatusCode::OK, "the :mrf batch");
    assert_eq!(
        written.json_value()["need_reboot"],
        json!(false),
        "the chain reloads without a restart"
    );

    let after = TestRequest::get("/api/v1/instance").send(&app).await;
    let federation = after.json_value()["pleroma"]["metadata"]["federation"].clone();
    assert_eq!(
        federation["mrf_policies"],
        json!(["SimplePolicy"]),
        "the POST cleared the cache and the new chain answers"
    );
    assert_eq!(
        federation["mrf_simple"]["reject"],
        json!(["evil.example"]),
        "the :mrf_simple entry is the policy's own lists"
    );

    let deleted = post_configs(
        &app,
        &token,
        json!([
            { "group": ":pleroma", "key": ":mrf", "delete": true },
            { "group": ":pleroma", "key": ":mrf_simple", "delete": true },
        ]),
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the delete batch");
    let reverted = TestRequest::get("/api/v1/instance").send(&app).await;
    assert_eq!(
        reverted.json_value()["pleroma"]["metadata"]["federation"]["mrf_policies"],
        json!([]),
        "and with the rows gone the env fallback runs the empty chain again"
    );
}

/// The `:instance` overrides ride the public documents: the v1 and v2
/// instance titles, the v1 contact email and registration flags, and the
/// `NodeInfo` name and `openRegistrations` all answer from the stored
/// row until it is deleted.
#[tokio::test]
async fn instance_overrides_show_in_the_public_documents() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configname").await;

    let written = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[
                ("name", json!("Renamed Radio")),
                ("email", json!("desk@example.test")),
                ("registrations_open", json!(false)),
                ("account_approval_required", json!(true)),
            ]),
        }]),
    )
    .await;
    common::assert_answer(&written, StatusCode::OK, "the :instance write");

    let v1 = TestRequest::get("/api/v1/instance").send(&app).await;
    let document = v1.json_value();
    assert_eq!(document["title"], "Renamed Radio", "the v1 title");
    assert_eq!(document["email"], "desk@example.test", "the v1 email");
    assert_eq!(document["registrations"], json!(false), "the v1 flag");
    assert_eq!(
        document["approval_required"],
        json!(true),
        "account_approval_required rides the approval gate"
    );

    let v2 = TestRequest::get("/api/v2/instance").send(&app).await;
    assert_eq!(v2.json_value()["title"], "Renamed Radio", "the v2 title");
    assert_eq!(
        v2.json_value()["registrations"]["enabled"],
        json!(false),
        "and the v2 registration block"
    );

    let nodeinfo = TestRequest::get("/nodeinfo/2.0").send(&app).await;
    let summary = nodeinfo.json_value();
    assert_eq!(
        summary["metadata"]["nodeName"], "Renamed Radio",
        "the NodeInfo name"
    );
    assert_eq!(
        summary["openRegistrations"],
        json!(false),
        "and its registration answer"
    );

    let deleted = post_configs(
        &app,
        &token,
        json!([{ "group": ":pleroma", "key": ":instance", "delete": true }]),
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the delete");
    let back = TestRequest::get("/api/v1/instance").send(&app).await;
    assert_eq!(
        back.json_value()["title"],
        "Wired",
        "the env name is back once the row is gone"
    );
    assert_eq!(
        back.json_value()["registrations"],
        json!(true),
        "and so is the env's open door"
    );
}

/// The `need_reboot` flag follows the registry: no whitelisted key is
/// restart-gated, so a live key's write keeps it down in the POST answer
/// and in the later GET. The test reads the process-wide flag the
/// WBE-207 tests also own, so it holds [`PROCESS_FLAGS_LOCK`].
#[tokio::test]
async fn config_need_reboot_follows_the_changed_keys() {
    let _flag_guard = PROCESS_FLAGS_LOCK.lock().await;
    RuntimeConfig::reset_process_flags();
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configboot").await;

    let instance_write = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("description", json!("Live change"))]),
        }]),
    )
    .await;
    assert_eq!(
        instance_write.json_value()["need_reboot"],
        json!(false),
        ":instance takes effect without a restart"
    );

    let standing = TestRequest::get(CONFIG).bearer(&token).send(&app).await;
    assert_eq!(
        standing.json_value()["need_reboot"],
        json!(false),
        "and the page flag stays down while the stored row stands"
    );
    assert!(
        !RuntimeConfig::need_reboot(),
        "the process-wide flag is untouched"
    );
}

/// `:media_proxy` and `:restrict_unauthenticated` are refused with the
/// not-whitelisted 400 — upserts and deletes alike — because nothing
/// reads a stored row for either key: no `config_entries` row appears,
/// a valid sibling in the same batch is not written, `need_reboot` stays
/// down in the page and in the process-wide flag, and neither key is
/// listed by `GET /config`.
#[tokio::test]
async fn unread_keys_are_refused_and_write_nothing() {
    let _flag_guard = PROCESS_FLAGS_LOCK.lock().await;
    RuntimeConfig::reset_process_flags();
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configunread").await;

    for (key, value) in [
        (
            ":restrict_unauthenticated",
            json!([{"tuple": ["timelines", [{"tuple": ["local", true]}]]}]),
        ),
        (
            ":media_proxy",
            Value::Array(wire(&[("enabled", json!(false))])),
        ),
    ] {
        let upsert = post_configs(
            &app,
            &token,
            json!([
                { "group": ":pleroma", "key": ":instance", "value": wire(&[("name", json!("Sneaky"))]) },
                { "group": ":pleroma", "key": key, "value": value },
            ]),
        )
        .await;
        common::assert_answer(&upsert, StatusCode::BAD_REQUEST, "the upsert");
        common::assert_error(
            &upsert,
            &format!("Config not allowed: :pleroma/{key}"),
            "the key is not whitelisted",
        );

        let delete = post_configs(
            &app,
            &token,
            json!([{ "group": ":pleroma", "key": key, "delete": true }]),
        )
        .await;
        common::assert_answer(&delete, StatusCode::BAD_REQUEST, "the delete");
    }

    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM config_entries")
        .fetch_one(&pool)
        .await
        .expect("the row count reads");
    assert_eq!(
        stored, 0,
        "no refused batch wrote a row, valid sibling included"
    );

    let page = TestRequest::get(CONFIG).bearer(&token).send(&app).await;
    let body = page.json_value();
    assert_eq!(body["need_reboot"], json!(false), "no restart is owed");
    let listed: Vec<&str> = body["configs"]
        .as_array()
        .expect("the configs array")
        .iter()
        .filter_map(|entry| entry["key"].as_str())
        .collect();
    assert!(
        !listed.contains(&":media_proxy") && !listed.contains(&":restrict_unauthenticated"),
        "neither key is offered: {listed:?}"
    );
    assert!(
        !RuntimeConfig::need_reboot(),
        "the process-wide flag was never raised"
    );
}
