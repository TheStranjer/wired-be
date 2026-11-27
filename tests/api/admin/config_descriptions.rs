//! The admin config descriptions and the reboot pair's successful paths
//! (WBE-207): `GET /config/descriptions` answering the registry — one
//! documented object per whitelisted key with its children typing inside
//! the ticket's vocabulary — and the process-wide `GET /need_reboot` and
//! `GET /restart` flags, which the tests serialize behind
//! [`PROCESS_FLAGS_LOCK`] and reset rather than wait out.
//!
//! [`PROCESS_FLAGS_LOCK`]: super::config_writes::PROCESS_FLAGS_LOCK

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use wired_be::services::runtime_config::RuntimeConfig;

use super::config_writes::{
    CONFIG_DESCRIPTIONS, NEED_REBOOT, PROCESS_FLAGS_LOCK, RESTART, config_admin, post_configs, wire,
};
use crate::common::{self, TestRequest};

/// The ticket's type vocabulary, everywhere in the payload and at every
/// nesting depth.
const DESCRIPTION_TYPES: [&str; 7] = [
    "string", "boolean", "integer", "[:atom]", "keyword", "map", "group",
];

/// Collects every `type` string of one `children` array, recursively.
fn collect_types(children: &Value, seen: &mut Vec<String>) {
    if let Some(items) = children.as_array() {
        for child in items {
            if let Some(name) = child["type"].as_str() {
                seen.push(name.to_owned());
            }
            collect_types(&child["children"], seen);
        }
    }
}

/// `GET /config/descriptions` as the scoped admin answers it: the array
/// of description objects, in registry order.
async fn descriptions_json(app: &Router, token: &str) -> Vec<Value> {
    let page = TestRequest::get(CONFIG_DESCRIPTIONS)
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&page, StatusCode::OK, "the descriptions payload");
    page.json_value()
        .as_array()
        .expect("an array of description objects")
        .clone()
}

/// `GET /config/descriptions` answers the registry: one documented
/// object per whitelisted key, in table order, and the `:instance`
/// element spelled like the ticket's example — its children naming their
/// types, the hinted leaves carrying `suggestions` and the plain
/// booleans carrying none.
#[tokio::test]
async fn config_descriptions_answer_with_the_registry() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configdocs").await;

    let objects = descriptions_json(&app, &token).await;
    assert_eq!(objects.len(), 9, "one object per whitelist entry");
    let keys: Vec<&str> = objects
        .iter()
        .map(|object| {
            assert_eq!(
                object["group"],
                json!(":pleroma"),
                "every entry is :pleroma"
            );
            assert!(
                object["description"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "no entry goes undocumented"
            );
            object["key"].as_str().expect("the key is a string")
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            ":instance",
            ":mrf",
            ":mrf_simple",
            ":mrf_keyword",
            ":mrf_hellthread",
            ":mrf_object_age",
            ":mrf_hashtag",
            ":oauth_token_ttl_secs",
            ":frontend_configurations",
        ],
        "the whitelist, in table order, the two unread keys absent"
    );

    let instance = &objects[0];
    assert_eq!(instance["type"], json!("group"), "a documented container");
    assert_eq!(
        instance["description"],
        json!("Instance-related settings"),
        "Pleroma's wording"
    );
    let children = instance["children"].as_array().expect("the children");
    assert_eq!(children.len(), 9, "one child per whitelisted subkey");
    assert_eq!(children[0]["key"], json!("name"), "the example child");
    assert_eq!(children[0]["type"], json!("string"));
    assert_eq!(children[0]["description"], json!("Name of the instance"));
    assert_eq!(
        children[0]["suggestions"],
        json!(["Wired"]),
        "the example value, as the ticket's example carries one"
    );
    let registrations = children
        .iter()
        .find(|child| child["key"] == json!("registrations_open"))
        .expect("the subkey is documented");
    assert_eq!(registrations["type"], json!("boolean"), "booleans type");
    assert!(
        registrations.get("suggestions").is_none(),
        "a leaf without hints renders no suggestions field"
    );
}

/// The payload's other child metadata: the `[:atom]` policy chain with
/// its example names, the whole-value policy entries documented as
/// keyword parameter objects — and every `type`, at every depth, inside
/// the ticket's vocabulary.
#[tokio::test]
async fn config_descriptions_document_the_registry_children() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configkids").await;

    let objects = descriptions_json(&app, &token).await;
    let mrf = &objects[1];
    assert_eq!(
        mrf["children"][0]["key"],
        json!("policies"),
        "the one subkey"
    );
    assert_eq!(
        mrf["children"][0]["type"],
        json!("[:atom]"),
        "the list-of-names spelling"
    );
    let policies = mrf["children"][0]["suggestions"]
        .as_array()
        .expect("the chain has example values");
    assert!(
        policies.contains(&json!("simple")) && policies.contains(&json!("keyword")),
        "the wired policy names are offered"
    );

    let simple = &objects[2];
    assert_eq!(simple["type"], json!("keyword"), "a whole parameter object");
    let lists = simple["children"].as_array().expect("the policy's lists");
    assert_eq!(
        lists[0]["key"],
        json!("accept"),
        "Pleroma's simple-policy docs"
    );
    assert_eq!(lists[0]["type"], json!("[:atom]"));

    let ttl = &objects[7];
    assert_eq!(ttl["type"], json!("group"), "the lifetimes are a group");
    let lifetimes: Vec<(&str, &str)> = ttl["children"]
        .as_array()
        .expect("the lifetime children")
        .iter()
        .map(|child| {
            (
                child["key"].as_str().expect("a key"),
                child["type"].as_str().expect("a type"),
            )
        })
        .collect();
    assert_eq!(
        lifetimes,
        vec![
            ("admin", "integer"),
            ("bot", "integer"),
            ("user", "integer")
        ],
        "one integer child per tier"
    );

    assert_eq!(objects.len(), 9, "the registry's nine entries, no more");

    let mut types = Vec::new();
    for object in objects {
        types.push(
            object["type"]
                .as_str()
                .expect("every entry types")
                .to_owned(),
        );
        collect_types(&object["children"], &mut types);
    }
    assert!(
        types
            .iter()
            .all(|name| DESCRIPTION_TYPES.contains(&name.as_str())),
        "the payload stays inside the ticket's vocabulary: {types:?}"
    );
}

/// `GET /need_reboot` reads the process-wide flag, not the store: down
/// for a fresh boot, still down after a live key's write and after a
/// refused write of a key nothing reads, and up once the flag is raised
/// (no whitelisted key raises it through the API today, so the test
/// raises it directly).
#[tokio::test]
async fn need_reboot_toggles_once_raised() {
    let _flag_guard = PROCESS_FLAGS_LOCK.lock().await;
    RuntimeConfig::reset_process_flags();
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configflag").await;

    let quiet = TestRequest::get(NEED_REBOOT)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&quiet, StatusCode::OK, "the quiet GET");
    assert_eq!(
        quiet.json_value(),
        json!({ "need_reboot": false }),
        "a fresh boot waits for nobody"
    );

    let live = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("name", json!("Live"))]),
        }]),
    )
    .await;
    common::assert_answer(&live, StatusCode::OK, "the live write");
    let still = TestRequest::get(NEED_REBOOT)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        still.json_value(),
        json!({ "need_reboot": false }),
        ":instance takes effect live and raises nothing"
    );

    let refused = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":media_proxy",
            "value": wire(&[("max_bytes", json!(2048))]),
        }]),
    )
    .await;
    common::assert_answer(&refused, StatusCode::BAD_REQUEST, "the unread key");
    let untouched = TestRequest::get(NEED_REBOOT)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        untouched.json_value(),
        json!({ "need_reboot": false }),
        "a refused write raises nothing"
    );

    RuntimeConfig::set_need_reboot();
    let raised = TestRequest::get(NEED_REBOOT)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        raised.json_value(),
        json!({ "need_reboot": true }),
        "the raised flag is what the read answers"
    );
    RuntimeConfig::reset_process_flags();
}

/// Once raised, `GET /need_reboot` keeps answering true: only the next
/// boot lowers the flag. A later live write does not lower it, a refused
/// batch changes nothing, and a delete that finds nothing to delete
/// toggles nothing either.
#[tokio::test]
async fn need_reboot_stands_through_calm_and_refused_writes() {
    let _flag_guard = PROCESS_FLAGS_LOCK.lock().await;
    RuntimeConfig::reset_process_flags();
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "confighold").await;

    RuntimeConfig::set_need_reboot();

    let later = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("name", json!("Calmer"))]),
        }]),
    )
    .await;
    common::assert_answer(&later, StatusCode::OK, "a later live write");
    let holds = TestRequest::get(NEED_REBOOT)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        holds.json_value(),
        json!({ "need_reboot": true }),
        "live changes do not lower what a restart still owes"
    );

    let refused = post_configs(
        &app,
        &token,
        json!([{ "group": ":pleroma", "key": ":nope", "value": json!([]) }]),
    )
    .await;
    common::assert_answer(&refused, StatusCode::BAD_REQUEST, "the refused batch");
    let unchanged = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "delete": true,
            "subkeys": ["never-stored"],
        }]),
    )
    .await;
    common::assert_answer(&unchanged, StatusCode::OK, "the no-op delete");
    let flag = TestRequest::get(NEED_REBOOT)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        flag.json_value(),
        json!({ "need_reboot": true }),
        "changes that are not changes toggle nothing"
    );
}

/// `GET /restart` answers `{}` and raises the shutdown flag `serve()`
/// observes beside ctrl-c — and it does NOT clear `need_reboot`: the
/// running process applied nothing, the next boot will. Tests assert the
/// flag; nothing here restarts a process.
#[tokio::test]
async fn restart_raises_the_shutdown_flag() {
    let _flag_guard = PROCESS_FLAGS_LOCK.lock().await;
    RuntimeConfig::reset_process_flags();
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configrestart").await;

    assert!(
        !RuntimeConfig::restart_requested(),
        "nobody has asked this process to restart"
    );

    let ack = TestRequest::get(RESTART).bearer(&token).send(&app).await;
    common::assert_answer(&ack, StatusCode::OK, "the restart request");
    assert_eq!(ack.json_value(), json!({}), "Pleroma's empty answer");
    assert!(
        RuntimeConfig::restart_requested(),
        "the shutdown flag stands for serve() to drain on"
    );

    RuntimeConfig::set_need_reboot();
    let raised = TestRequest::get(NEED_REBOOT)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        raised.json_value(),
        json!({ "need_reboot": true }),
        "the flag was raised before the request"
    );

    let again = TestRequest::get(RESTART).bearer(&token).send(&app).await;
    common::assert_answer(&again, StatusCode::OK, "a second request is answerable too");
    let stands = TestRequest::get(NEED_REBOOT)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        stands.json_value(),
        json!({ "need_reboot": true }),
        "and clearing need_reboot is NOT the restart's business"
    );
    RuntimeConfig::reset_process_flags();
}
