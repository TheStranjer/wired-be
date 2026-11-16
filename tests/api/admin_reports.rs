//! The admin reports surface over HTTP (`/api/v1/pleroma/admin/reports`
//! and the legacy `/api/v0/pleroma/reports`, WBE-200): the entity list
//! with its `state` / `rule_id` / `assigned_account` filters and its
//! `limit` / `page` / `page_size` window beside the total, the entity
//! show with its cited statuses rendered although the reading admin has
//! no visibility leave on them and its `assigned_account` omitted while
//! unassigned, the bulk `assign_account` pair, the notes lifecycle with
//! its `report_note` and `report_note_delete` moderation-log entries, and
//! the gate every route runs on — [`StaffUser`], so moderators pass where
//! non-staff are refused, on top of the `admin:read:reports` /
//! `admin:write:reports` scope check. The `PATCH` state change itself is
//! pinned in `webhooks.rs` (WBE-212); here it only joins the gate walk.
//!
//! Reports are seeded through [`wired_be::models::report`] directly — the
//! filing endpoint is `reports.rs`'s business — and the newest-first
//! expectations read the stored `ORDER BY id DESC` order back, so no
//! assertion depends on which millisecond a fixture's `UUIDv7` landed in.
//!
//! [`StaffUser`]: wired_be::auth::admin::StaffUser

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    api::admin::reports::MAX_NOTE_LENGTH,
    models::{
        instance_rule,
        report::{self, Category, NewReport, Report as ReportRow, ReportState},
        status::Visibility,
        user::{Flag, NewUser, User},
    },
};

use crate::common;

/// The admin reports route.
const ADMIN: &str = "/api/v1/pleroma/admin/reports";
/// The legacy v0 mount of the two admin reads.
const V0: &str = "/api/v0/pleroma/reports";
/// The 401 body when no token is presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for a valid token whose user is not staff.
const NOT_STAFF: &str = "User is not a staff member.";
/// The 403 body of a missing `admin:read` grant.
const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 403 body of a missing `admin:write` grant.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 404 body the report routes give a miss, per the ticket.
const NOT_FOUND: &str = "Not found";
/// The 400 body of a `state` off the three, per the ticket.
const UNSUPPORTED_STATE: &str = "Unsupported state";
/// The 400 body of a filter id that is no id.
const INVALID_ID: &str = "Invalid id";

/// A local user named `nickname` whose account is an admin, with a bearer
/// token granting `scopes`.
async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` whose account is a moderator, with a
/// bearer token granting `scopes`.
async fn moderator(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` with no staff role, with a bearer token
/// granting `scopes`.
async fn regular(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// The filing content with nothing but a comment: no rule, no citation.
fn filing(comment: &str) -> NewReport {
    NewReport {
        comment: comment.to_owned(),
        category: Category::Other,
        forwarded: false,
        rule_ids: Vec::new(),
        status_ids: Vec::new(),
    }
}

/// Files `reporter`'s report about `target` through the model.
async fn file(pool: &PgPool, reporter: &User, target: &User, new: &NewReport) -> ReportRow {
    report::create(pool, &common::test_config().base_url, reporter, target, new)
        .await
        .expect("the fixture report files")
}

/// `GET` of `path` with `query` as `token`, asserted 200 and JSON.
async fn list_ok(app: &Router, path: &str, query: &str, token: &str) -> Value {
    let response = common::TestRequest::get(&format!("{path}?{query}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "a staff read");
    response.json_value()
}

/// `GET {base}/{id}` as `token`, answering the whole response.
async fn show(app: &Router, base: &str, id: Uuid, token: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("{base}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// `POST {ADMIN}/assign_account` with `body` as `token`.
async fn assign(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(&format!("{ADMIN}/assign_account"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// An assignment the whole request resolves: asserted 200, answered as
/// the answer array's first entity.
async fn assigned_entity(app: &Router, token: &str, body: Value) -> Value {
    let answer = assign(app, token, body).await;
    common::assert_answer(&answer, StatusCode::OK, "the assignment");
    answer
        .json_value()
        .as_array()
        .expect("the answer array")
        .first()
        .expect("one entity")
        .clone()
}

/// The `id` fields of the `reports` array, in order.
fn report_ids(answer: &Value) -> Vec<String> {
    answer["reports"]
        .as_array()
        .expect("the reports array")
        .iter()
        .filter_map(|report| report["id"].as_str().map(str::to_owned))
        .collect()
}

/// Every stored report, newest first — the order the listing walks — as
/// `(id, comment)` text pairs.
async fn stored_rows(pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as::<_, (Uuid, String)>("SELECT id, comment FROM reports ORDER BY id DESC")
        .fetch_all(pool)
        .await
        .expect("the stored rows read")
        .into_iter()
        .map(|(id, comment)| (id.to_string(), comment))
        .collect()
}

/// The moderation-log rows of `action`, oldest first.
async fn log_lines(pool: &PgPool, action: &str) -> Vec<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT actor_nickname, message FROM moderation_log WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(pool)
    .await
    .expect("the log reads")
}

/// The listing answers the ticket's `{total, reports}` shape: every
/// stored report, newest first, each entity with its admin accounts and
/// its empty arrays, `assigned_account` omitted.
#[tokio::test]
async fn the_list_reports_the_entities_beside_the_total() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "ar_list_root", "admin:read:reports").await;
    let alice = common::create_local_user(&pool, "ar_list_alice").await;
    let bob = common::create_local_user(&pool, "ar_list_bob").await;
    for nth in 0..4 {
        let _filed = file(&pool, &alice, &bob, &filing(&format!("report {nth}"))).await;
    }
    let stored = stored_rows(&pool).await;

    let answer = list_ok(&app, ADMIN, "", &token).await;
    assert_eq!(answer["total"], json!(4), "the whole count");
    let ids = report_ids(&answer);
    let wanted: Vec<String> = stored.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(ids, wanted, "every report, newest filing first");
    let newest = answer["reports"]
        .as_array()
        .expect("the array")
        .iter()
        .find(|report| report["id"] == stored[0].0)
        .expect("the newest stored report");
    assert_eq!(newest["state"], "open", "a fresh filing is open");
    assert_eq!(newest["content"], stored[0].1, "the comment verbatim");
    assert_eq!(newest["account"]["id"], bob.id.to_string(), "the target");
    assert_eq!(newest["actor"]["id"], alice.id.to_string(), "the reporter");
    assert!(
        newest["account"].get("roles").is_some(),
        "the accounts are the admin render"
    );
    assert_eq!(newest["statuses"], json!([]), "nothing cited");
    assert_eq!(newest["notes"], json!([]), "nothing noted");
    assert!(
        newest.get("assigned_account").is_none(),
        "and unassigned omits the key: {newest}"
    );
}

/// The window parameters cut the page while the total stays the whole
/// count: `limit`, `page` walking it, `page_size` winning over `limit`,
/// the admin cap, the lenient junk and the off-the-end page.
#[tokio::test]
async fn the_listing_windows_by_limit_page_and_page_size() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "ar_page_root", "admin:read:reports").await;
    let alice = common::create_local_user(&pool, "ar_page_alice").await;
    let bob = common::create_local_user(&pool, "ar_page_bob").await;
    for nth in 0..4 {
        let _filed = file(&pool, &alice, &bob, &filing(&format!("page {nth}"))).await;
    }
    let stored: Vec<String> = stored_rows(&pool)
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    let limited = list_ok(&app, ADMIN, "limit=2", &token).await;
    assert_eq!(report_ids(&limited), stored[..2], "`limit` cuts the window");
    assert_eq!(limited["total"], json!(4), "while the total counts all");
    let second = list_ok(&app, ADMIN, "limit=2&page=2", &token).await;
    assert_eq!(report_ids(&second), stored[2..], "`page` walks it");
    let sized = list_ok(&app, ADMIN, "limit=1&page_size=3", &token).await;
    assert_eq!(
        report_ids(&sized),
        stored[..3],
        "`page_size` wins over `limit`"
    );
    let default = list_ok(&app, ADMIN, "page_size=5000", &token).await;
    assert_eq!(
        report_ids(&default).len(),
        4,
        "an oversized page still answers what exists"
    );
    let past = list_ok(&app, ADMIN, "page=3&limit=2", &token).await;
    assert_eq!(report_ids(&past), Vec::<String>::new(), "off the end");
    assert_eq!(past["total"], json!(4), "beside the whole total");
    let junk = list_ok(&app, ADMIN, "page=nope&limit=x&page_size=-3", &token).await;
    assert_eq!(report_ids(&junk), stored, "unusable numbers fall through");
}

/// The `state` filter selects exactly the reports in the named state,
/// and a state off the three is the ticket's 400.
#[tokio::test]
async fn the_list_filters_by_state() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "ar_state_root", "admin:read:reports").await;
    let alice = common::create_local_user(&pool, "ar_state_alice").await;
    let bob = common::create_local_user(&pool, "ar_state_bob").await;
    let open = file(&pool, &alice, &bob, &filing("open one")).await;
    let closed = file(&pool, &alice, &bob, &filing("closed one")).await;
    let resolved = file(&pool, &alice, &bob, &filing("resolved one")).await;
    for (row, state) in [
        (&closed, ReportState::Closed),
        (&resolved, ReportState::Resolved),
    ] {
        let _moved = report::set_state(&pool, row.id, state)
            .await
            .expect("the fixture moves")
            .expect("the report exists");
    }

    for (state, wanted) in [
        ("open", open.id.to_string()),
        ("closed", closed.id.to_string()),
        ("resolved", resolved.id.to_string()),
    ] {
        let answer = list_ok(&app, ADMIN, &format!("state={state}"), &token).await;
        assert_eq!(report_ids(&answer), vec![wanted], "the `{state}` page");
        assert_eq!(answer["total"], json!(1), "counted to the filter");
    }

    let refused = common::TestRequest::get(&format!("{ADMIN}?state=fixed"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&refused, StatusCode::BAD_REQUEST, "a state off the three");
    common::assert_error(&refused, UNSUPPORTED_STATE, "a state off the three");
}

/// The `rule_id` filter keeps the reports citing that rule and the
/// `assigned_account` filter those assigned to that account; a filter id
/// that is no id is the 400.
#[tokio::test]
async fn the_list_filters_by_rule_and_assignee() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "ar_filter_root", "admin:read:reports").await;
    let alice = common::create_local_user(&pool, "ar_filter_alice").await;
    let bob = common::create_local_user(&pool, "ar_filter_bob").await;
    let moddie = common::create_local_user(&pool, "ar_filter_mod").await;
    let rule = instance_rule::create(&pool, "No harassment", "", 1)
        .await
        .expect("the rule fixture inserts");
    let citing = file(
        &pool,
        &alice,
        &bob,
        &NewReport {
            rule_ids: vec![rule.id],
            ..filing("citing")
        },
    )
    .await;
    let plain = file(&pool, &alice, &bob, &filing("plain")).await;
    let _assigned = report::assign(&pool, plain.id, Some(moddie.id))
        .await
        .expect("the fixture assigns")
        .expect("the report exists");

    let by_rule = list_ok(&app, ADMIN, &format!("rule_id={}", rule.id), &token).await;
    assert_eq!(
        report_ids(&by_rule),
        vec![citing.id.to_string()],
        "the citing report only"
    );
    let by_assignee = list_ok(
        &app,
        ADMIN,
        &format!("assigned_account={}", moddie.id),
        &token,
    )
    .await;
    assert_eq!(
        report_ids(&by_assignee),
        vec![plain.id.to_string()],
        "the assigned report only"
    );

    for key in ["rule_id", "assigned_account"] {
        let refused = common::TestRequest::get(&format!("{ADMIN}?{key}=not-a-uuid"))
            .bearer(&token)
            .send(&app)
            .await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "a broken filter id");
        common::assert_error(&refused, INVALID_ID, &format!("the {key} filter"));
    }
}

/// The show answers the entity with its status, note and assignee slots
/// filled, and both halves of a miss — no id and no report — are the
/// ticket's 404 `{"error":"Not found"}`.
#[tokio::test]
async fn the_show_answers_the_entity_and_a_miss_is_the_pinned_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (root, token) = admin(&pool, "ar_show_root", "admin:read:reports").await;
    let alice = common::create_local_user(&pool, "ar_show_alice").await;
    let bob = common::create_local_user(&pool, "ar_show_bob").await;
    let cited = common::create_status(&pool, &bob, "the reported post", Visibility::Public).await;
    let row = file(
        &pool,
        &alice,
        &bob,
        &NewReport {
            status_ids: vec![cited.id],
            ..filing("show me")
        },
    )
    .await;
    let _note = report::create_note(&pool, row.id, root.id, "triaged")
        .await
        .expect("the fixture note writes");
    let _assigned = report::assign(&pool, row.id, Some(root.id))
        .await
        .expect("the fixture assigns")
        .expect("the report exists");

    let response = show(&app, ADMIN, row.id, &token).await;
    common::assert_answer(&response, StatusCode::OK, "the show");
    let entity = response.json_value();
    assert_eq!(entity["id"], row.id.to_string(), "the id as text");
    assert_eq!(entity["state"], "open", "the stored state");
    assert_eq!(entity["content"], "show me", "the comment");
    assert!(
        wired_be::util::time::parse_time(entity["created_at"].as_str().expect("a stamp")).is_some(),
        "the filing time is the decision 5 format"
    );
    assert_eq!(
        entity["statuses"][0]["id"],
        cited.id.to_string(),
        "the cited status rides in the entity"
    );
    assert_eq!(entity["notes"][0]["content"], "triaged", "the note");
    assert_eq!(
        entity["notes"][0]["user_id"],
        root.id.to_string(),
        "authored by the fixture admin"
    );
    assert_eq!(
        entity["assigned_account"]["nickname"], "ar_show_root",
        "the assignee is named"
    );

    let unknown = show(&app, ADMIN, Uuid::now_v7(), &token).await;
    common::assert_answer(&unknown, StatusCode::NOT_FOUND, "an unknown report");
    common::assert_error(&unknown, NOT_FOUND, "an unknown report");
    let malformed = common::TestRequest::get(&format!("{ADMIN}/nonsense"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&malformed, StatusCode::NOT_FOUND, "a malformed id");
    common::assert_error(&malformed, NOT_FOUND, "a malformed id");
}

/// The entity's statuses are rendered with the reading admin as viewer
/// and no visibility filter: a private post and a direct post the cited
/// account sent stay out of the admin's own status reads but come back
/// inside the report.
#[tokio::test]
async fn the_entity_shows_statuses_the_admins_own_reads_refuse() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "ar_vis_root", "admin:read:reports read").await;
    let alice = common::create_local_user(&pool, "ar_vis_alice").await;
    let bob = common::create_local_user(&pool, "ar_vis_bob").await;
    let hidden = common::create_status(&pool, &bob, "followers only", Visibility::Private).await;
    let directed = common::create_status(&pool, &bob, "mentioned only", Visibility::Direct).await;
    let row = file(
        &pool,
        &alice,
        &bob,
        &NewReport {
            status_ids: vec![hidden.id, directed.id],
            ..filing("invisible posts")
        },
    )
    .await;

    let control = common::TestRequest::get(&format!("/api/v1/statuses/{}", hidden.id))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(
        &control,
        StatusCode::NOT_FOUND,
        "the admin's own status read",
    );

    let entity = show(&app, ADMIN, row.id, &token).await.json_value();
    let ids: Vec<&str> = entity["statuses"]
        .as_array()
        .expect("the statuses array")
        .iter()
        .filter_map(|status| status["id"].as_str())
        .collect();
    assert_eq!(
        ids,
        vec![hidden.id.to_string(), directed.id.to_string()],
        "both cited posts answer inside the report, oldest first"
    );
}

/// A written note answers 204, lands in the entity authored by its
/// writer, and leaves the `report_note` entry naming the text and the
/// target; the delete answers 204, empties the entity, and logs the
/// `report_note_delete` entry with the removed text.
#[tokio::test]
async fn notes_are_written_and_deleted_with_their_log_entries() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (root, token) = admin(
        &pool,
        "ar_notes_root",
        "admin:read:reports admin:write:reports",
    )
    .await;
    let alice = common::create_local_user(&pool, "ar_notes_alice").await;
    let bob = common::create_local_user(&pool, "ar_notes_bob").await;
    let row = file(&pool, &alice, &bob, &filing("note me")).await;

    let added = common::TestRequest::post(&format!("{ADMIN}/{}/notes", row.id))
        .bearer(&token)
        .json(&json!({"content": "looked at it"}))
        .send(&app)
        .await;
    common::assert_answer(&added, StatusCode::NO_CONTENT, "a written note");

    let shown = show(&app, ADMIN, row.id, &token).await.json_value();
    assert_eq!(
        shown["notes"][0]["content"], "looked at it",
        "in the entity"
    );
    assert_eq!(
        shown["notes"][0]["user_id"],
        root.id.to_string(),
        "authored by the caller"
    );
    let note_id = shown["notes"][0]["id"]
        .as_str()
        .expect("the note id")
        .to_owned();
    assert_eq!(
        log_lines(&pool, "report_note").await,
        vec![(
            "ar_notes_root".to_owned(),
            format!(
                "@ar_notes_root added note 'looked at it' to report ##{} on user @ar_notes_bob",
                row.id
            )
        )],
        "the report_note entry names the text and the target"
    );

    let deleted = common::TestRequest::delete(&format!("{ADMIN}/{}/notes/{note_id}", row.id))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&deleted, StatusCode::NO_CONTENT, "the delete");
    let after = show(&app, ADMIN, row.id, &token).await.json_value();
    assert_eq!(after["notes"], json!([]), "gone from the entity");
    let unlogged = log_lines(&pool, "report_note_delete").await;
    assert_eq!(
        unlogged,
        vec![(
            "ar_notes_root".to_owned(),
            format!(
                "@ar_notes_root deleted note 'looked at it' from report ##{} on user @ar_notes_bob",
                row.id
            )
        )],
        "the removal logged Pleroma's report_note_delete sentence"
    );

    let again = common::TestRequest::delete(&format!("{ADMIN}/{}/notes/{note_id}", row.id))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&again, StatusCode::NOT_FOUND, "the second delete");
    common::assert_error(&again, NOT_FOUND, "the second delete");
}

/// The note writes refuse their bodies and their misses: blank, past the
/// ticket's 5000-character cap, and `content`-less bodies are 422s while
/// the cap itself fits; a note on no report and a note of another report
/// are the 404.
#[tokio::test]
async fn the_note_writes_refuse_bad_bodies_and_foreign_reports() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(
        &pool,
        "ar_refuse_root",
        "admin:read:reports admin:write:reports",
    )
    .await;
    let alice = common::create_local_user(&pool, "ar_refuse_alice").await;
    let bob = common::create_local_user(&pool, "ar_refuse_bob").await;
    let one = file(&pool, &alice, &bob, &filing("one")).await;
    let two = file(&pool, &alice, &bob, &filing("two")).await;

    for (body, who) in [
        (json!({"content": ""}), "a blank note"),
        (
            json!({"content": "x".repeat(MAX_NOTE_LENGTH + 1)}),
            "a note past the character cap",
        ),
        (json!({}), "a body without content"),
    ] {
        let refused = common::TestRequest::post(&format!("{ADMIN}/{}/notes", one.id))
            .bearer(&token)
            .json(&body)
            .send(&app)
            .await;
        common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, who);
    }
    let edge = common::TestRequest::post(&format!("{ADMIN}/{}/notes", one.id))
        .bearer(&token)
        .json(&json!({"content": "x".repeat(MAX_NOTE_LENGTH)}))
        .send(&app)
        .await;
    common::assert_answer(&edge, StatusCode::NO_CONTENT, "the cap itself fits");

    let entity = show(&app, ADMIN, one.id, &token).await.json_value();
    let note_id = entity["notes"][0]["id"].as_str().expect("the note id");
    let foreign = common::TestRequest::delete(&format!("{ADMIN}/{}/notes/{}", two.id, note_id))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&foreign, StatusCode::NOT_FOUND, "a note of another report");

    let nowhere = common::TestRequest::post(&format!("{ADMIN}/{}/notes", Uuid::now_v7()))
        .bearer(&token)
        .json(&json!({"content": "orphan"}))
        .send(&app)
        .await;
    common::assert_answer(&nowhere, StatusCode::NOT_FOUND, "a note on no report");
    common::assert_error(&nowhere, NOT_FOUND, "a note on no report");
}

/// The assignment answers the updated entity with its new
/// `assigned_account`, hands the report back on the `null` nickname, and
/// answers unresolved elements with Pleroma's 400 `not_found` list while
/// the elements that did resolve stay assigned.
#[tokio::test]
async fn assign_account_moves_reports_and_answers_entities() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (root, token) = admin(&pool, "ar_assign_root", "admin:write:reports").await;
    let alice = common::create_local_user(&pool, "ar_assign_alice").await;
    let bob = common::create_local_user(&pool, "ar_assign_bob").await;
    let row = file(&pool, &alice, &bob, &filing("assign me")).await;

    let entity = assigned_entity(
        &app,
        &token,
        json!({"reports": [{"id": row.id, "assigned_account": "ar_assign_root"}]}),
    )
    .await;
    assert_eq!(entity["id"], row.id.to_string(), "the updated report");
    assert_eq!(
        entity["assigned_account"]["id"],
        root.id.to_string(),
        "with its new owner"
    );

    let freed = assigned_entity(
        &app,
        &token,
        json!({"reports": [{"id": row.id, "assigned_account": null}]}),
    )
    .await;
    assert!(
        freed.get("assigned_account").is_none(),
        "the null nickname hands it back and the key is gone: {freed}"
    );

    let refused = assign(
        &app,
        &token,
        json!({"reports": [
            {"id": "nonsense", "assigned_account": "ar_assign_root"},
            {"id": row.id, "assigned_account": "nobody-here"},
        ]}),
    )
    .await;
    common::assert_answer(&refused, StatusCode::BAD_REQUEST, "nothing resolved");
    assert_eq!(
        refused.json_value(),
        json!([
            {"id": "nonsense", "error": "not_found"},
            {"id": row.id.to_string(), "error": "not_found"},
        ]),
        "Pleroma's per-item list"
    );

    let mixed = assign(
        &app,
        &token,
        json!({"reports": [
            {"id": "nonsense", "assigned_account": "ar_assign_root"},
            {"id": row.id, "assigned_account": "ar_assign_root"},
        ]}),
    )
    .await;
    common::assert_answer(&mixed, StatusCode::BAD_REQUEST, "the mixed request");
    let kept = report::get(&pool, row.id)
        .await
        .expect("the read runs")
        .expect("the report exists");
    assert_eq!(
        kept.assigned_to,
        Some(root.id),
        "and the element that resolved stayed assigned"
    );
}

/// The gate's refusals on every route: no token is 401 "Invalid
/// credentials", and a valid token whose user is not staff is 403
/// "User is not a staff member." even holding the broad admin scopes.
#[tokio::test]
async fn anonymous_and_non_staff_are_refused_every_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, plain_token) = regular(&pool, "ar_gate_plain", "admin:read admin:write").await;
    let alice = common::create_local_user(&pool, "ar_gate_alice").await;
    let bob = common::create_local_user(&pool, "ar_gate_bob").await;
    let row = file(&pool, &alice, &bob, &filing("gated")).await;

    let anonymous = vec![
        common::TestRequest::get(ADMIN).send(&app).await,
        common::TestRequest::get(&format!("{ADMIN}/{}", row.id))
            .send(&app)
            .await,
        common::TestRequest::patch(ADMIN)
            .json(&json!({"reports": []}))
            .send(&app)
            .await,
        common::TestRequest::post(&format!("{ADMIN}/assign_account"))
            .json(&json!({"reports": []}))
            .send(&app)
            .await,
        common::TestRequest::post(&format!("{ADMIN}/{}/notes", row.id))
            .json(&json!({"content": "x"}))
            .send(&app)
            .await,
        common::TestRequest::delete(&format!("{ADMIN}/{}/notes/{}", row.id, Uuid::now_v7()))
            .send(&app)
            .await,
    ];
    common::assert_all_answered(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_all_errors(&anonymous, NO_TOKEN, "no token");

    let not_staff = vec![
        common::TestRequest::get(ADMIN)
            .bearer(&plain_token)
            .send(&app)
            .await,
        show(&app, ADMIN, row.id, &plain_token).await,
        common::TestRequest::patch(ADMIN)
            .bearer(&plain_token)
            .json(&json!({"reports": []}))
            .send(&app)
            .await,
        common::TestRequest::post(&format!("{ADMIN}/assign_account"))
            .bearer(&plain_token)
            .json(&json!({"reports": []}))
            .send(&app)
            .await,
        common::TestRequest::post(&format!("{ADMIN}/{}/notes", row.id))
            .bearer(&plain_token)
            .json(&json!({"content": "x"}))
            .send(&app)
            .await,
    ];
    common::assert_all_refused(
        &not_staff,
        StatusCode::FORBIDDEN,
        NOT_STAFF,
        "a non-staff user",
    );
}

/// The scope half of the gate: the class root or the area sub-scope of
/// the right class grants, a moderator with the narrow areas passes both
/// verbs, and the note a moderator writes logs under the moderator's
/// nickname.
#[tokio::test]
async fn the_report_scopes_gate_reads_and_writes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (modder, mod_read) = moderator(&pool, "ar_scope_mod", "admin:read:reports").await;
    let (writer, mod_write) = moderator(&pool, "ar_scope_mod2", "admin:write:reports").await;
    let (_, write_only) = admin(&pool, "ar_scope_writer", "admin:write:reports").await;
    let (_, other_area) = admin(&pool, "ar_scope_accounts", "admin:read:accounts").await;
    let alice = common::create_local_user(&pool, "ar_scope_alice").await;
    let bob = common::create_local_user(&pool, "ar_scope_bob").await;
    let row = file(&pool, &alice, &bob, &filing("scoped")).await;

    let refused = vec![
        common::TestRequest::get(ADMIN)
            .bearer(&write_only)
            .send(&app)
            .await,
        common::TestRequest::get(ADMIN)
            .bearer(&other_area)
            .send(&app)
            .await,
    ];
    common::assert_all_refused(
        &refused,
        StatusCode::FORBIDDEN,
        NO_ADMIN_READ,
        "a wrong read grant",
    );
    let no_write = common::TestRequest::post(&format!("{ADMIN}/{}/notes", row.id))
        .bearer(&mod_read)
        .json(&json!({"content": "not yet"}))
        .send(&app)
        .await;
    common::assert_answer(&no_write, StatusCode::FORBIDDEN, "a read-only writer");
    common::assert_error(&no_write, NO_ADMIN_WRITE, "a read-only writer");

    common::assert_answer(
        &common::TestRequest::get(ADMIN)
            .bearer(&mod_read)
            .send(&app)
            .await,
        StatusCode::OK,
        "a moderator reading reports",
    );
    let modded_note = common::TestRequest::post(&format!("{ADMIN}/{}/notes", row.id))
        .bearer(&mod_write)
        .json(&json!({"content": "a moderator looked"}))
        .send(&app)
        .await;
    common::assert_answer(&modded_note, StatusCode::NO_CONTENT, "a moderator's note");
    assert_eq!(
        log_lines(&pool, "report_note").await,
        vec![(
            "ar_scope_mod2".to_owned(),
            format!(
                "@ar_scope_mod2 added note 'a moderator looked' to report ##{} on user @ar_scope_bob",
                row.id
            )
        )],
        "the entry names the writing moderator {} and the target",
        writer.nickname
    );
    assert_ne!(modder.nickname, writer.nickname, "two distinct moderators");
}

/// The legacy `/api/v0/pleroma` pair serves the same two reads — the same
/// `{total, reports}` answer, the same entity, the same filters — behind
/// the same staff-and-scope gate.
#[tokio::test]
async fn the_legacy_v0_pair_serves_the_same_entities_behind_the_same_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "ar_v0_root", "admin:read:reports").await;
    let alice = common::create_local_user(&pool, "ar_v0_alice").await;
    let bob = common::create_local_user(&pool, "ar_v0_bob").await;
    let kept = file(&pool, &alice, &bob, &filing("kept")).await;
    let dropped = file(&pool, &alice, &bob, &filing("dropped")).await;
    let _closed = report::set_state(&pool, dropped.id, ReportState::Closed)
        .await
        .expect("the fixture closes")
        .expect("the report exists");

    let listed = list_ok(&app, V0, "", &token).await;
    let admin_listed = list_ok(&app, ADMIN, "", &token).await;
    assert_eq!(listed["total"], json!(2), "the same total");
    assert_eq!(
        report_ids(&listed),
        report_ids(&admin_listed),
        "the same page in the same order"
    );
    let filtered = list_ok(&app, V0, "state=open", &token).await;
    assert_eq!(
        report_ids(&filtered),
        vec![kept.id.to_string()],
        "the same filters"
    );

    let shown = show(&app, V0, kept.id, &token).await;
    common::assert_answer(&shown, StatusCode::OK, "the legacy show");
    let admin_shown = show(&app, ADMIN, kept.id, &token).await;
    assert_eq!(
        shown.json_value(),
        admin_shown.json_value(),
        "one and the same entity, rendered for the same viewer"
    );
    assert_eq!(
        shown.json_value()["actor"]["id"],
        alice.id.to_string(),
        "the admin wrappers ride along"
    );

    let (_, plain_token) = regular(&pool, "ar_v0_plain", "admin:read").await;
    let not_staff = common::TestRequest::get(V0)
        .bearer(&plain_token)
        .send(&app)
        .await;
    common::assert_answer(&not_staff, StatusCode::FORBIDDEN, "a non-staff v0 read");
    common::assert_error(&not_staff, NOT_STAFF, "a non-staff v0 read");
    let anonymous = show(&app, V0, kept.id, "no-token").await;
    common::assert_answer(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "the anonymous v0 read",
    );
}
