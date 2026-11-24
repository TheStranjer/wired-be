//! The admin rule routes over HTTP: create, patch, delete and listing, the
//! audit log, the refusals and the public reads that follow the writes.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::support::{
    ALL_ADMIN, BLANK, INSTANCE, PUBLIC_RULES, RULE_KEYS, RULES, TOO_LONG, a_rule, admin, app,
    create_rule, destroy, get, json_keys, log_entry, patch, post_rule, rule_count, texts_of,
};
use crate::common;

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
        Uuid::parse_str(&common::id_of(&full)).is_ok(),
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
            {"id": common::id_of(&bare), "text": "No spam", "hint": "", "priority": 0},
            {"id": common::id_of(&full), "text": "Be nice", "hint": "say hi first", "priority": 2},
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
    let id = common::id_of(&stored);

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
    let id = common::id_of(&doomed);

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
        common::ids_of(&get(&app, &token, RULES).await.json_value()),
        vec![common::id_of(&keeper)],
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
    let id = common::id_of(&written);

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
        common::ids_of(&listed),
        vec![
            common::id_of(&top),
            common::id_of(&one),
            common::id_of(&two),
            common::id_of(&last)
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
    let id = common::id_of(&stored);

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
            {"id": common::id_of(&keep), "text": "Rule one", "hint": "first"},
            {"id": common::id_of(&drop_me), "text": "Rule two", "hint": ""},
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

    let gone = destroy(
        &app,
        &token,
        &format!("{RULES}/{}", common::id_of(&drop_me)),
    )
    .await;
    common::assert_answer(&gone, StatusCode::OK, "deleting the second rule");
    assert_eq!(
        common::TestRequest::get(PUBLIC_RULES)
            .send(&app)
            .await
            .json_value(),
        json!([{"id": common::id_of(&keep), "text": "Rule one", "hint": "first"}]),
        "the withdrawal reaches the public read too"
    );
}

// --- the announcements ---------------------------------------------------------------------
