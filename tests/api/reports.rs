//! Reports over HTTP (`POST /api/v1/reports`, WBE-120): filing a report.
//!
//! The tests pin what the ticket promises: a filing succeeds with and without
//! cited statuses, answers the ten-key Mastodon `Report` entity whose
//! `target_account` is the full account render, and stores the row; `form`
//! bodies cite statuses with `status_ids[]` keys; repeat filings are two
//! reports; and every validation answers its own 422 — a self-report, a
//! `comment` over `MAX_REPORT_COMMENT`, a `category` outside the four, a
//! cited status that is another account's, invisible, deleted or not an id,
//! and a cited rule that names none. An unknown, malformed or deactivated
//! `account_id` is the shared 404, the route enforces the whole `AuthUser` +
//! `write:reports` pipeline, and it rides the `statuses_actions` bucket.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        instance_rule,
        report::{self, Category, ReportState},
        status::Visibility,
        user::{Flag, NewUser, User},
    },
    pagination::{MAX_LIMIT, PageParams},
};

use crate::common;

/// The route every test posts to.
const ROOT: &str = "/api/v1/reports";

/// Every JSON key a report entity must carry, sorted.
const EXPECTED_KEYS: [&str; 10] = [
    "action_taken",
    "action_taken_at",
    "category",
    "comment",
    "created_at",
    "forwarded",
    "id",
    "rule_ids",
    "status_ids",
    "target_account",
];

/// The 422 wording of the cited-status check.
const BAD_STATUSES: &str = "Invalid status_ids";
/// The 422 wording of the cited-rule check.
const BAD_RULES: &str = "Invalid rule_ids";

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` with a `write:reports` token: the shape
/// almost every filing in this file is made by.
async fn reporter(pool: &PgPool, nickname: &str) -> (User, String) {
    fixture(pool, nickname, "write:reports").await
}

/// `POST /api/v1/reports` with the JSON `body` as `token`.
async fn file(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(ROOT)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// A filing, asserted 200, answered as JSON.
async fn filed(app: &Router, token: &str, body: Value) -> Value {
    let response = file(app, token, body).await;
    assert_eq!(response.status, StatusCode::OK, "the report files");
    response.json_value()
}

/// The minimal valid body: report the account, say nothing.
fn about(target: &User) -> Value {
    json!({"account_id": target.id.to_string()})
}

/// The keys of a JSON object, sorted, for exact key-set assertions.
fn json_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("reports render objects")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The `id` the entity answered with, as the row's own `UUIDv7` text.
fn entity_id(value: &Value) -> Uuid {
    let text = value["id"].as_str().expect("the entity carries its own id");
    Uuid::parse_str(text).expect("the entity id is an id")
}

/// The stored report the entity answered with, which must exist.
async fn stored(pool: &PgPool, value: &Value) -> report::Report {
    report::get(pool, entity_id(value))
        .await
        .unwrap()
        .expect("the filed report is stored")
}

/// Every report in the database, newest filing first: what a refusal never
/// adds to and a filing always does.
async fn filings(pool: &PgPool) -> Vec<report::Report> {
    let page = PageParams::from_query(Some(&format!("limit={MAX_LIMIT}")), MAX_LIMIT)
        .expect("the fixture page parses");
    report::list(pool, page).await.expect("the listing reads")
}

/// A live post by `author` of `visibility`, for the citations.
async fn post(pool: &PgPool, author: &User, text: &str, visibility: Visibility) -> Uuid {
    common::create_status(pool, author, text, visibility)
        .await
        .id
}

/// A rule of this instance named `text`, for the `rule_ids` citations.
async fn rule(pool: &PgPool, text: &str) -> Uuid {
    instance_rule::create(pool, text, "", 1)
        .await
        .expect("the rule fixture creates")
        .id
}

/// A local user named `nickname` whose account is deactivated: the shape
/// [`AuthUser`] refuses with its 403.
async fn dormant_user(pool: &PgPool, nickname: &str) -> User {
    wired_be::models::user::insert(
        pool,
        &NewUser {
            is_active: Flag::new(false),
            ..NewUser::local(nickname, &format!("https://test.example/users/{nickname}"))
        },
    )
    .await
    .expect("the dormant fixture inserts")
}

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

/// The route is user-token-only: no token, an unknown token, an app-only
/// credential and a deactivated owner are refused before anything else is
/// looked at, and none of them files anything.
#[tokio::test]
async fn the_credential_gates_refuse_the_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let bob = common::create_local_user(&pool, "bob").await;
    let app_only = common::create_app_only_token(&pool, "write write:reports").await;
    let dormant = dormant_user(&pool, "dormant").await;
    let dormant_token = common::create_app_and_token(&pool, &dormant, "write:reports").await;

    let missing = common::TestRequest::post(ROOT)
        .json(&about(&bob))
        .send(&app)
        .await;
    common::assert_answer(&missing, StatusCode::UNAUTHORIZED, "no token at all");
    common::assert_error(&missing, "Invalid credentials", "no token at all");

    let unknown = common::TestRequest::post(ROOT)
        .bearer("no-such-token")
        .json(&about(&bob))
        .send(&app)
        .await;
    common::assert_answer(
        &unknown,
        StatusCode::UNAUTHORIZED,
        "a token that does not resolve",
    );
    common::assert_error(
        &unknown,
        "The access token is invalid",
        "a token that does not resolve",
    );

    let credential = common::TestRequest::post(ROOT)
        .bearer(&app_only)
        .json(&about(&bob))
        .send(&app)
        .await;
    common::assert_answer(&credential, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(
        &credential,
        "This API requires an authenticated user",
        "an app-only token",
    );

    let disabled = common::TestRequest::post(ROOT)
        .bearer(&dormant_token)
        .json(&about(&bob))
        .send(&app)
        .await;
    common::assert_answer(&disabled, StatusCode::FORBIDDEN, "a deactivated owner");
    common::assert_error(
        &disabled,
        "Your account is currently disabled",
        "a deactivated owner",
    );

    assert!(filings(&pool).await.is_empty(), "and no refusal filed");
}

/// The scope half of the pipeline: `write:reports` and the broad `write` root
/// file, while any other grant is the 403 naming the missing one.
#[tokio::test]
async fn a_token_without_the_route_scope_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_narrow, narrow) = fixture(&pool, "narrow", "read:statuses write:statuses").await;
    let (_beside, beside) = fixture(&pool, "beside", "write:accounts").await;
    let (_root, root) = fixture(&pool, "root", "write").await;
    let (_follow, follow) = fixture(&pool, "follow", "follow").await;
    let bob = common::create_local_user(&pool, "bob").await;

    for token in [&narrow, &beside, &follow] {
        let response = file(&app, token, about(&bob)).await;
        common::assert_answer(
            &response,
            StatusCode::FORBIDDEN,
            "a token without the scope",
        );
        common::assert_error(
            &response,
            "Insufficient permissions: write:reports.",
            "a token without the scope",
        );
    }

    let response = file(&app, &root, about(&bob)).await;
    common::assert_answer(&response, StatusCode::OK, "the broad write root files");
    assert_eq!(
        filings(&pool).await.len(),
        1,
        "and only the granted token did"
    );
}

/// A report is answered 200 with `application/json`, and a refusal's body is
/// the one-key `{"error": …}` shape of decision 8.
#[tokio::test]
async fn both_answers_are_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;

    let refused = file(&app, &token, about(&alice)).await;
    common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, "a self-report");
    assert_eq!(
        refused.header("content-type"),
        Some("application/json"),
        "the refusal is JSON"
    );
    assert_eq!(
        refused
            .json_value()
            .as_object()
            .expect("an error object")
            .keys()
            .len(),
        1,
        "and carries nothing but its `error`"
    );

    let ok = file(&app, &token, about(&bob)).await;
    common::assert_answer(&ok, StatusCode::OK, "a filing");
    assert_eq!(
        ok.header("content-type"),
        Some("application/json"),
        "so is the success"
    );
}

/// The route rides the `statuses_actions` bucket: the third request of the
/// window is refused 429 `{"error":"Throttled"}` and never reaches the
/// handler, so nothing is filed for it.
#[tokio::test]
async fn the_route_spends_its_rate_limit_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("statuses_actions=2:600"),
    );
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;

    for spent in 0..2 {
        let response = file(&app, &token, about(&bob)).await;
        common::assert_answer(&response, StatusCode::OK, "a request inside the budget");
        assert_eq!(
            response.header("x-ratelimit-limit"),
            Some("2"),
            "the bucket's budget"
        );
        assert_eq!(
            response.header("x-ratelimit-remaining"),
            Some(if spent == 0 { "1" } else { "0" }),
            "the window counts down"
        );
    }

    let throttled = file(&app, &token, about(&bob)).await;
    common::assert_answer(&throttled, StatusCode::TOO_MANY_REQUESTS, "over the budget");
    common::assert_error(&throttled, "Throttled", "over the budget");
    assert_eq!(
        filings(&pool).await.len(),
        2,
        "the third request never reached the handler"
    );
}
