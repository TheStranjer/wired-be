//! The admin content endpoints over HTTP (`/api/v1/pleroma/admin/rules` and
//! `/api/v1/pleroma/admin/announcements`, WBE-204): the instance-rule
//! quartet and the announcement quintet — the two lists the public API only
//! ever reads.
//!
//! The rule tests pin the four-key entity Pleroma's `AdminAPI.RuleView`
//! renders, the priority-then-creation order the listing answers in, the
//! present-only `PATCH`, the `{}` a delete leaves behind, the
//! `{"errors":{"text":["can't be blank"]}}` (and the 500-character)
//! refusals of a body, and that `GET /api/v1/instance/rules` and the
//! `rules` block of `GET /api/v1/instance` read the very list these routes
//! maintain.
//!
//! The announcement tests pin the entity the client API already serves —
//! the same fourteen keys, `all_day` and `pleroma.raw_content` included,
//! `read` false for a list an administration view computes no viewer for —
//! the `limit` / `offset` window over notices whose activity has not opened
//! and whose activity has closed, `text_to_html` for the served `content`
//! with the author's text kept verbatim beside it, the window refusals, the
//! moderation-log entry and the `announcement` stream event a create
//! publishes, and that a user's own `GET /api/v1/announcements` shows what
//! the admin wrote only while its window says so.
//!
//! Both areas then walk the gate: the `AdminUser` role half on every route,
//! and `admin:read` / `admin:write` — the root or the `rules` /
//! `announcements` sub-scope — as the scope half.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::broadcast;
use uuid::Uuid;
use wired_be::{
    models::user::{Flag, NewUser, User},
    state::AppState,
    streaming::hub::Audience,
    util::format::text_to_html,
};

use crate::common;

/// The admin rule routes.
const RULES: &str = "/api/v1/pleroma/admin/rules";
/// The admin announcement routes.
const NOTICES: &str = "/api/v1/pleroma/admin/announcements";
/// The public rules listing the instance publishes.
const PUBLIC_RULES: &str = "/api/v1/instance/rules";
/// The public instance document, whose `rules` block is the same list.
const INSTANCE: &str = "/api/v1/instance";
/// The user-facing announcements listing.
const USER_NOTICES: &str = "/api/v1/announcements";
/// The admin scopes both halves of every gate here accept, as roots.
const ALL_ADMIN: &str = "admin:read admin:write";
/// Every JSON key the admin rule entity carries, sorted.
const RULE_KEYS: [&str; 4] = ["hint", "id", "priority", "text"];
/// Every JSON key an announcement entity carries, sorted: the client API's
/// fourteen, `all_day` and the `pleroma` block included.
const NOTICE_KEYS: [&str; 14] = [
    "all_day",
    "content",
    "emojis",
    "ends_at",
    "id",
    "mentions",
    "pleroma",
    "published_at",
    "reactions",
    "read",
    "starts_at",
    "statuses",
    "tags",
    "updated_at",
];
/// 400 wording a blank required field earns.
const BLANK: &str = "can't be blank";
/// 400 wording a rule text past 500 characters earns.
const TOO_LONG: &str = "should be at most 500 character(s)";
/// 400 wording a bound that is no RFC 3339 timestamp earns.
const INVALID_TIME: &str = "is invalid";
/// 400 wording a window whose end does not follow its start earns.
const ENDS_AFTER: &str = "must be after starts_at";
/// A window bound far enough in the past to be open already.
const PAST: &str = "2000-06-01T12:00:00.250Z";
/// A window bound far enough in the future to be closed only much later.
const FUTURE: &str = "2100-06-01T12:00:00.250Z";

// --- fixtures ---------------------------------------------------------------------------

/// The router over the stock test config.
fn app(pool: &PgPool) -> Router {
    common::test_app(pool.clone())
}

/// The router beside the state whose hub the stream events are read from.
fn app_with_state(pool: &PgPool) -> (Router, AppState) {
    let state = common::test_app_state(pool.clone(), common::test_config());
    (wired_be::app(state.clone()), state)
}

/// A local admin named `nickname` with a bearer token granting `scopes`.
async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// The newest `moderation_log` row written for `action`: its sentence and
/// data map, insisting at least one stands.
async fn log_entry(pool: &PgPool, action: &str) -> (String, Value) {
    let found = sqlx::query_as::<_, (String, Value)>(
        "SELECT message, data FROM moderation_log WHERE action = $1 \
         ORDER BY inserted_at DESC, id DESC LIMIT 1",
    )
    .bind(action)
    .fetch_optional(pool)
    .await
    .expect("the log reads");
    found.expect("at least one entry for the action")
}

/// How many `instance_rules` rows the database holds.
async fn rule_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instance_rules")
        .fetch_one(pool)
        .await
        .expect("the rules count")
}

/// How many `announcements` rows the database holds.
async fn notice_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM announcements")
        .fetch_one(pool)
        .await
        .expect("the notices count")
}

// --- request helpers --------------------------------------------------------------------

/// `POST RULES` with `body` as `token`.
async fn post_rule(app: &Router, token: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::post(RULES)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// `POST NOTICES` with `body` as `token`.
async fn post_notice(app: &Router, token: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::post(NOTICES)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// `PATCH path` with `body` as `token`.
async fn patch(app: &Router, token: &str, path: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::patch(path)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// `GET path` as `token`.
async fn get(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// `DELETE path` as `token`.
async fn destroy(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::delete(path)
        .bearer(token)
        .send(app)
        .await
}

/// `POST RULES` for `body`, insisting on 200 and answering the entity.
async fn create_rule(app: &Router, token: &str, body: &Value) -> Value {
    let created = post_rule(app, token, body).await;
    common::assert_answer(&created, StatusCode::OK, "creating a rule");
    created.json_value()
}

/// `POST NOTICES` for `body`, insisting on 200 and answering the entity.
async fn create_notice(app: &Router, token: &str, body: &Value) -> Value {
    let created = post_notice(app, token, body).await;
    common::assert_answer(&created, StatusCode::OK, "creating an announcement");
    created.json_value()
}

/// A rule reading `text` at `priority`, as the create answered it.
async fn a_rule(app: &Router, token: &str, text: &str, priority: i64) -> Value {
    create_rule(app, token, &json!({"text": text, "priority": priority})).await
}

/// A notice reading `content`, as the create answered it.
async fn a_notice(app: &Router, token: &str, content: &str) -> Value {
    create_notice(app, token, &json!({"content": content})).await
}

/// The `id` an entity answers with.
fn entity_id(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity carries an id")
        .to_owned()
}

/// The `id`s of a returned array, in answer order.
fn ids_of(listed: &Value) -> Vec<String> {
    listed
        .as_array()
        .expect("the listings are arrays")
        .iter()
        .map(entity_id)
        .collect()
}

/// The `text`s of a returned rule array, in answer order.
fn texts_of(listed: &Value) -> Vec<String> {
    listed
        .as_array()
        .expect("the listings are arrays")
        .iter()
        .map(|entry| {
            entry["text"]
                .as_str()
                .expect("every rule carries its text")
                .to_owned()
        })
        .collect()
}

/// The rendered object's keys, sorted, for exact key-set assertions.
fn json_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("entities render objects")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

// --- the rules ----------------------------------------------------------------------------

/// The rule create answers the four-key entity Pleroma's view renders — the
/// `hint` and `priority` the body leaves out take the column defaults — and
/// the listing is the same entities in display order: `priority` first, the
/// `id` (so creation order) within one priority.
#[tokio::test]
async fn a_created_rule_answers_the_four_key_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let bare = create_rule(&app, &token, &json!({"text": "No spam"})).await;
    let full = create_rule(
        &app,
        &token,
        &json!({"text": "Be nice", "hint": "say hi first", "priority": 2}),
    )
    .await;
    assert_eq!(json_keys(&full), RULE_KEYS, "exactly the view's keys");
    assert_eq!(
        json_keys(&bare),
        RULE_KEYS,
        "and the same four on a bare write"
    );
    assert!(
        Uuid::parse_str(&entity_id(&full)).is_ok(),
        "the id is the hyphenated uuid"
    );
    assert_eq!(full["text"], "Be nice", "the text verbatim");
    assert_eq!(full["hint"], "say hi first", "the hint verbatim");
    assert_eq!(full["priority"], 2, "the priority as an integer");
    assert_eq!(bare["hint"], "", "an absent hint is the empty default");
    assert_eq!(
        bare["priority"], 0,
        "an absent priority is the column default"
    );

    let listed = get(&app, &token, RULES).await;
    common::assert_answer(&listed, StatusCode::OK, "listing the rules");
    assert_eq!(
        listed.json_value(),
        json!([
            {"id": entity_id(&bare), "text": "No spam", "hint": "", "priority": 0},
            {"id": entity_id(&full), "text": "Be nice", "hint": "say hi first", "priority": 2},
        ]),
        "the bare array is the entities in the order the model reads them"
    );
}

/// `PATCH /rules/{id}` is present-only: a body naming one column rewrites
/// that one and answers the whole row, an explicit empty `hint` clears it,
/// and the write leaves its `rule_update` entry in the log.
#[tokio::test]
async fn a_rule_patch_changes_only_the_fields_it_names() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let stored = create_rule(
        &app,
        &token,
        &json!({"text": "Be nice", "hint": "say hi", "priority": 2}),
    )
    .await;
    let id = entity_id(&stored);

    let moved = patch(
        &app,
        &token,
        &format!("{RULES}/{id}"),
        &json!({"priority": 9}),
    )
    .await;
    common::assert_answer(&moved, StatusCode::OK, "moving a rule");
    assert_eq!(
        moved.json_value(),
        json!({"id": id, "text": "Be nice", "hint": "say hi", "priority": 9}),
        "the two fields the body did not name stand"
    );

    let renamed = patch(
        &app,
        &token,
        &format!("{RULES}/{id}"),
        &json!({"text": "Be nicer", "hint": ""}),
    )
    .await;
    common::assert_answer(&renamed, StatusCode::OK, "renaming a rule");
    assert_eq!(
        renamed.json_value(),
        json!({"id": id, "text": "Be nicer", "hint": "", "priority": 9}),
        "and an empty hint clears rather than keeps"
    );

    let (message, data) = log_entry(&pool, "rule_update").await;
    assert_eq!(
        message,
        format!("@root updated rule: {id}"),
        "the write is logged as the newest entry"
    );
    assert_eq!(data["subject_id"], id, "naming the rule it rewrote");
}

/// `DELETE /rules/{id}` answers `{}`, takes the row out of the listing, and
/// create, update and delete each leave their sentence in the log — the
/// delete's being the trace the row itself no longer is.
#[tokio::test]
async fn a_deleted_rule_leaves_the_log_and_an_empty_object() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let keeper = a_rule(&app, &token, "keeper", 0).await;
    let doomed = a_rule(&app, &token, "doomed", 0).await;
    let id = entity_id(&doomed);

    let moved = patch(
        &app,
        &token,
        &format!("{RULES}/{id}"),
        &json!({"priority": 3}),
    )
    .await;
    common::assert_answer(&moved, StatusCode::OK, "moving the doomed rule");
    let gone = destroy(&app, &token, &format!("{RULES}/{id}")).await;
    common::assert_answer(&gone, StatusCode::OK, "deleting a rule");
    assert_eq!(
        gone.json_value(),
        json!({}),
        "the delete answers the empty object"
    );
    assert_eq!(
        ids_of(&get(&app, &token, RULES).await.json_value()),
        vec![entity_id(&keeper)],
        "and the row is out of the list"
    );

    for (action, verb) in [
        ("rule_create", "created"),
        ("rule_update", "updated"),
        ("rule_delete", "deleted"),
    ] {
        let (message, data) = log_entry(&pool, action).await;
        assert_eq!(
            message,
            format!("@root {verb} rule: {id}"),
            "the {action} sentence names the rule by id"
        );
        assert_eq!(
            data["subject_id"], id,
            "and carries it in the data map's subject_id"
        );
    }
}

/// The rest of that walk's log lines: the update the test ran on the doomed
/// rule is the entry the create's and delete's neighbours name.
#[tokio::test]
async fn a_rule_write_logs_under_the_rules_area() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, write) = admin(&pool, "root", "admin:write:rules").await;
    let (_reader, read) = admin(&pool, "reader", "admin:read:rules").await;
    let written = a_rule(&app, &write, "logged", 4).await;
    let id = entity_id(&written);

    let moved = patch(
        &app,
        &write,
        &format!("{RULES}/{id}"),
        &json!({"priority": 5}),
    )
    .await;
    common::assert_answer(&moved, StatusCode::OK, "moving the rule the writer made");
    assert_eq!(moved.json_value()["priority"], 5, "the write took effect");

    let (created, _) = log_entry(&pool, "rule_create").await;
    assert_eq!(
        created,
        format!("@root created rule: {id}"),
        "the create entry"
    );
    let (updated, _) = log_entry(&pool, "rule_update").await;
    assert_eq!(
        updated,
        format!("@root updated rule: {id}"),
        "the update entry"
    );
    assert_eq!(
        rule_count(&pool).await,
        1,
        "and one row behind the two entries"
    );
    assert_eq!(
        texts_of(&get(&app, &read, RULES).await.json_value()),
        vec!["logged"],
        "the area's own read scope lists what its write scope made"
    );
}

/// The listing's order across four rules written out of sequence: the
/// integer priority decides and, inside one priority, the order the writes
/// arrived in holds — the tie-break `models::instance_rule::list` reads off
/// the `UUIDv7` id.
#[tokio::test]
async fn the_rule_listing_orders_by_priority_then_creation() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let one = a_rule(&app, &token, "one", 1).await;
    let top = a_rule(&app, &token, "top", -5).await;
    let two = a_rule(&app, &token, "two", 1).await;
    let last = a_rule(&app, &token, "last", 10).await;

    let listed = get(&app, &token, RULES).await.json_value();
    assert_eq!(
        texts_of(&listed),
        vec!["top", "one", "two", "last"],
        "priority first, creation order within one priority"
    );
    assert_eq!(
        ids_of(&listed),
        vec![
            entity_id(&top),
            entity_id(&one),
            entity_id(&two),
            entity_id(&last)
        ],
        "the ids the writes answered, in that order"
    );
}

/// A rule body's `text` is required and bounded: absent, empty and
/// whitespace-only earn the changeset refusal in the `{"errors": …}` shape,
/// one character past 500 earns the length refusal, and the bound itself
/// still passes.
#[tokio::test]
async fn a_rule_body_refuses_a_blank_or_overlong_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    for body in [
        json!({}),
        json!({"text": ""}),
        json!({"text": "   "}),
        json!({"hint": "no text"}),
    ] {
        let refused = post_rule(&app, &token, &body).await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "a blank text");
        assert_eq!(
            refused.json_value(),
            json!({"errors": {"text": [BLANK]}}),
            "the changeset refusal in the errors map: {}",
            refused.text()
        );
    }

    let refused = post_rule(&app, &token, &json!({"text": "x".repeat(501)})).await;
    common::assert_answer(&refused, StatusCode::BAD_REQUEST, "an over-long text");
    assert_eq!(
        refused.json_value(),
        json!({"errors": {"text": [TOO_LONG]}}),
        "the length refusal, same shape"
    );

    let exact = "y".repeat(500);
    let accepted = create_rule(&app, &token, &json!({"text": exact.clone()})).await;
    assert_eq!(
        accepted["text"], exact,
        "500 characters is inside the bound"
    );
    assert_eq!(rule_count(&pool).await, 1, "five refusals, one row");
}

/// A `PATCH` validates what it is given: a blank or over-long `text` is the
/// same 400, and the stored rule is left exactly as it was.
#[tokio::test]
async fn a_rule_patch_refuses_a_text_a_create_would_refuse() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let stored = create_rule(&app, &token, &json!({"text": "Keep me", "hint": "as is"})).await;
    let id = entity_id(&stored);

    for (body, expected) in [
        (json!({"text": ""}), json!([BLANK])),
        (json!({"text": "  "}), json!([BLANK])),
        (json!({"text": "z".repeat(501)}), json!([TOO_LONG])),
    ] {
        let refused = patch(&app, &token, &format!("{RULES}/{id}"), &body).await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "a refused patch");
        assert_eq!(
            refused.json_value()["errors"]["text"],
            expected,
            "the refusal the value earns: {}",
            refused.text()
        );
    }
    assert_eq!(
        get(&app, &token, RULES).await.json_value(),
        json!([{"id": id, "text": "Keep me", "hint": "as is", "priority": 0}]),
        "and the rule stands as written"
    );
}

/// A rule id that is no uuid and one that names no row are both the 404
/// `{"error":"Not found"}` Pleroma's admin `FallbackController` renders, on
/// both routes that take one.
#[tokio::test]
async fn an_unknown_rule_is_the_not_found_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let ghost = patch(
        &app,
        &token,
        &format!("{RULES}/not-an-id"),
        &json!({"text": "ghost"}),
    )
    .await;
    common::assert_answer(&ghost, StatusCode::NOT_FOUND, "patching no rule");
    common::assert_error(&ghost, "Not found", "patching no rule");

    for path in [
        format!("{RULES}/not-a-uuid"),
        format!("{RULES}/{}", Uuid::now_v7()),
        format!("{RULES}/0"),
    ] {
        let gone = destroy(&app, &token, &path).await;
        common::assert_answer(&gone, StatusCode::NOT_FOUND, &format!("deleting {path}"));
        common::assert_error(&gone, "Not found", &format!("deleting {path}"));
    }
    assert_eq!(
        rule_count(&pool).await,
        0,
        "and nothing was written on the way"
    );
}

/// The public reads follow the admin's writes: `GET /api/v1/instance/rules`
/// and the `rules` block of `GET /api/v1/instance` answer the same entries
/// in the same order — without the priority — and a delete takes a rule out
/// of both.
#[tokio::test]
async fn the_public_rules_reads_reflect_the_admin_writes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let empty = common::TestRequest::get(PUBLIC_RULES).send(&app).await;
    common::assert_answer(&empty, StatusCode::OK, "the public rules, unauthenticated");
    assert_eq!(empty.json_value(), json!([]), "no admin writes, no rules");

    let keep = create_rule(
        &app,
        &token,
        &json!({"text": "Rule one", "hint": "first", "priority": 1}),
    )
    .await;
    let drop_me = create_rule(&app, &token, &json!({"text": "Rule two", "priority": 2})).await;

    let public = common::TestRequest::get(PUBLIC_RULES)
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        public,
        json!([
            {"id": entity_id(&keep), "text": "Rule one", "hint": "first"},
            {"id": entity_id(&drop_me), "text": "Rule two", "hint": ""},
        ]),
        "the public listing is the admin's own order, the priority left out"
    );
    let document = common::TestRequest::get(INSTANCE)
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        document["rules"], public,
        "and the instance document's `rules` block is the very same list"
    );

    let gone = destroy(&app, &token, &format!("{RULES}/{}", entity_id(&drop_me))).await;
    common::assert_answer(&gone, StatusCode::OK, "deleting the second rule");
    assert_eq!(
        common::TestRequest::get(PUBLIC_RULES)
            .send(&app)
            .await
            .json_value(),
        json!([{"id": entity_id(&keep), "text": "Rule one", "hint": "first"}]),
        "the withdrawal reaches the public read too"
    );
}

// --- the announcements ---------------------------------------------------------------------

/// The notice create answers the entity the client API already serves —
/// fourteen keys, `all_day` and `pleroma.raw_content` included, `read`
/// false for an administration list — with the author's text rendered by
/// `text_to_html` for the wire and kept verbatim as the source; `show` and
/// the listing answer the same entity.
#[tokio::test]
async fn a_created_notice_answers_the_entity_with_its_raw_content() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let typed = "Down at five: https://example.org/status";
    let created = create_notice(
        &app,
        &token,
        &json!({"content": typed, "starts_at": PAST, "ends_at": FUTURE, "all_day": true}),
    )
    .await;
    assert_eq!(json_keys(&created), NOTICE_KEYS, "exactly the entity keys");
    assert_eq!(
        created["content"],
        text_to_html(typed),
        "the served content is the plain-text render"
    );
    let rendered = created["content"].as_str().expect("html");
    assert!(
        rendered.contains("<a href=") && rendered.contains("example.org/status"),
        "with the bare URL linkified: {rendered}"
    );
    assert_eq!(
        created["pleroma"]["raw_content"], typed,
        "while the source is kept verbatim"
    );
    assert_eq!(
        json_keys(&created["pleroma"]),
        ["raw_content"],
        "the pleroma block carries that key alone"
    );
    assert_eq!(created["starts_at"], PAST, "the window, mastodon times");
    assert_eq!(created["ends_at"], FUTURE, "at both ends");
    assert_eq!(created["all_day"], true, "the stored date-only flag");
    assert_eq!(
        created["read"], false,
        "an admin list computes no dismissal"
    );
    assert_eq!(
        created["published_at"], created["updated_at"],
        "a fresh row is stamped once"
    );

    let shown = get(&app, &token, &format!("{NOTICES}/{}", entity_id(&created))).await;
    common::assert_answer(&shown, StatusCode::OK, "reading one notice");
    assert_eq!(
        shown.json_value(),
        created,
        "show answers the created entity"
    );

    let listed = get(&app, &token, NOTICES).await;
    common::assert_answer(&listed, StatusCode::OK, "listing the notices");
    assert_eq!(
        listed.json_value(),
        json!([created]),
        "the listing is a bare array of the same entities"
    );
}

/// A plain-text notice is the render its author's words get: markup typed
/// into the body comes back escaped in `content`, verbatim in
/// `pleroma.raw_content` — and inert on the user's own read.
#[tokio::test]
async fn a_notice_serves_the_render_of_what_its_author_typed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let read = common::create_app_and_token(&pool, &alice, "read:accounts").await;

    let typed = "New <script>alert(1)</script> text";
    let entity = a_notice(&app, &token, typed).await;
    assert_eq!(
        entity["content"], "<p>New &lt;script&gt;alert(1)&lt;/script&gt; text</p>",
        "the markup the author typed is inert text in the served content"
    );
    assert_eq!(
        entity["pleroma"]["raw_content"], typed,
        "and the source keeps every byte of it"
    );

    let mine = get(&app, &read, USER_NOTICES).await.json_value();
    assert_eq!(
        mine[0], entity,
        "the user's read is the same entity, unread"
    );
}

/// `PATCH /announcements/{id}` is present-only over the four parameters: a
/// text write replaces both spellings and leaves the window and flag alone,
/// a window write leaves the text alone, and the row reads back as written.
#[tokio::test]
async fn a_notice_patch_changes_only_the_fields_it_names() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let stored = create_notice(
        &app,
        &token,
        &json!({"content": "before", "starts_at": PAST, "ends_at": FUTURE, "all_day": true}),
    )
    .await;
    let id = entity_id(&stored);

    let typed = "New <script>alert(1)</script> text";
    let rendered = "<p>New &lt;script&gt;alert(1)&lt;/script&gt; text</p>";
    let retitled = patch(
        &app,
        &token,
        &format!("{NOTICES}/{id}"),
        &json!({"content": typed}),
    )
    .await;
    common::assert_answer(&retitled, StatusCode::OK, "replacing the text");
    let body = retitled.json_value();
    assert_eq!(body["content"], rendered, "the new text re-rendered");
    assert_eq!(
        body["pleroma"]["raw_content"], typed,
        "with the source kept verbatim"
    );
    assert_eq!(
        body["starts_at"], PAST,
        "the window the write did not name stands"
    );
    assert_eq!(body["ends_at"], FUTURE, "at both ends");
    assert_eq!(body["all_day"], true, "and so does the flag");

    let closed = patch(
        &app,
        &token,
        &format!("{NOTICES}/{id}"),
        &json!({"all_day": false, "ends_at": "2000-06-02T00:00:00Z"}),
    )
    .await;
    common::assert_answer(&closed, StatusCode::OK, "closing the window");
    let window = closed.json_value();
    assert_eq!(window["all_day"], false, "the flag flipped");
    assert_eq!(
        window["ends_at"], "2000-06-02T00:00:00.000Z",
        "the bound replaced"
    );
    assert_eq!(
        window["content"], rendered,
        "the stored render survives a write that did not name it"
    );
    assert_eq!(
        get(&app, &token, &format!("{NOTICES}/{id}"))
            .await
            .json_value(),
        window,
        "and it reads back as written"
    );

    let (message, data) = log_entry(&pool, "announcement_update").await;
    assert_eq!(
        message,
        format!("@root updated announcement: {id}"),
        "the write is logged"
    );
    assert_eq!(data["subject_id"], id, "by the notice's own id");
}

/// `DELETE /announcements/{id}` answers `{}` and takes the notice out of
/// the admin and the user reads alike, its create and delete entries the
/// surviving trace.
#[tokio::test]
async fn a_deleted_notice_answers_an_empty_object() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let read = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let keeper = a_notice(&app, &token, "keeper").await;
    let doomed = a_notice(&app, &token, "doomed").await;
    let id = entity_id(&doomed);

    let gone = destroy(&app, &token, &format!("{NOTICES}/{id}")).await;
    common::assert_answer(&gone, StatusCode::OK, "deleting a notice");
    assert_eq!(
        gone.json_value(),
        json!({}),
        "the delete answers the empty object"
    );

    let missing = get(&app, &token, &format!("{NOTICES}/{id}")).await;
    common::assert_answer(
        &missing,
        StatusCode::NOT_FOUND,
        "the deleted notice is gone",
    );
    common::assert_error(&missing, "Not found", "the deleted notice is gone");
    assert_eq!(
        ids_of(&get(&app, &read, USER_NOTICES).await.json_value()),
        vec![entity_id(&keeper)],
        "the user's list keeps only what was not deleted"
    );

    for (action, verb) in [
        ("announcement_create", "created"),
        ("announcement_delete", "deleted"),
    ] {
        let (message, data) = log_entry(&pool, action).await;
        assert_eq!(
            message,
            format!("@root {verb} announcement: {id}"),
            "the {action} sentence names the notice"
        );
        assert_eq!(data["subject_id"], id, "and carries its id");
    }
}

/// The listing window: `limit` and `offset` cut the newest-first page, an
/// absent or unusable parameter falls back to the admin default, and an
/// `offset` past the last row answers nothing.
#[tokio::test]
async fn the_announcement_listing_windows_by_limit_and_offset() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let mut made = Vec::new();
    for number in 0..4 {
        let entity = a_notice(&app, &token, &format!("notice {number}")).await;
        made.push(entity_id(&entity));
    }
    let newest_first: Vec<String> = made.iter().rev().cloned().collect();

    let head = get(&app, &token, &format!("{NOTICES}?limit=2&offset=1")).await;
    common::assert_answer(&head, StatusCode::OK, "a windowed listing");
    assert_eq!(
        ids_of(&head.json_value()),
        vec![newest_first[1].clone(), newest_first[2].clone()],
        "the second and third rows of the newest-first list"
    );

    let whole = get(&app, &token, NOTICES).await.json_value();
    assert_eq!(
        ids_of(&whole),
        newest_first,
        "no parameters: the whole list"
    );
    let junk = get(&app, &token, &format!("{NOTICES}?limit=no&offset=-3")).await;
    assert_eq!(
        ids_of(&junk.json_value()),
        newest_first,
        "an unusable pair falls back to the default window"
    );
    assert_eq!(
        get(&app, &token, &format!("{NOTICES}?offset=90"))
            .await
            .json_value(),
        json!([]),
        "an offset past the last row answers nothing"
    );
    assert_eq!(
        ids_of(
            &get(&app, &token, &format!("{NOTICES}?limit=100000"))
                .await
                .json_value()
        ),
        newest_first,
        "the admin ceiling on `limit` changes nothing for four rows"
    );
}

/// The administration view sees every notice — the one whose activity has
/// not opened, the one whose activity closed and the live one — while the
/// user-facing list serves only the live one, as the same entity.
#[tokio::test]
async fn the_admin_sees_notices_whose_window_excludes_the_present() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let read = common::create_app_and_token(&pool, &alice, "read:accounts").await;

    let upcoming = create_notice(
        &app,
        &token,
        &json!({"content": "later", "starts_at": FUTURE}),
    )
    .await;
    let expired = create_notice(&app, &token, &json!({"content": "over", "ends_at": PAST})).await;
    let live = a_notice(&app, &token, "now").await;

    let listed = get(&app, &token, NOTICES).await.json_value();
    assert_eq!(
        ids_of(&listed),
        vec![entity_id(&live), entity_id(&expired), entity_id(&upcoming)],
        "all three, newest first, window aside"
    );

    let mine = get(&app, &read, USER_NOTICES).await;
    common::assert_answer(&mine, StatusCode::OK, "the user's list");
    assert_eq!(
        mine.json_value(),
        json!([live]),
        "the user sees only what is active, as the same entity"
    );
}

/// A user-facing list is what an admin write changes: opening an upcoming
/// notice puts it in the user's window, and deleting the live one takes it
/// out again.
#[tokio::test]
async fn a_user_facing_list_follows_the_admin_writes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let read = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let upcoming = create_notice(
        &app,
        &token,
        &json!({"content": "later", "starts_at": FUTURE}),
    )
    .await;
    let live = a_notice(&app, &token, "now").await;

    let opened = patch(
        &app,
        &token,
        &format!("{NOTICES}/{}", entity_id(&upcoming)),
        &json!({"starts_at": PAST}),
    )
    .await;
    common::assert_answer(&opened, StatusCode::OK, "opening the upcoming notice");
    let after = get(&app, &read, USER_NOTICES).await.json_value();
    assert_eq!(
        ids_of(&after),
        vec![entity_id(&live), entity_id(&upcoming)],
        "both active now, newest first"
    );
    assert_eq!(
        after[1]["content"], "<p>later</p>",
        "with the text the admin sent"
    );

    let gone = destroy(&app, &token, &format!("{NOTICES}/{}", entity_id(&live))).await;
    common::assert_answer(&gone, StatusCode::OK, "deleting the live notice");
    assert_eq!(
        ids_of(&get(&app, &read, USER_NOTICES).await.json_value()),
        vec![entity_id(&upcoming)],
        "and it leaves the user's list with it"
    );
}

/// A notice body's refusals, each leaving no row: `content` is required, a
/// bound that is no RFC 3339 timestamp is refused by name (both together
/// when both are unusable), and a window whose end does not fall strictly
/// after its start is refused on `ends_at`.
#[tokio::test]
async fn a_notice_body_refuses_blank_content_and_a_bad_window() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    for body in [
        json!({}),
        json!({"content": ""}),
        json!({"content": "  \n "}),
    ] {
        let refused = post_notice(&app, &token, &body).await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "a blank content");
        assert_eq!(
            refused.json_value(),
            json!({"errors": {"content": [BLANK]}}),
            "the required-text refusal: {}",
            refused.text()
        );
    }

    let refusals = [
        (
            json!({"content": "x", "starts_at": FUTURE, "ends_at": PAST}),
            json!({"errors": {"ends_at": [ENDS_AFTER]}}),
        ),
        (
            json!({"content": "x", "starts_at": PAST, "ends_at": PAST}),
            json!({"errors": {"ends_at": [ENDS_AFTER]}}),
        ),
        (
            json!({"content": "x", "starts_at": "yesterday", "ends_at": "06/01/2100"}),
            json!({"errors": {"starts_at": [INVALID_TIME], "ends_at": [INVALID_TIME]}}),
        ),
        (
            json!({"content": "x", "starts_at": "2026-02-01"}),
            json!({"errors": {"starts_at": [INVALID_TIME]}}),
        ),
    ];
    for (body, expected) in refusals {
        let refused = post_notice(&app, &token, &body).await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "a refused window");
        assert_eq!(
            refused.json_value(),
            expected,
            "the refusal the body earns: {}",
            refused.text()
        );
    }
    assert_eq!(notice_count(&pool).await, 0, "and no refusal wrote a row");
}

/// A `PATCH` checks the window it merges, not only the pair it was sent: a
/// `null` bound reads as absent, so the stored one still decides, and an
/// end moved before it is the same refusal with the row left alone.
#[tokio::test]
async fn a_notice_patch_checks_the_window_it_merges() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let stored = create_notice(
        &app,
        &token,
        &json!({"content": "anchored", "starts_at": PAST}),
    )
    .await;
    let id = entity_id(&stored);

    let nulled = patch(
        &app,
        &token,
        &format!("{NOTICES}/{id}"),
        &json!({"starts_at": null, "ends_at": "2000-06-02T00:00:00Z"}),
    )
    .await;
    common::assert_answer(&nulled, StatusCode::OK, "a null bound is no bound to write");
    let body = nulled.json_value();
    assert_eq!(body["starts_at"], PAST, "the stored start stands");
    assert_eq!(
        body["ends_at"], "2000-06-02T00:00:00.000Z",
        "and the new end is the one written"
    );

    let crossed = patch(
        &app,
        &token,
        &format!("{NOTICES}/{id}"),
        &json!({"ends_at": "1999-01-01T00:00:00Z"}),
    )
    .await;
    common::assert_answer(
        &crossed,
        StatusCode::BAD_REQUEST,
        "the merged window is checked",
    );
    assert_eq!(
        crossed.json_value(),
        json!({"errors": {"ends_at": [ENDS_AFTER]}}),
        "against the stored start, not just the sent pair"
    );
    let unchanged = get(&app, &token, &format!("{NOTICES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        unchanged["ends_at"], "2000-06-02T00:00:00.000Z",
        "the refusal wrote nothing"
    );
}

/// A notice id that is no uuid and one that names no row are the 404
/// `{"error":"Not found"}` on all three routes that take one.
#[tokio::test]
async fn an_unknown_announcement_is_the_not_found_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    for path in [
        format!("{NOTICES}/nope"),
        format!("{NOTICES}/{}", Uuid::now_v7()),
    ] {
        let shown = get(&app, &token, &path).await;
        common::assert_answer(&shown, StatusCode::NOT_FOUND, &format!("showing {path}"));
        common::assert_error(&shown, "Not found", &format!("showing {path}"));

        let changed = patch(&app, &token, &path, &json!({"content": "ghost"})).await;
        common::assert_answer(&changed, StatusCode::NOT_FOUND, "updating no notice");
        common::assert_error(&changed, "Not found", "updating no notice");

        let gone = destroy(&app, &token, &path).await;
        common::assert_answer(&gone, StatusCode::NOT_FOUND, "deleting no notice");
        common::assert_error(&gone, "Not found", "deleting no notice");
    }
    assert_eq!(
        notice_count(&pool).await,
        0,
        "and nothing was written on the way"
    );
}

/// A created notice is published to the sockets as Mastodon's
/// `announcement` event — the entity as the string-in-envelope, addressed
/// to every active local account.
#[tokio::test]
async fn a_created_notice_publishes_the_announcement_event() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = app_with_state(&pool);
    let (root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let mut rx = state.hub.subscribe();

    let refused = post_notice(&app, &token, &json!({"content": "  "})).await;
    common::assert_answer(&refused, StatusCode::BAD_REQUEST, "a blank create");
    assert!(
        matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
        "a refused write publishes nothing"
    );

    let entity = a_notice(&app, &token, "We are up").await;
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("the create published an event")
        .expect("the hub holds it");
    assert_eq!(event.event, "announcement", "Mastodon's announcement event");
    let mut streams: Vec<String> = event.streams.clone();
    streams.sort();
    let mut expected: Vec<String> = vec![format!("user:{}", root.id), format!("user:{}", alice.id)];
    expected.sort();
    assert_eq!(
        streams, expected,
        "one user stream per active local account"
    );
    assert!(
        matches!(&event.audience, Audience::Users(ids) if ids.len() == 2),
        "the same roster is the whole audience"
    );
    let payload: Value = serde_json::from_str(
        event
            .payload
            .as_deref()
            .expect("the event carries the entity"),
    )
    .expect("the payload is the entity's JSON");
    assert_eq!(payload, entity, "byte for byte what the route answered");
    assert!(
        event.render.is_none() && !event.local_only,
        "the entity rides, unflagged"
    );
}

/// The other writes keep the sockets quiet: Mastodon announces a new notice
/// only, so neither a patch nor a delete publishes anything.
#[tokio::test]
async fn neither_a_patched_nor_a_deleted_notice_is_published() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = app_with_state(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let entity = a_notice(&app, &token, "once").await;
    let id = entity_id(&entity);
    let mut rx = state.hub.subscribe();

    let patched = patch(
        &app,
        &token,
        &format!("{NOTICES}/{id}"),
        &json!({"content": "twice"}),
    )
    .await;
    common::assert_answer(&patched, StatusCode::OK, "patching the notice");
    let gone = destroy(&app, &token, &format!("{NOTICES}/{id}")).await;
    common::assert_answer(&gone, StatusCode::OK, "deleting the notice");
    assert!(
        matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
        "nothing after the create waits on the hub"
    );
}

/// A form-encoded body works as well as a JSON one: the stringly-typed
/// `priority` and `all_day` read through the same flex helpers a client's
/// form request needs, and the refusals are the same.
#[tokio::test]
async fn a_form_body_writes_both_kinds_of_content() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let rule_response = common::TestRequest::post(RULES)
        .bearer(&token)
        .form(&[("text", "No spam"), ("priority", "-3")])
        .send(&app)
        .await;
    common::assert_answer(&rule_response, StatusCode::OK, "a form-encoded rule");
    let body = rule_response.json_value();
    assert_eq!(
        body,
        json!({"id": body["id"], "text": "No spam", "hint": "", "priority": -3}),
        "the string number read as an integer"
    );

    let notice_response = common::TestRequest::post(NOTICES)
        .bearer(&token)
        .form(&[
            ("content", "Form text"),
            ("all_day", "true"),
            ("starts_at", PAST),
        ])
        .send(&app)
        .await;
    common::assert_answer(&notice_response, StatusCode::OK, "a form-encoded notice");
    let written = notice_response.json_value();
    assert_eq!(written["content"], "<p>Form text</p>", "the text rendered");
    assert_eq!(
        written["raw_content"],
        Value::Null,
        "which is no entity key"
    );
    assert_eq!(
        written["pleroma"]["raw_content"], "Form text",
        "the source is under the pleroma block"
    );
    assert_eq!(written["all_day"], true, "the string gate read as true");
    assert_eq!(written["starts_at"], PAST, "and the bound parsed");

    let blank = common::TestRequest::post(RULES)
        .bearer(&token)
        .form(&[("text", " ")])
        .send(&app)
        .await;
    assert_eq!(
        blank.json_value(),
        json!({"errors": {"text": [BLANK]}}),
        "the same refusal in a form body"
    );
}

// --- the gate --------------------------------------------------------------------------------

/// The tokens the gate tests hand the routes: the admin's roots, a
/// non-admin and a moderator carrying the same scopes, an app-only token,
/// an admin with only ordinary scopes, and the four narrow area scopes.
#[derive(Debug)]
struct Tokens {
    /// The admin's `admin:read admin:write`.
    all: String,
    /// A non-admin's same roots.
    plain: String,
    /// A moderator's same roots.
    staff: String,
    /// A user-unbound token with the same roots.
    app_only: String,
    /// An admin with `read write` and no admin grant.
    ordinary: String,
    /// An admin holding only `admin:read:rules`.
    read_rules: String,
    /// An admin holding only `admin:write:rules`.
    write_rules: String,
    /// An admin holding only `admin:read:announcements`.
    read_notices: String,
    /// An admin holding only `admin:write:announcements`.
    write_notices: String,
}

/// Mints the [`Tokens`] fixture.
async fn tokens(pool: &PgPool) -> Tokens {
    let (_root, all) = admin(pool, "root", ALL_ADMIN).await;
    let pleb = common::create_local_user(pool, "pleb").await;
    let mod_user = common::create_local_user_with(pool, "mod", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    Tokens {
        all,
        plain: common::create_app_and_token(pool, &pleb, "read write admin:read admin:write").await,
        staff: common::create_app_and_token(pool, &mod_user, ALL_ADMIN).await,
        app_only: common::create_app_only_token(pool, ALL_ADMIN).await,
        ordinary: admin(pool, "unscoped", "read write").await.1,
        read_rules: admin(pool, "reader", "admin:read:rules").await.1,
        write_rules: admin(pool, "mover", "admin:write:rules").await.1,
        read_notices: admin(pool, "watcher", "admin:read:announcements").await.1,
        write_notices: admin(pool, "poster", "admin:write:announcements").await.1,
    }
}

/// One request to each of the seven routes, for the walk of refusals.
fn routes() -> [fn() -> common::TestRequest; 7] {
    [
        || common::TestRequest::get(RULES),
        || common::TestRequest::post(RULES).json(&json!({"text": "x"})),
        || common::TestRequest::patch(&format!("{RULES}/1")).json(&json!({"text": "x"})),
        || common::TestRequest::delete(&format!("{RULES}/1")),
        || common::TestRequest::get(NOTICES),
        || common::TestRequest::post(NOTICES).json(&json!({"content": "x"})),
        || common::TestRequest::delete(&format!("{NOTICES}/1")),
    ]
}

/// The half of the gate that comes before any scope is consulted: no
/// credentials and an unknown token are the 401s Pleroma's user-identity
/// plug gives, on every route.
#[tokio::test]
async fn every_content_route_demands_a_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);

    for route in routes() {
        let unauthenticated = route().send(&app).await;
        common::assert_answer(&unauthenticated, StatusCode::UNAUTHORIZED, "no token");
        common::assert_error(&unauthenticated, "Invalid credentials", "no token");

        let bogus = route().bearer("not-a-real-token").send(&app).await;
        common::assert_answer(&bogus, StatusCode::UNAUTHORIZED, "a bogus token");
        common::assert_error(&bogus, "The access token is invalid", "a bogus token");
    }
}

/// The role half of the gate: an app-only token, a non-admin and a
/// moderator are refused on every route — Pleroma's `UserIsAdminPlug`
/// order, before the scope check and before a body is read.
#[tokio::test]
async fn every_content_route_demands_an_admin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let everyone = tokens(&pool).await;

    for route in routes() {
        let unbound = route().bearer(&everyone.app_only).send(&app).await;
        common::assert_answer(&unbound, StatusCode::FORBIDDEN, "an app-only token");
        common::assert_error(
            &unbound,
            "This API requires an authenticated user",
            "an app-only token",
        );

        for (token, who) in [
            (everyone.plain.as_str(), "a non-admin user"),
            (everyone.staff.as_str(), "a moderator"),
        ] {
            let refused = route().bearer(token).send(&app).await;
            common::assert_answer(&refused, StatusCode::FORBIDDEN, who);
            common::assert_error(&refused, "User is not an admin.", who);
        }
    }
    assert_eq!(rule_count(&pool).await, 0, "no refusal wrote a rule");
    assert_eq!(notice_count(&pool).await, 0, "or a notice");
}

/// The scope half of the gate: the wrong class on the right area, the right
/// class on the wrong area, and an admin with no admin grant at all are all
/// 403s naming the class root the endpoint needed.
#[tokio::test]
async fn the_content_scopes_refuse_the_wrong_class_and_area() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let everyone = tokens(&pool).await;

    let cases = [
        (
            get(&app, &everyone.write_rules, RULES).await,
            "a write scope on a read",
            "Insufficient permissions: admin:read.",
        ),
        (
            post_rule(&app, &everyone.read_rules, &json!({"text": "x"})).await,
            "a read scope on a write",
            "Insufficient permissions: admin:write.",
        ),
        (
            get(&app, &everyone.read_rules, NOTICES).await,
            "rule rights do not read notices",
            "Insufficient permissions: admin:read.",
        ),
        (
            post_rule(&app, &everyone.write_notices, &json!({"text": "x"})).await,
            "notice rights do not write rules",
            "Insufficient permissions: admin:write.",
        ),
        (
            get(&app, &everyone.ordinary, RULES).await,
            "an admin with no admin grant",
            "Insufficient permissions: admin:read.",
        ),
        (
            patch(
                &app,
                &everyone.read_notices,
                &format!("{NOTICES}/1"),
                &json!({"content": "x"}),
            )
            .await,
            "a notice read on a notice write",
            "Insufficient permissions: admin:write.",
        ),
    ];
    for (response, who, message) in cases {
        common::assert_answer(&response, StatusCode::FORBIDDEN, who);
        common::assert_error(&response, message, who);
    }
}

/// The grant half of the gate: each narrow area scope passes its own
/// routes, and the two areas are independent of one another.
#[tokio::test]
async fn the_content_scopes_pass_their_own_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let everyone = tokens(&pool).await;

    let written = a_rule(&app, &everyone.write_rules, "scoped", 0).await;
    let id = entity_id(&written);
    common::assert_answer(
        &patch(
            &app,
            &everyone.write_rules,
            &format!("{RULES}/{id}"),
            &json!({"priority": 1}),
        )
        .await,
        StatusCode::OK,
        "admin:write:rules on the patch",
    );
    common::assert_answer(
        &destroy(&app, &everyone.write_rules, &format!("{RULES}/{id}")).await,
        StatusCode::OK,
        "admin:write:rules on the delete",
    );
    common::assert_answer(
        &get(&app, &everyone.read_rules, RULES).await,
        StatusCode::OK,
        "admin:read:rules on the listing",
    );

    let notice = a_notice(&app, &everyone.write_notices, "scoped").await;
    let nid = entity_id(&notice);
    common::assert_answer(
        &get(&app, &everyone.read_notices, &format!("{NOTICES}/{nid}")).await,
        StatusCode::OK,
        "admin:read:announcements on the show",
    );
    common::assert_answer(
        &get(&app, &everyone.read_notices, NOTICES).await,
        StatusCode::OK,
        "and on the listing",
    );
    common::assert_answer(
        &patch(
            &app,
            &everyone.write_notices,
            &format!("{NOTICES}/{nid}"),
            &json!({"all_day": true}),
        )
        .await,
        StatusCode::OK,
        "admin:write:announcements on the patch",
    );
    common::assert_answer(
        &destroy(&app, &everyone.write_notices, &format!("{NOTICES}/{nid}")).await,
        StatusCode::OK,
        "and on the delete",
    );
    common::assert_answer(
        &get(&app, &everyone.all, RULES).await,
        StatusCode::OK,
        "and the broad roots of one token pass either area",
    );
}

/// The class roots are the broad grant: `admin:read` reads both listings
/// and `admin:write` writes both kinds of content, with no area scope in
/// either token.
#[tokio::test]
async fn the_admin_class_roots_pass_every_route_of_their_class() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_reader, read) = admin(&pool, "watcher", "admin:read").await;
    let (_mover, write) = admin(&pool, "pusher", "admin:write").await;

    let written = a_rule(&app, &write, "by the root", 1).await;
    let notice = a_notice(&app, &write, "by the root").await;
    let rules = get(&app, &read, RULES).await.json_value();
    assert_eq!(
        rules,
        json!([{"id": entity_id(&written), "text": "by the root", "hint": "", "priority": 1}]),
        "the read root lists what the write root made"
    );
    assert_eq!(
        ids_of(&get(&app, &read, NOTICES).await.json_value()),
        vec![entity_id(&notice)],
        "and reads the notice area too"
    );
}
