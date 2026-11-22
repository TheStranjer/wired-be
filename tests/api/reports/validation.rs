//! The refusals a filing meets: self-reports, unknown accounts, comment and
//! category limits, and the cited statuses and rules that must be valid.

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{report, status::Visibility},
};

use super::support::{
    BAD_RULES, BAD_STATUSES, about, dormant_user, entity_id, file, filed, filings, post, reporter,
    rule, stored,
};
use crate::common;

#[tokio::test]
async fn reporting_yourself_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reporter(&pool, "alice").await;

    let response = file(&app, &token, about(&alice)).await;

    common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, "a self-report");
    common::assert_error(&response, "Can not report yourself", "a self-report");
    assert!(filings(&pool).await.is_empty(), "and nothing was filed");
}

/// A malformed, empty, unknown or deactivated `account_id` is one 404: the
/// endpoint never tells a caller which of the four it asked for.
#[tokio::test]
async fn an_account_that_is_not_there_is_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let ghost = dormant_user(&pool, "ghost").await;

    for id in [
        "not-an-id",
        "",
        &Uuid::now_v7().to_string(),
        &ghost.id.to_string(),
    ] {
        let response = file(&app, &token, json!({"account_id": id, "comment": "who?"})).await;
        common::assert_answer(&response, StatusCode::NOT_FOUND, "an unusable account_id");
        common::assert_error(&response, "Record not found", "an unusable account_id");
    }
    assert!(filings(&pool).await.is_empty(), "and none of them filed");
}

/// The body's own shape is checked before any lookup: a request that names no
/// account at all is the extractor's 422, naming the missing parameter.
#[tokio::test]
async fn a_body_without_an_account_id_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;

    let response = file(&app, &token, json!({"comment": "no target"})).await;

    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a body missing account_id",
    );
    assert!(
        response
            .json_value()
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("account_id")),
        "the 422 names the missing parameter: {}",
        response.text()
    );
}

/// A `comment` over the cap is 422 with Mastodon's changeset wording, and the
/// cap counts characters rather than bytes.
#[tokio::test]
async fn a_comment_over_the_cap_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;

    let value = filed(
        &app,
        &token,
        json!({"account_id": bob.id.to_string(), "comment": "x".repeat(1_000)}),
    )
    .await;
    assert_eq!(
        value["comment"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .count(),
        1_000,
        "the cap itself still files"
    );

    let response = file(
        &app,
        &token,
        json!({"account_id": carol.id.to_string(), "comment": "é".repeat(1_001)}),
    )
    .await;
    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a comment over the cap",
    );
    common::assert_error(
        &response,
        "Validation failed: Comment is too long (maximum is 1000 characters)",
        "a comment over the cap",
    );
    assert_eq!(
        filings(&pool).await.len(),
        1,
        "and the long filing stored nothing"
    );
}

/// The cap is a setting: `MAX_REPORT_COMMENT` moves it, and the refusal
/// wording moves with it.
#[tokio::test]
async fn max_report_comment_moves_the_cap() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.max_report_comment = 5;
    let app = common::test_app_with_config(pool.clone(), config);
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;

    let short = filed(
        &app,
        &token,
        json!({"account_id": bob.id.to_string(), "comment": "five!"}),
    )
    .await;
    assert_eq!(
        short["comment"], "five!",
        "five characters fit the moved cap"
    );

    let response = file(
        &app,
        &token,
        json!({"account_id": carol.id.to_string(), "comment": "six!!!"}),
    )
    .await;
    common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, "a moved cap");
    common::assert_error(
        &response,
        "Validation failed: Comment is too long (maximum is 5 characters)",
        "a moved cap",
    );
}

/// A `category` outside the four is 422; nothing was filed for it.
#[tokio::test]
async fn an_unknown_category_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;

    for category in ["", "Spam", "harassment", "OTHER", "spam "] {
        let response = file(
            &app,
            &token,
            json!({"account_id": bob.id.to_string(), "category": category}),
        )
        .await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            "an unknown category",
        );
        common::assert_error(
            &response,
            "Validation failed: Category is not included in the list",
            "an unknown category",
        );
    }
    assert!(filings(&pool).await.is_empty(), "and none of them filed");
}

/// A cited status must be the reported account's: another account's post is
/// the same refusal, and files nothing.
#[tokio::test]
async fn a_status_of_another_account_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let carols_own = post(&pool, &carol, "not bob's post", Visibility::Public).await;

    let response = file(
        &app,
        &token,
        json!({
            "account_id": bob.id.to_string(),
            "status_ids": [carols_own.to_string()],
        }),
    )
    .await;

    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a foreign status",
    );
    common::assert_error(&response, BAD_STATUSES, "a foreign status");
    assert!(filings(&pool).await.is_empty(), "and nothing was filed");
}

/// The reporter's own post does not belong in a report about someone else:
/// authorship by the target is the rule.
#[tokio::test]
async fn a_reporter_s_own_post_does_not_belong_in_a_report() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let bobs = post(&pool, &bob, "the reported post", Visibility::Public).await;
    let alices = post(&pool, &alice, "my own post", Visibility::Public).await;

    let response = file(
        &app,
        &token,
        json!({
            "account_id": bob.id.to_string(),
            "status_ids": [bobs.to_string(), alices.to_string()],
        }),
    )
    .await;

    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the reporter's own post",
    );
    common::assert_error(&response, BAD_STATUSES, "the reporter's own post");
}

/// A status the reporter may not see is refused without revealing that it
/// exists: the same 422 as any other unusable citation.
#[tokio::test]
async fn a_status_the_reporter_cannot_see_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let hidden = post(&pool, &bob, "a direct post", Visibility::Direct).await;

    let response = file(
        &app,
        &token,
        json!({
            "account_id": bob.id.to_string(),
            "status_ids": [hidden.to_string()],
        }),
    )
    .await;

    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an invisible status",
    );
    common::assert_error(&response, BAD_STATUSES, "an invisible status");
}

/// The visibility model is the one the read endpoints apply, so a follower
/// may cite a post only followers see: the same request that was refused
/// before the follow files after it.
#[tokio::test]
async fn a_follower_may_cite_a_post_only_followers_see() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let followers_only = post(&pool, &bob, "followers", Visibility::Private).await;
    let body = json!({
        "account_id": bob.id.to_string(),
        "status_ids": [followers_only.to_string()],
    });

    let refused = file(&app, &token, body.clone()).await;
    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "before the follow",
    );
    common::assert_error(&refused, BAD_STATUSES, "before the follow");

    let granted = wired_be::models::follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .expect("the fixture follows");
    assert!(
        matches!(
            granted.state,
            wired_be::models::follow::FollowState::Accepted
        ),
        "an unlocked local account answers the follow at once"
    );

    let value = filed(&app, &token, body).await;
    assert_eq!(
        value["status_ids"],
        json!([followers_only.to_string()]),
        "the same citation files once the reporter may see the post"
    );
}

/// A deleted post is no longer citable, and one unusable citation fails the
/// whole filing: a report is never stored with partial context.
#[tokio::test]
async fn a_deleted_status_is_refused_and_takes_the_rest_with_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let kept = post(&pool, &bob, "still here", Visibility::Public).await;
    let doomed = post(&pool, &bob, "about to go", Visibility::Public).await;
    let _tombstoned = sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
        .bind(doomed)
        .execute(&pool)
        .await
        .expect("the fixture deletes the post");

    let response = file(
        &app,
        &token,
        json!({
            "account_id": bob.id.to_string(),
            "status_ids": [kept.to_string(), doomed.to_string()],
        }),
    )
    .await;

    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a deleted status",
    );
    common::assert_error(&response, BAD_STATUSES, "a deleted status");
    assert!(
        filings(&pool).await.is_empty(),
        "one unusable citation refuses the whole filing"
    );
}

/// An id that is not id text, and one that names no post, are the same 422 as
/// a foreign or invisible one — and both arrive before anything is written.
#[tokio::test]
async fn a_status_id_that_names_nothing_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;

    for cited in ["not-an-id", "", &Uuid::now_v7().to_string()] {
        let response = file(
            &app,
            &token,
            json!({"account_id": bob.id.to_string(), "status_ids": [cited]}),
        )
        .await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            "an unusable id",
        );
        common::assert_error(&response, BAD_STATUSES, "an unusable id");
    }
    assert!(
        filings(&pool).await.is_empty(),
        "and none of them stored a report"
    );
}

/// A `rule_ids` entry must name a rule of this instance: an unknown or
/// malformed id is 422 `{"error":"Invalid rule_ids"}`, and a body that cites
/// one rule this instance does hold files with it stored in the cited order.
#[tokio::test]
async fn a_rule_that_does_not_exist_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let known = rule(&pool, "Be kind").await;

    for cited in [&Uuid::now_v7().to_string(), "not-an-id", ""] {
        let response = file(
            &app,
            &token,
            json!({
                "account_id": bob.id.to_string(),
                "rule_ids": [known.to_string(), cited],
            }),
        )
        .await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            "an unusable rule",
        );
        common::assert_error(&response, BAD_RULES, "an unusable rule");
    }

    let value = filed(
        &app,
        &token,
        json!({"account_id": bob.id.to_string(), "rule_ids": [known.to_string()]}),
    )
    .await;
    assert_eq!(value["rule_ids"], json!([known.to_string()]), "files");
    assert_eq!(stored(&pool, &value).await.rule_ids, vec![known]);
}

/// A status cited twice in one filing is stored — and answered — once.
#[tokio::test]
async fn citing_one_status_twice_files_it_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let cited = post(&pool, &bob, "twice", Visibility::Public).await;

    let value = filed(
        &app,
        &token,
        json!({
            "account_id": bob.id.to_string(),
            "status_ids": [cited.to_string(), cited.to_string()],
        }),
    )
    .await;

    assert_eq!(
        value["status_ids"],
        json!([cited.to_string()]),
        "the repeat answers once"
    );
    assert_eq!(
        report::statuses_for(&pool, entity_id(&value))
            .await
            .unwrap(),
        vec![cited],
        "and is stored once"
    );
}
