//! The `config_change` entry of the moderation log: one row per changed
//! batch with its sentence naming the pairs in order and its data map
//! carrying every pair's write-or-delete flag, and the silence of the
//! batches that changed nothing or the whitelist refused.

use axum::http::StatusCode;
use serde_json::json;

use super::config_writes::{post_configs, wire};
use super::support::{LOG, admin};
use crate::common::{self, TestRequest};

/// The `config_change` entry of the moderation log: one row per changed
/// batch, its sentence naming the pairs in order and its data carrying
/// every pair with its write-or-delete flag — findable by the listing's
/// search filter like every other entry.
#[tokio::test]
async fn config_changes_record_the_moderation_log() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "configlogger", "admin:read admin:write").await;

    let written = post_configs(
        &app,
        &token,
        json!([
            {
                "group": ":pleroma",
                "key": ":instance",
                "value": wire(&[("name", json!("Logged"))]),
            },
            {
                "group": ":pleroma",
                "key": ":mrf",
                "value": wire(&[("policies", json!(["noop"]))]),
            },
        ]),
    )
    .await;
    common::assert_answer(&written, StatusCode::OK, "the two-entry batch");

    let log_page = TestRequest::get(LOG).bearer(&token).send(&app).await;
    let page = log_page.json_value();
    assert_eq!(page["total"], json!(1), "one entry for the batch");
    let entry = &page["items"][0];
    assert_eq!(
        entry["message"], "@configlogger changed config: :pleroma/:instance, :pleroma/:mrf",
        "the sentence names every changed pair, in order"
    );
    assert_eq!(
        entry["data"]["action"], "config_change",
        "the stored data map carries the action"
    );
    assert_eq!(
        entry["data"]["configs"],
        json!([
            {"group": ":pleroma", "key": ":instance", "delete": false},
            {"group": ":pleroma", "key": ":mrf", "delete": false},
        ]),
        "with every pair's write-or-delete flag"
    );
    assert_eq!(
        entry["data"]["actor"]["nickname"], "configlogger",
        "and the acting admin, as every entry's data carries it"
    );

    let search = TestRequest::get(&format!("{LOG}?search=changed+config"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        search.json_value()["total"],
        json!(1),
        "the entry is findable"
    );
}

/// Nothing that changed nothing gets logged: the delete of an absent row
/// answers empty and writes no line, and a batch the whitelist refused
/// wrote neither store nor log.
#[tokio::test]
async fn unchanged_and_refused_batches_log_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "configquiet", "admin:read admin:write").await;

    let quiet = post_configs(
        &app,
        &token,
        json!([{ "group": ":pleroma", "key": ":mrf_keyword", "delete": true }]),
    )
    .await;
    common::assert_answer(&quiet, StatusCode::OK, "the delete of an absent entry");
    let empty_log = TestRequest::get(LOG).bearer(&token).send(&app).await;
    assert_eq!(
        empty_log.json_value()["total"],
        json!(0),
        "nothing changed, nothing logged"
    );

    let refused = post_configs(
        &app,
        &token,
        json!([{ "group": ":pleroma", "key": ":nope", "value": json!([]) }]),
    )
    .await;
    common::assert_answer(&refused, StatusCode::BAD_REQUEST, "the refused batch");
    let after_refusal = TestRequest::get(LOG).bearer(&token).send(&app).await;
    assert_eq!(
        after_refusal.json_value()["total"],
        json!(0),
        "the refused batch added no log line either"
    );
}
