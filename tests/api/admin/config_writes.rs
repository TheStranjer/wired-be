//! The admin config store (WBE-206) over HTTP: `POST /config` batch
//! writes over the whitelist registry — a set rendering the wire tuple
//! form whose effective GET merges the row over the env defaults, a
//! second set merging into the stored row, the subkey and whole-row
//! deletes, and the whitelist refusing an unknown pair before any of a
//! batch writes. The route consts, the flag lock and the wire and batch
//! helpers every config submodule shares ride here as `pub(super)`.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::support::admin;
use crate::common::{self, TestRequest};

/// The config store route.
pub(super) const CONFIG: &str = "/api/v1/pleroma/admin/config";
/// The config descriptions route (WBE-207).
pub(super) const CONFIG_DESCRIPTIONS: &str = "/api/v1/pleroma/admin/config/descriptions";
/// The process-wide need-reboot read route (WBE-207).
pub(super) const NEED_REBOOT: &str = "/api/v1/pleroma/admin/need_reboot";
/// The graceful-restart request route (WBE-207).
pub(super) const RESTART: &str = "/api/v1/pleroma/admin/restart";

/// Serializes the tests that read or reset the process-wide config
/// flags: `need_reboot` and the restart request live one per process,
/// so only one of them may own them at a time (WBE-207, decision 13).
/// An async lock because the guarded tests await throughout.
pub(super) static PROCESS_FLAGS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The `{"tuple": [key, value]}` array the API exchanges keyword lists
/// as (Pleroma's `ConfigDB.to_json_types` shape).
pub(super) fn wire(pairs: &[(&str, Value)]) -> Vec<Value> {
    pairs
        .iter()
        .map(|(key, value)| json!({"tuple": [key, value]}))
        .collect()
}

/// The stored value `value` carries under wire-form `key`, `None` when no
/// such tuple entry stands in the array.
pub(super) fn tuple_value<'v>(value: &'v Value, key: &str) -> Option<&'v Value> {
    value.as_array()?.iter().find_map(|entry| {
        let pair = entry.get("tuple").and_then(Value::as_array)?;
        if pair.first() == Some(&json!(key)) {
            pair.get(1)
        } else {
            None
        }
    })
}

/// `POST /config` with one `configs` batch.
pub(super) async fn post_configs(app: &Router, token: &str, items: Value) -> common::TestResponse {
    TestRequest::post(CONFIG)
        .bearer(token)
        .json(&json!({ "configs": items }))
        .send(app)
        .await
}

/// An admin holding both config scopes: the round-trip tests GET what
/// they POST.
pub(super) async fn config_admin(pool: &PgPool, nickname: &str) -> String {
    admin(pool, nickname, "admin:read:config admin:write:config")
        .await
        .1
}

/// A set renders the wire tuple form, names its `db` subkeys, and the
/// effective GET merges the row over the env defaults — every whitelisted
/// key answers, and `need_reboot` stays down for the live keys.
#[tokio::test]
async fn config_set_renders_the_wire_shape_and_the_effective_view() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configroot").await;

    let first = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("name", json!("Renamed")), ("email", json!("staff@example.test"))]),
        }]),
    )
    .await;
    common::assert_answer(&first, StatusCode::OK, "the first write");
    let body = first.json_value();
    assert_eq!(body["configs"][0]["group"], ":pleroma", "the group");
    assert_eq!(body["configs"][0]["key"], ":instance", "the key");
    assert_eq!(
        body["configs"][0]["value"],
        json!(wire(&[
            (":email", json!("staff@example.test")),
            (":name", json!("Renamed"))
        ])),
        "the stored row renders as its tuple array, keys with their colon"
    );
    assert_eq!(
        body["configs"][0]["db"],
        json!([":email", ":name"]),
        "db names the stored subkeys"
    );
    assert_eq!(body["need_reboot"], json!(false), ":instance changes live");

    let merged = TestRequest::get(CONFIG).bearer(&token).send(&app).await;
    common::assert_answer(&merged, StatusCode::OK, "the effective GET");
    let view = merged.json_value();
    assert_eq!(
        view["configs"].as_array().map(Vec::len),
        Some(9),
        "every whitelisted key answers without only_db"
    );
    let instance = &view["configs"][0];
    assert_eq!(
        tuple_value(&instance["value"], ":name"),
        Some(&json!("Renamed")),
        "the row's name is the effective one"
    );
    assert_eq!(
        tuple_value(&instance["value"], ":description"),
        Some(&json!("A Wired instance, compatible with Pleroma clients")),
        "the untouched sibling keeps the env default"
    );
    assert_eq!(instance["db"], json!([":email", ":name"]), "and db stays");
    assert_eq!(
        view["configs"][1].get("db"),
        None,
        "an entry with no row renders no db"
    );
}

/// A second set merges into the stored row: the new subkey writes
/// through, the first batch's subkey survives, and `only_db` answers the
/// stored rows alone.
#[tokio::test]
async fn config_second_write_merges_into_the_stored_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configmerge").await;

    let opening = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("name", json!("Renamed")), ("email", json!("staff@example.test"))]),
        }]),
    )
    .await;
    common::assert_answer(&opening, StatusCode::OK, "the first write");

    let second = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("name", json!("Again"))]),
        }]),
    )
    .await;
    let body = second.json_value();
    assert_eq!(
        tuple_value(&body["configs"][0]["value"], ":name"),
        Some(&json!("Again")),
        "the re-set writes through"
    );
    assert_eq!(
        tuple_value(&body["configs"][0]["value"], ":email"),
        Some(&json!("staff@example.test")),
        "and the earlier subkey survives the merge"
    );
    assert_eq!(
        body["configs"][0]["db"],
        json!([":email", ":name"]),
        "both subkeys are stored now"
    );

    let only_db = TestRequest::get(&format!("{CONFIG}?only_db=true"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&only_db, StatusCode::OK, "the only_db GET");
    let rows = only_db.json_value();
    assert_eq!(
        rows["configs"].as_array().map(Vec::len),
        Some(1),
        "only_db answers the stored rows alone"
    );
    assert_eq!(rows["configs"][0]["key"], ":instance", "the one row");
}

/// The deletes: the subkey delete leaves the rest of the row standing,
/// the whole delete takes the row out of the store, and the effective GET
/// falls back to the env values once nothing stands.
#[tokio::test]
async fn config_delete_takes_subkeys_then_the_whole_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configdel").await;

    let opening = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("name", json!("Renamed")), ("email", json!("staff@example.test"))]),
        }]),
    )
    .await;
    common::assert_answer(&opening, StatusCode::OK, "the seeded write");

    let subkey_delete = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "delete": true,
            "subkeys": ["name"],
        }]),
    )
    .await;
    let left = subkey_delete.json_value();
    assert_eq!(
        left["configs"][0]["value"],
        json!(wire(&[(":email", json!("staff@example.test"))])),
        "the deleted subkey is out and the row keeps the rest"
    );
    assert_eq!(left["configs"][0]["db"], json!([":email"]), "db shrank too");

    let whole_delete = post_configs(
        &app,
        &token,
        json!([{ "group": ":pleroma", "key": ":instance", "delete": true }]),
    )
    .await;
    let gone = whole_delete.json_value();
    assert_eq!(
        gone["configs"],
        json!([]),
        "a whole delete leaves nothing to render"
    );
    let only_db = TestRequest::get(&format!("{CONFIG}?only_db=true"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        only_db.json_value()["configs"],
        json!([]),
        "and the row is gone from the store"
    );
    let effective = TestRequest::get(CONFIG).bearer(&token).send(&app).await;
    let fallback = &effective.json_value()["configs"][0];
    assert_eq!(
        tuple_value(&fallback["value"], ":name"),
        Some(&json!("Wired")),
        "the effective name fell back to the env default"
    );
    assert_eq!(fallback.get("db"), None, "with no row there is no db");
}

/// The whitelist is the write gate: an unknown key or group is the 400
/// naming the pair, the whole batch is refused before any of it writes,
/// and an upsert without a value is refused too.
#[tokio::test]
async fn config_whitelist_refuses_unknown_pairs() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configgate").await;

    let mixed = post_configs(
        &app,
        &token,
        json!([
            { "group": ":pleroma", "key": ":instance", "value": wire(&[("name", json!("Sneaky"))]) },
            { "group": ":pleroma", "key": ":database_config", "value": json!([]) },
        ]),
    )
    .await;
    common::assert_answer(&mixed, StatusCode::BAD_REQUEST, "the mixed batch");
    common::assert_error(
        &mixed,
        "Config not allowed: :pleroma/:database_config",
        "an unknown key in a batch that leads with a valid pair",
    );
    let only_db = TestRequest::get(&format!("{CONFIG}?only_db=true"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        only_db.json_value()["configs"],
        json!([]),
        "the refused batch wrote nothing, not even its valid first item"
    );

    let wrong_group = post_configs(
        &app,
        &token,
        json!([{ "group": ":wired", "key": ":instance", "value": json!([]) }]),
    )
    .await;
    common::assert_error(
        &wrong_group,
        "Config not allowed: :wired/:instance",
        "a real key in an unregistered group",
    );

    let valueless = post_configs(
        &app,
        &token,
        json!([{ "group": ":pleroma", "key": ":mrf" }]),
    )
    .await;
    common::assert_answer(
        &valueless,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an upsert without a value",
    );
}

/// A POST with colon keys and a POST with bare keys store the same row,
/// and both read back (POST answer and `only_db` GET) with colon keys.
#[tokio::test]
async fn config_keys_round_trip_with_colons_whichever_spelling_is_sent() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configcolons").await;
    let expected = json!(wire(&[
        (":email", json!("staff@example.test")),
        (":name", json!("Renamed")),
    ]));

    let mut stored_rows = Vec::new();
    for pairs in [
        [
            (":name", json!("Renamed")),
            (":email", json!("staff@example.test")),
        ],
        [
            ("name", json!("Renamed")),
            ("email", json!("staff@example.test")),
        ],
    ] {
        let write = post_configs(
            &app,
            &token,
            json!([{ "group": ":pleroma", "key": ":instance", "value": wire(&pairs) }]),
        )
        .await;
        common::assert_answer(&write, StatusCode::OK, "the write");
        assert_eq!(
            write.json_value()["configs"][0]["value"],
            expected,
            "the answer carries colon keys"
        );
        let listing = TestRequest::get(&format!("{CONFIG}?only_db=true"))
            .bearer(&token)
            .send(&app)
            .await;
        let rows = listing.json_value();
        assert_eq!(rows["configs"][0]["value"], expected, "the GET agrees");
        assert_eq!(
            rows["configs"][0]["db"],
            json!([":email", ":name"]),
            "db carries colons"
        );
        stored_rows.push(rows["configs"][0].clone());
    }
    assert_eq!(
        stored_rows.first(),
        stored_rows.get(1),
        "both spellings store the same thing"
    );
}
