//! The live consumers of the admin config store: the `:mrf*` chain the
//! MRF loader reads back off the instance document the moment a write
//! clears the cache, the `:instance` overrides riding the public v1, v2
//! and `NodeInfo` documents, and the `need_reboot` flag following the
//! registry's restart classes while the changed row stands.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::config_writes::{
    CONFIG, PROCESS_FLAGS_LOCK, config_admin, post_configs, tuple_value, wire,
};
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

/// The `:media_proxy` entry of a GET `/config` answer.
fn media_proxy_entry(page: &Value) -> Value {
    page["configs"]
        .as_array()
        .and_then(|configs| {
            configs
                .iter()
                .find(|entry| {
                    entry["key"]
                        .as_str()
                        .is_some_and(|key| key == ":media_proxy")
                })
                .cloned()
        })
        .expect("the media proxy entry answers")
}

/// The `need_reboot` flag follows the registry: a live key's write keeps
/// it down and a restart-flagged key's write raises it — in the POST
/// answer and in every later GET while the row stands. The flagged write
/// also raises the process-wide flag the WBE-207 tests read, so this
/// holds [`PROCESS_FLAGS_LOCK`] across it.
#[tokio::test]
async fn config_need_reboot_follows_the_changed_keys() {
    let _flag_guard = PROCESS_FLAGS_LOCK.lock().await;
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

    let proxy_write = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":media_proxy",
            "value": wire(&[("max_bytes", json!(1024))]),
        }]),
    )
    .await;
    assert_eq!(
        proxy_write.json_value()["need_reboot"],
        json!(true),
        "the media proxy waits for the restart"
    );

    let standing = TestRequest::get(CONFIG).bearer(&token).send(&app).await;
    assert_eq!(
        standing.json_value()["need_reboot"],
        json!(true),
        "and the flag holds while the stored row stands"
    );
    let page = standing.json_value();
    assert_eq!(
        tuple_value(&media_proxy_entry(&page)["value"], "max_bytes"),
        Some(&json!(1024)),
        "the stored value is the effective one"
    );
}

/// The restart flag clears with the row: deleting the stored
/// `:media_proxy` is itself still a restart-flagged change, and once it
/// stands no more the GET answers the env default and `need_reboot` is
/// down. The page flag is the store's answer; the process-wide flag the
/// same writes raise belongs to [`PROCESS_FLAGS_LOCK`]'s readers.
#[tokio::test]
async fn deleting_a_restart_flagged_row_lets_the_flag_fall() {
    let _flag_guard = PROCESS_FLAGS_LOCK.lock().await;
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configreboot").await;

    let opening = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":media_proxy",
            "value": wire(&[("max_bytes", json!(1024))]),
        }]),
    )
    .await;
    common::assert_answer(&opening, StatusCode::OK, "the seeded write");

    let deleted = post_configs(
        &app,
        &token,
        json!([{ "group": ":pleroma", "key": ":media_proxy", "delete": true }]),
    )
    .await;
    assert_eq!(
        deleted.json_value()["need_reboot"],
        json!(true),
        "deleting a restart key is still a change waiting for the restart"
    );

    let after = TestRequest::get(CONFIG).bearer(&token).send(&app).await;
    assert_eq!(
        after.json_value()["need_reboot"],
        json!(false),
        "with the row gone nothing waits any more"
    );
    let page = after.json_value();
    assert_eq!(
        tuple_value(&media_proxy_entry(&page)["value"], "max_bytes"),
        Some(&json!(50_000_000)),
        "and the env default is back"
    );
}
