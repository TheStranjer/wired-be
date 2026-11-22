//! Filing a report: the full, bare, form and lone-id bodies, every
//! category, repeat filings and the target account entity.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::models::{
    report::{self, Category, ReportState},
    status::Visibility,
};

use super::support::{
    EXPECTED_KEYS, ROOT, about, filed, filings, json_keys, post, reporter, rule, stored,
};
use crate::common;

/// A filing with everything the reporter may send: two of the target's public
/// statuses, one cited rule, a comment, a category, and the forward request.
#[tokio::test]
async fn a_full_filing_stores_and_answers_what_it_was_given() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let first = post(&pool, &bob, "first", Visibility::Public).await;
    let second = post(&pool, &bob, "second", Visibility::Public).await;
    let cited = rule(&pool, "No harassment").await;

    let value = filed(
        &app,
        &token,
        json!({
            "account_id": bob.id.to_string(),
            "status_ids": [first.to_string(), second.to_string()],
            "comment": "they will not stop",
            "forward": true,
            "category": "violation",
            "rule_ids": [cited.to_string()],
        }),
    )
    .await;

    assert_eq!(json_keys(&value), EXPECTED_KEYS, "exactly the keys");
    assert_eq!(value["action_taken"], false, "nothing is done yet");
    assert_eq!(value["action_taken_at"], Value::Null, "and nothing was");
    assert_eq!(value["category"], "violation", "the cited category");
    assert_eq!(value["comment"], "they will not stop", "the comment");
    assert_eq!(value["forwarded"], true, "the forward request");
    assert_eq!(
        value["status_ids"],
        json!([first.to_string(), second.to_string()]),
        "the cited statuses, oldest first"
    );
    assert_eq!(value["rule_ids"], json!([cited.to_string()]), "the rule");
    assert_eq!(
        value["target_account"]["id"],
        bob.id.to_string(),
        "the reported account"
    );

    let row = stored(&pool, &value).await;
    assert_eq!(row.reporter_id, alice.id, "who filed it");
    assert_eq!(row.target_id, bob.id, "about whom");
    assert_eq!(row.state, ReportState::Open, "and it opens open");
    assert_eq!(row.assigned_to, None, "unassigned");
    assert_eq!(row.action_taken_at, None, "with no action taken");
    assert_eq!(row.category, Category::Violation, "the stored category");
    assert!(row.forwarded, "the stored flag");
    assert_eq!(row.rule_ids, vec![cited], "the stored rules");
    assert!(
        row.ap_id
            .as_deref()
            .is_some_and(|ap_id| ap_id.starts_with("https://test.example/activities/")),
        "a local reporter minted a Flag id: {:?}",
        row.ap_id
    );
    assert_eq!(
        report::statuses_for(&pool, row.id).await.unwrap(),
        vec![first, second],
        "the stored context"
    );
}

/// A filing that names only the account takes every default: empty comment,
/// `other`, no forwarding, and both arrays empty.
#[tokio::test]
async fn a_bare_filing_answers_the_defaults() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;

    let value = filed(&app, &token, about(&bob)).await;

    assert_eq!(json_keys(&value), EXPECTED_KEYS, "exactly the keys");
    assert_eq!(value["comment"], "", "no comment is the empty comment");
    assert_eq!(value["category"], "other", "and no category is `other`");
    assert_eq!(value["forwarded"], false, "reports are not forwarded");
    assert_eq!(value["status_ids"], json!([]), "with nothing cited");
    assert_eq!(value["rule_ids"], json!([]), "and no rules");
    assert_eq!(
        stored(&pool, &value).await.state,
        ReportState::Open,
        "the stored row is open"
    );
    assert!(
        value["created_at"]
            .as_str()
            .is_some_and(|text| text.ends_with('Z')),
        "created_at is a Mastodon timestamp: {}",
        value["created_at"]
    );
}

/// A form body — what Mastodon clients send — cites statuses with
/// `status_ids[]` keys and spells its flag and category as strings.
#[tokio::test]
async fn a_form_body_files_the_same_report() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let cited = post(&pool, &bob, "the cited post", Visibility::Public).await;

    let response = common::TestRequest::post(ROOT)
        .bearer(&token)
        .form(&[
            ("account_id", &bob.id.to_string()),
            ("status_ids[]", &cited.to_string()),
            ("comment", "form filing"),
            ("forward", "on"),
            ("category", "spam"),
        ])
        .send(&app)
        .await;

    common::assert_answer(&response, StatusCode::OK, "a form body files");
    let value = response.json_value();
    assert_eq!(value["category"], "spam", "the form category");
    assert_eq!(value["forwarded"], true, "the form flag");
    assert_eq!(
        value["status_ids"],
        json!([cited.to_string()]),
        "the array key is read"
    );
    assert_eq!(value["comment"], "form filing", "and the plain field");
}

/// A lone `status_ids` value (no array) is the one-element list Pleroma's
/// `List.wrap` accepts.
#[tokio::test]
async fn a_lone_status_id_is_the_one_element_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let cited = post(&pool, &bob, "lone", Visibility::Public).await;

    let value = filed(
        &app,
        &token,
        json!({"account_id": bob.id.to_string(), "status_ids": cited.to_string()}),
    )
    .await;

    assert_eq!(value["status_ids"], json!([cited.to_string()]));
}

/// Every category the column allows files, and each comes back as spelled.
#[tokio::test]
async fn each_known_category_files_as_the_column_spells_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;

    for category in Category::ALL {
        let bob = common::create_local_user(&pool, &format!("bob-{}", category.as_str())).await;
        let value = filed(
            &app,
            &token,
            json!({"account_id": bob.id.to_string(), "category": category.as_str()}),
        )
        .await;
        assert_eq!(
            value["category"],
            category.as_str(),
            "`{category}` round-trips"
        );
    }
    assert_eq!(filings(&pool).await.len(), 4, "one report each");
}

/// Filing is not deduplicated: the same account may report the same account
/// again, and each filing is its own row.
#[tokio::test]
async fn the_same_account_may_report_the_same_account_twice() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;

    let first = filed(&app, &token, about(&bob)).await;
    let second = filed(&app, &token, about(&bob)).await;

    assert_ne!(
        first["id"], second["id"],
        "two filings, two report ids, both 200"
    );
    assert_eq!(
        first["target_account"]["id"], second["target_account"]["id"],
        "and the same reported account"
    );
    let rows = filings(&pool).await;
    assert_eq!(rows.len(), 2, "both are stored");
    assert_ne!(rows[0].ap_id, rows[1].ap_id, "each with its own Flag id");
    assert_eq!(rows[0].target_id, bob.id, "newest filing first");
}

/// The reported account is the full `Account` entity as the reporter sees it,
/// not a stub.
#[tokio::test]
async fn the_target_account_is_the_full_account_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;

    let value = filed(&app, &token, about(&bob)).await;
    let account = &value["target_account"];

    assert_eq!(account["id"], bob.id.to_string(), "the id");
    assert_eq!(account["username"], "bob", "the nickname part");
    assert_eq!(account["acct"], "bob", "the stored nickname");
    assert_eq!(account["url"], bob.ap_id, "the profile url");
    assert_eq!(account["followers_count"], 0, "the counters");
    assert!(account["pleroma"].is_object(), "with the pleroma block");
    assert!(
        account["source"].is_null(),
        "and never the owner-only source block"
    );
}
