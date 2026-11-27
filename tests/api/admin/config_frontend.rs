//! The `:pleroma/:frontend_configurations` admin-config key (WBE-895) over
//! HTTP: subkey-wise upserts that replace each named frontend's whole
//! object, subkey and whole-key deletes, the 400 for malformed values, the
//! admin listing and the public `GET /api/pleroma/frontend_configurations`
//! reading the stored value, with the `instance_settings` row as fallback.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use wired_be::models::instance_setting;

use super::config_writes::{CONFIG, config_admin, post_configs, tuple_value, wire};
use super::support::{NOT_ADMIN, regular};
use crate::common::{self, TestRequest};

/// The public read route.
const PUBLIC: &str = "/api/pleroma/frontend_configurations";

/// One `:frontend_configurations` upsert item for `pairs`.
fn upsert(pairs: &[(&str, Value)]) -> Value {
    json!([{
        "group": ":pleroma",
        "key": ":frontend_configurations",
        "value": wire(pairs),
    }])
}

/// A delete item, with `subkeys` when given.
fn delete(subkeys: Option<&[&str]>) -> Value {
    let mut item = json!({
        "group": ":pleroma",
        "key": ":frontend_configurations",
        "delete": true,
    });
    if let (Some(names), Some(object)) = (subkeys, item.as_object_mut()) {
        let previous = object.insert("subkeys".to_owned(), json!(names));
        drop(previous);
    }
    json!([item])
}

/// The public document as the unauthenticated endpoint answers it.
async fn public_document(app: &Router) -> Value {
    let response = TestRequest::get(PUBLIC).send(app).await;
    common::assert_answer(&response, StatusCode::OK, "the public read");
    response.json_value()
}

/// The `:frontend_configurations` entry of the stored-rows-only listing,
/// `None` when no row stands.
async fn stored_entry(app: &Router, token: &str) -> Option<Value> {
    let response = TestRequest::get(&format!("{CONFIG}?only_db=true"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the stored rows");
    response.json_value()["configs"]
        .as_array()?
        .iter()
        .find(|entry| entry["key"] == json!(":frontend_configurations"))
        .cloned()
}

/// The first write: the 200, the public read and both admin listings.
#[tokio::test]
async fn frontend_configurations_are_written_listed_and_served() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "frontendadmin").await;

    let first = post_configs(
        &app,
        &token,
        upsert(&[(":soapbox_fe", json!({"brandColor": "#123456"}))]),
    )
    .await;
    common::assert_answer(&first, StatusCode::OK, "the first write");
    let body = first.json_value();
    assert_eq!(
        body["need_reboot"],
        json!(false),
        "the key takes effect live"
    );
    assert_eq!(
        tuple_value(&body["configs"][0]["value"], "soapbox_fe"),
        Some(&json!({"brandColor": "#123456"})),
        "the write renders as a tuple around the plain object"
    );
    assert_eq!(
        public_document(&app).await,
        json!({"soapbox_fe": {"brandColor": "#123456"}}),
        "the public read is keyed by subkey without the colon"
    );

    let listed = stored_entry(&app, &token)
        .await
        .expect("the admin listing shows the key");
    assert_eq!(listed["group"], json!(":pleroma"), "listed under :pleroma");
    assert_eq!(listed["db"], json!(["soapbox_fe"]), "db names the subkey");
    assert_eq!(
        tuple_value(&listed["value"], "soapbox_fe"),
        Some(&json!({"brandColor": "#123456"})),
        "the listing uses the tuple shape"
    );
    let effective = TestRequest::get(CONFIG).bearer(&token).send(&app).await;
    common::assert_answer(&effective, StatusCode::OK, "the effective listing");
    let effective = effective.json_value();
    let in_effective = effective["configs"]
        .as_array()
        .and_then(|configs| {
            configs
                .iter()
                .find(|entry| entry["key"] == json!(":frontend_configurations"))
        })
        .expect("the effective listing carries the key");
    assert_eq!(
        tuple_value(&in_effective["value"], "soapbox_fe"),
        Some(&json!({"brandColor": "#123456"})),
        "the effective view shows the stored subkey"
    );
}

/// A second subkey joins the first, a repeated subkey is replaced whole, a
/// subkey delete leaves the rest and deleting the last one removes the key.
#[tokio::test]
async fn frontend_configurations_replace_whole_subkeys_and_delete() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "frontendmerge").await;

    let first = post_configs(
        &app,
        &token,
        upsert(&[(":soapbox_fe", json!({"brandColor": "#123456"}))]),
    )
    .await;
    common::assert_answer(&first, StatusCode::OK, "the first write");

    let second = post_configs(
        &app,
        &token,
        upsert(&[(":pleroma_fe", json!({"theme": "dark"}))]),
    )
    .await;
    common::assert_answer(&second, StatusCode::OK, "the second write");
    assert_eq!(
        public_document(&app).await,
        json!({
            "soapbox_fe": {"brandColor": "#123456"},
            "pleroma_fe": {"theme": "dark"},
        }),
        "a new subkey joins the stored one"
    );

    let third = post_configs(
        &app,
        &token,
        upsert(&[(":soapbox_fe", json!({"logo": "/logo.png"}))]),
    )
    .await;
    common::assert_answer(&third, StatusCode::OK, "the replacing write");
    assert_eq!(
        public_document(&app).await,
        json!({
            "soapbox_fe": {"logo": "/logo.png"},
            "pleroma_fe": {"theme": "dark"},
        }),
        "the named subkey is replaced whole, no old key survives"
    );

    let partial = post_configs(&app, &token, delete(Some(&[":soapbox_fe"]))).await;
    common::assert_answer(&partial, StatusCode::OK, "the subkey delete");
    assert_eq!(
        public_document(&app).await,
        json!({"pleroma_fe": {"theme": "dark"}}),
        "only the named subkey went"
    );

    let last = post_configs(&app, &token, delete(Some(&[":pleroma_fe"]))).await;
    common::assert_answer(&last, StatusCode::OK, "the last subkey delete");
    assert!(
        stored_entry(&app, &token).await.is_none(),
        "dropping the last subkey removes the key"
    );
    assert_eq!(
        public_document(&app).await,
        json!({}),
        "nothing stored falls back to the empty object"
    );
}

/// Without `subkeys`, a delete removes the whole key.
#[tokio::test]
async fn a_whole_key_delete_removes_every_subkey() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "frontenddelete").await;

    let written = post_configs(
        &app,
        &token,
        upsert(&[
            (":soapbox_fe", json!({"a": 1})),
            (":pleroma_fe", json!({"b": 2})),
        ]),
    )
    .await;
    common::assert_answer(&written, StatusCode::OK, "both subkeys write");
    assert_eq!(
        public_document(&app).await,
        json!({"soapbox_fe": {"a": 1}, "pleroma_fe": {"b": 2}}),
        "both subkeys stand"
    );

    let deleted = post_configs(&app, &token, delete(None)).await;
    common::assert_answer(&deleted, StatusCode::OK, "the whole-key delete");
    assert!(stored_entry(&app, &token).await.is_none(), "no row is left");
    assert_eq!(public_document(&app).await, json!({}), "and no subkey");
}

/// A value that is not a list of `[:name, {…}]` tuples is the 400, and a
/// batch holding one writes nothing.
#[tokio::test]
async fn malformed_values_are_refused_and_nothing_is_stored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "frontendbad").await;

    for bad in [
        json!({"soapbox_fe": {"a": 1}}),
        json!("soapbox_fe"),
        json!([]),
        json!(["soapbox_fe"]),
        json!([{"tuple": [":soapbox_fe"]}]),
        json!([{"tuple": [":soapbox_fe", "dark"]}]),
        json!([{"tuple": [":soapbox_fe", [1, 2]]}]),
    ] {
        let refused = post_configs(
            &app,
            &token,
            json!([
                {
                    "group": ":pleroma",
                    "key": ":instance",
                    "value": wire(&[("name", json!("Never Written"))]),
                },
                {
                    "group": ":pleroma",
                    "key": ":frontend_configurations",
                    "value": bad,
                },
            ]),
        )
        .await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "a malformed value");
        assert!(
            refused
                .error_field()
                .is_some_and(|message| message.contains(":frontend_configurations")),
            "the error names the key: {}",
            refused.text()
        );
    }
    let stored = TestRequest::get(&format!("{CONFIG}?only_db=true"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&stored, StatusCode::OK, "the stored rows");
    assert_eq!(
        stored.json_value()["configs"],
        json!([]),
        "nothing was stored, not even the valid item of the batch"
    );
    assert_eq!(public_document(&app).await, json!({}), "the read is empty");
}

/// A token that is not an admin's is refused the way every config write
/// is, and the public read needs no token at all.
#[tokio::test]
async fn only_admins_write_and_anyone_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, plain) = regular(&pool, "frontendplain", "admin:write:config").await;

    let refused = post_configs(&app, &plain, upsert(&[(":soapbox_fe", json!({"a": 1}))])).await;
    common::assert_all_refused(
        &[refused],
        StatusCode::FORBIDDEN,
        NOT_ADMIN,
        "a non-admin token writing the key",
    );
    assert_eq!(
        public_document(&app).await,
        json!({}),
        "the refused write stored nothing, and the read took no token"
    );
}

/// With nothing stored through the admin config, the `instance_settings`
/// row still serves, and an admin-config value takes over from it.
#[tokio::test]
async fn the_instance_setting_row_is_the_fallback() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "frontendfallback").await;
    let row = json!({"theme": "dark", "mobile": {"notifications": true}});
    instance_setting::set(&pool, "frontend_configurations", &row)
        .await
        .expect("the setting writes");
    assert_eq!(public_document(&app).await, row, "the row serves as before");

    let written = post_configs(&app, &token, upsert(&[(":soapbox_fe", json!({"a": 1}))])).await;
    common::assert_answer(&written, StatusCode::OK, "the admin write");
    assert_eq!(
        public_document(&app).await,
        json!({"soapbox_fe": {"a": 1}}),
        "the admin-config value wins while it stands"
    );

    let deleted = post_configs(&app, &token, delete(None)).await;
    common::assert_answer(&deleted, StatusCode::OK, "the delete");
    assert_eq!(
        public_document(&app).await,
        row,
        "deleting it hands the read back to the row"
    );
}
