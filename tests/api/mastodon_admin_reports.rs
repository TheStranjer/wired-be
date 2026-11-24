//! The Mastodon admin reports surface over HTTP (`/api/v1/admin/reports`,
//! WBE-214): Pleroma's `MastodonAPI.Admin.ReportController` — the queue
//! listing with its `resolved` / `account_id` / `target_account_id` filters
//! and its keyset `Link` header keyed on the report ids, the fourteen-key
//! entity with its four admin accounts, its rendered statuses and its cited
//! rules, the four moderation writes each answering the fresh entity and
//! leaving its moderation-log entry, the one 404 `{"error":"Record not
//! found"}` every miss answers with, and the [`StaffUser`]-plus-scope gate
//! on all six routes.
//!
//! Reports are seeded through [`wired_be::models::report`] directly — the
//! filing endpoint is `reports.rs`'s business, and the state transitions
//! under test would be circular through their own routes — and the
//! newest-first expectations read the stored order back, so no assertion
//! depends on which millisecond a fixture's `UUIDv7` landed in.
//!
//! [`StaffUser`]: wired_be::auth::admin::StaffUser

#![cfg(test)]

use std::collections::BTreeSet;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    models::{
        instance_rule,
        report::{self, Category, NewReport, Report as ReportRow, ReportState},
        status::{self, Visibility},
        user::{Flag, NewUser, User},
    },
    util::time::mastodon_time,
};

use crate::common::{self, TestRequest};

/// The routes' prefix.
const ADMIN: &str = "/api/v1/admin/reports";
/// The 401 body when no token is presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body when a presented token does not resolve.
const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body for an app-only (user-unbound) token.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body for a valid token whose user is not staff.
const NOT_STAFF: &str = "User is not a staff member.";
/// The 403 body of a token that grants neither `admin:read` nor
/// `admin:read:reports`.
const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 403 body of a token that grants neither `admin:write` nor
/// `admin:write:reports`.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 404 body of every miss on these routes, per the ticket.
const NOT_FOUND: &str = "Record not found";
/// The 400 body of a filter id that is no id.
const INVALID_ID: &str = "Invalid id";
/// The entity's fourteen keys, sorted the way [`keys`] reports them.
const ENTITY_KEYS: [&str; 14] = [
    "account",
    "action_taken",
    "action_taken_at",
    "action_taken_by_account",
    "assigned_account",
    "category",
    "comment",
    "created_at",
    "forwarded",
    "id",
    "rules",
    "statuses",
    "target_account",
    "updated_at",
];

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

/// The stored report behind `id`, which the test expects to exist.
async fn stored(pool: &PgPool, id: Uuid) -> ReportRow {
    report::get(pool, id)
        .await
        .expect("the report reads")
        .expect("the report exists")
}

/// `GET` of `path` as `token` (`None` is the anonymous request), answering
/// the whole response.
async fn get(app: &Router, path: &str, token: Option<&str>) -> common::TestResponse {
    let request = TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `GET` of `path` as `token`, asserted 200, answering the parsed body.
async fn read(app: &Router, path: &str, token: &str) -> Value {
    let response = get(app, path, Some(token)).await;
    common::assert_answer(&response, StatusCode::OK, "a staff read");
    response.json_value()
}

/// The listing of `query` (an empty string is the bare route).
async fn list(app: &Router, query: &str, token: &str) -> Value {
    let path = if query.is_empty() {
        ADMIN.to_owned()
    } else {
        format!("{ADMIN}?{query}")
    };
    read(app, &path, token).await
}

/// The ids a listing must answer for `wanted`, as entity-key text.
fn wanted_ids(wanted: &[Uuid]) -> Vec<String> {
    wanted.iter().map(ToString::to_string).collect()
}

/// `POST {ADMIN}/{id}/{suffix}` as `token` (`None` is the anonymous
/// request), answering the whole response.
async fn write(app: &Router, token: Option<&str>, id: Uuid, suffix: &str) -> common::TestResponse {
    let request = TestRequest::post(&format!("{ADMIN}/{id}/{suffix}"));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `POST {ADMIN}/{id}/{suffix}` as `token`, asserted 200, answering the
/// returned entity.
async fn write_entity(app: &Router, token: &str, id: Uuid, suffix: &str) -> Value {
    let answer = write(app, Some(token), id, suffix).await;
    common::assert_answer(&answer, StatusCode::OK, &format!("the {suffix} write"));
    answer.json_value()
}

/// The moderation-log rows of `action`, oldest first, as `(actor,
/// message)` pairs.
async fn log_lines(pool: &PgPool, action: &str) -> Vec<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT actor_nickname, message FROM moderation_log WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(pool)
    .await
    .expect("the log reads")
}

/// The keys of one rendered object, sorted so a missing or an extra one
/// shows up as a diff rather than a lucky pass.
fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect()
}

/// The full fixture of the entity tests: `alice` reports `bob` with a
/// comment, a category, a forward request, one cited private status (so
/// the admin's visibility-free rendering is on display) and two cited
/// rules in a citation order the rule table's own priority order
/// contradicts.
struct Filed {
    /// The filed report.
    row: ReportRow,
    /// The reporter.
    alice: User,
    /// The reported account.
    bob: User,
    /// The cited status.
    cited: status::Status,
    /// The first-cited rule (the table's later row).
    first: instance_rule::InstanceRule,
    /// The second-cited rule (the table's earlier row), named back in the
    /// rules array to prove the citation order rides on the answer.
    second: instance_rule::InstanceRule,
}

/// Files [`Filed`].
async fn filed(pool: &PgPool) -> Filed {
    let alice = common::create_local_user(pool, "mar_alice").await;
    let bob = common::create_local_user(pool, "mar_bob").await;
    let cited = common::create_status(pool, &bob, "the reported post", Visibility::Private).await;
    let second = instance_rule::create(pool, "No spam", "", 1)
        .await
        .expect("the spam rule inserts");
    let first = instance_rule::create(pool, "No harassment", "keep it civil", 2)
        .await
        .expect("the harassment rule inserts");
    let row = file(
        pool,
        &alice,
        &bob,
        &NewReport {
            category: Category::Violation,
            forwarded: true,
            rule_ids: vec![first.id, second.id],
            status_ids: vec![cited.id],
            ..filing("this is harassment")
        },
    )
    .await;
    Filed {
        row,
        alice,
        bob,
        cited,
        first,
        second,
    }
}

/// Every read route walks the gate: no token is 401, an unknown one 401,
/// an app-only token 403, a non-staff account's token 403 and a staff
/// token of the wrong area 403 — while the area grant and the broad
/// `admin:read` root both read, listing and showing alike.
#[tokio::test]
async fn the_reads_walk_the_staff_and_scope_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, scoped) = admin(&pool, "mar_gate_root", "admin:read:reports").await;
    let (_, broad) = admin(&pool, "mar_gate_broad", "admin:read").await;
    let (_, other_area) = moderator(&pool, "mar_gate_other", "admin:read:accounts").await;
    let (_, plain) = regular(&pool, "mar_gate_plain", "admin:read:reports").await;
    let app_only = common::create_app_only_token(&pool, "admin:read:reports").await;
    let alice = common::create_local_user(&pool, "mar_gate_alice").await;
    let bob = common::create_local_user(&pool, "mar_gate_bob").await;
    let row = file(&pool, &alice, &bob, &filing("gate walk")).await;
    let show = format!("{ADMIN}/{}", row.id);

    for path in [ADMIN.to_owned(), show.clone()] {
        let anonymous = get(&app, &path, None).await;
        common::assert_answer(
            &anonymous,
            StatusCode::UNAUTHORIZED,
            "a request with no token",
        );
        common::assert_error(&anonymous, NO_TOKEN, "a request with no token");
        let bogus = get(&app, &path, Some("no-such-token")).await;
        common::assert_answer(&bogus, StatusCode::UNAUTHORIZED, "an unknown token");
        common::assert_error(&bogus, INVALID_TOKEN, "an unknown token");
        let client = get(&app, &path, Some(&app_only)).await;
        common::assert_answer(&client, StatusCode::FORBIDDEN, "an app-only token");
        assert_eq!(client.json_value()["error"], USER_REQUIRED, "its wording");
        let stranger = get(&app, &path, Some(&plain)).await;
        common::assert_answer(&stranger, StatusCode::FORBIDDEN, "a non-staff token");
        common::assert_error(&stranger, NOT_STAFF, "a non-staff token");
        let wrong_area = get(&app, &path, Some(&other_area)).await;
        common::assert_answer(&wrong_area, StatusCode::FORBIDDEN, "the other area's grant");
        common::assert_error(&wrong_area, NO_ADMIN_READ, "the other area's grant");
    }

    // The granting arms, both paths and both grant spellings.
    for token in [&scoped, &broad] {
        let _listed = read(&app, ADMIN, token).await;
        let _shown = read(&app, &show, token).await;
    }
}

/// Every write route walks the gate the same way, with the write class: a
/// staff token that only reads is the 403 naming `admin:write`, and an
/// admin or a moderator holding `admin:write:reports` writes.
#[tokio::test]
async fn the_writes_walk_the_staff_and_scope_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (root, root_read) = admin(&pool, "mar_wg_root", "admin:read:reports").await;
    let root_write = common::create_app_and_token(&pool, &root, "admin:write:reports").await;
    let (moddy, moddy_write) = moderator(&pool, "mar_wg_moddy", "admin:write:reports").await;
    let (_, plain) = regular(&pool, "mar_wg_plain", "admin:write:reports").await;
    let app_only = common::create_app_only_token(&pool, "admin:write:reports").await;
    let row = file(&pool, &root, &moddy, &filing("gate walk")).await;

    for suffix in ["resolve", "reopen", "assign_to_self", "unassign"] {
        let anonymous = write(&app, None, row.id, suffix).await;
        common::assert_answer(
            &anonymous,
            StatusCode::UNAUTHORIZED,
            "a request with no token",
        );
        common::assert_error(&anonymous, NO_TOKEN, "a request with no token");
        let client = write(&app, Some(&app_only), row.id, suffix).await;
        common::assert_answer(&client, StatusCode::FORBIDDEN, "an app-only token");
        assert_eq!(client.json_value()["error"], USER_REQUIRED, "its wording");
        let stranger = write(&app, Some(&plain), row.id, suffix).await;
        common::assert_answer(&stranger, StatusCode::FORBIDDEN, "a non-staff token");
        common::assert_error(&stranger, NOT_STAFF, "a non-staff token");
        let read_only = write(&app, Some(&root_read), row.id, suffix).await;
        common::assert_answer(&read_only, StatusCode::FORBIDDEN, "a read grant on a write");
        common::assert_error(&read_only, NO_ADMIN_WRITE, "a read grant on a write");
        common::assert_answer(
            &write(&app, Some(&root_write), row.id, suffix).await,
            StatusCode::OK,
            &format!("the admin write grant on {suffix}"),
        );
        common::assert_answer(
            &write(&app, Some(&moddy_write), row.id, suffix).await,
            StatusCode::OK,
            &format!("the moderator write grant on {suffix}"),
        );
    }
}

/// The listing pages the OPEN queue — the unfiltered default is the queue
/// that still needs action — newest filing first, under a `Link` header
/// keyed on the report ids that repeats the request's own `limit`; the
/// cursor walk continues exactly where each page ended, and an empty
/// answer carries no `Link` at all.
#[tokio::test]
async fn the_listing_pages_the_open_queue_under_a_link_header() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "mar_page_root", "admin:read:reports").await;
    let alice = common::create_local_user(&pool, "mar_page_alice").await;
    let bob = common::create_local_user(&pool, "mar_page_bob").await;
    for nth in 0..3 {
        let _filed = file(&pool, &alice, &bob, &filing(&format!("report {nth}"))).await;
    }
    let acted = file(&pool, &bob, &alice, &filing("already resolved")).await;
    let _moved = report::set_state(&pool, acted.id, ReportState::Resolved, None)
        .await
        .expect("the fixture moves");
    let open: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM reports WHERE state = 'open' ORDER BY id DESC",
    )
    .fetch_all(&pool)
    .await
    .expect("the open ids read")
    .into_iter()
    .map(|(id,)| id)
    .collect();

    let first = TestRequest::get(&format!("{ADMIN}?limit=2"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&first, StatusCode::OK, "the first page");
    assert_eq!(
        common::ids_of(&first.json_value()),
        wanted_ids(&open[..2]),
        "the two newest open filings, the resolved one excluded"
    );
    let link = first.header("link").expect("a non-empty page links");
    assert!(
        link.contains(&format!("max_id={}", open[1]))
            && link.contains(&format!("min_id={}", open[0]))
            && link.contains("limit=2")
            && link.contains(ADMIN),
        "the cursors key on the report ids and repeat the window: {link}"
    );

    let second = TestRequest::get(&format!("{ADMIN}?limit=2&max_id={}", open[1]))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&second, StatusCode::OK, "the followed page");
    assert_eq!(
        common::ids_of(&second.json_value()),
        wanted_ids(&open[2..]),
        "the walk continues where the page ended"
    );

    let past = TestRequest::get(&format!("{ADMIN}?max_id={}", open[2]))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&past, StatusCode::OK, "the page past the end");
    assert_eq!(
        past.json_value(),
        json!([]),
        "walking off the end is the empty array"
    );
    assert_eq!(past.header("link"), None, "and no Link header at all");
}

/// The three filters, alone and combined: `resolved=true` selects the
/// acted-on pair (`resolved` *and* `closed`, the two states the entity's
/// `action_taken` answers true for), a junk `resolved` value is no
/// statement and keeps the open queue, `account_id` weighs the reporter
/// and `target_account_id` the reported, an id that names no account is
/// simply the empty page, and a filter that is no id is the 400.
#[tokio::test]
async fn the_listing_filters_select_their_own_rows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "mar_filt_root", "admin:read:reports").await;
    let alice = common::create_local_user(&pool, "mar_filt_alice").await;
    let bob = common::create_local_user(&pool, "mar_filt_bob").await;
    let carol = common::create_local_user(&pool, "mar_filt_carol").await;
    let open_by_alice = file(&pool, &alice, &bob, &filing("open by alice")).await;
    let open_by_bob = file(&pool, &bob, &carol, &filing("open by bob")).await;
    let resolved = file(&pool, &alice, &carol, &filing("resolved")).await;
    let _acted = report::set_state(&pool, resolved.id, ReportState::Resolved, None)
        .await
        .expect("the fixture resolves");
    let closed = file(&pool, &carol, &alice, &filing("closed")).await;
    let _moved = report::set_state(&pool, closed.id, ReportState::Closed, None)
        .await
        .expect("the fixture closes");
    let open = [open_by_bob.id, open_by_alice.id];
    let acted = [closed.id, resolved.id];

    let cases: &[(&str, &[Uuid])] = &[
        ("", &open),
        ("resolved=false", &open),
        ("resolved=maybe", &open),
        ("resolved=yes", &acted),
        ("resolved=true", &acted),
        ("resolved=1", &acted),
        (&format!("account_id={}", alice.id), &[open_by_alice.id]),
        (
            &format!("account_id={}&resolved=true", alice.id),
            &[resolved.id],
        ),
        (
            &format!("target_account_id={}", carol.id),
            &[open_by_bob.id],
        ),
        (
            &format!("account_id={}&target_account_id={}", alice.id, bob.id),
            &[open_by_alice.id],
        ),
        (&format!("target_account_id={}", Uuid::now_v7()), &[]),
    ];
    for (query, wanted) in cases {
        assert_eq!(
            common::ids_of(&list(&app, query, &token).await),
            wanted_ids(wanted),
            "the listing of `{query}`"
        );
    }

    for key in ["account_id", "target_account_id"] {
        let refused = get(&app, &format!("{ADMIN}?{key}=nope"), Some(&token)).await;
        common::assert_answer(
            &refused,
            StatusCode::BAD_REQUEST,
            &format!("the broken {key}"),
        );
        assert_eq!(refused.json_value()["error"], INVALID_ID, "its wording");
    }
}

/// Assigns and resolves `filed`'s report to `root` and reads the entity
/// back over HTTP: the shared answer both halves of the entity pinning
/// assert against.
async fn acted_entity(
    pool: &PgPool,
    app: &Router,
    token: &str,
    filed: &Filed,
    root: &User,
) -> Value {
    let _assigned = report::assign(pool, filed.row.id, Some(root.id))
        .await
        .expect("the fixture assigns");
    let _resolved = report::set_state(pool, filed.row.id, ReportState::Resolved, Some(root.id))
        .await
        .expect("the fixture resolves");
    read(app, &format!("{ADMIN}/{}", filed.row.id), token).await
}

/// The entity of an assigned, resolved report: every one of the ticket's
/// fourteen keys, the row's own scalars read through, and the four
/// admin-account slots in the Mastodon admin render — the account entity
/// with its role object and private columns, not the client profile.
#[tokio::test]
async fn the_entity_names_every_pinned_slot() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (root, token) = admin(&pool, "mar_shape_root", "admin:read:reports").await;
    let filed = filed(&pool).await;
    let answer = acted_entity(&pool, &app, &token, &filed, &root).await;
    let row = stored(&pool, filed.row.id).await;
    let stamp = row
        .action_taken_at
        .expect("the fixture resolution stamped the action");

    assert_eq!(
        keys(&answer),
        ENTITY_KEYS.iter().copied().collect::<BTreeSet<&str>>(),
        "exactly the fourteen pinned keys"
    );
    assert_eq!(answer["id"], row.id.to_string(), "the id as text");
    assert_eq!(answer["action_taken"], true, "resolved is an action");
    assert_eq!(
        answer["action_taken_at"],
        json!(mastodon_time(&stamp)),
        "the resolution stamp in the Mastodon format"
    );
    assert_eq!(answer["category"], "violation", "the row's own category");
    assert_eq!(
        answer["comment"], "this is harassment",
        "the comment verbatim"
    );
    assert_eq!(answer["forwarded"], true, "the row's own forward request");
    assert_eq!(
        answer["created_at"],
        mastodon_time(&row.inserted_at),
        "the filing instant"
    );
    assert_eq!(
        answer["updated_at"],
        mastodon_time(&row.updated_at),
        "the row's last change"
    );
    assert_eq!(
        answer["account"]["id"],
        filed.alice.id.to_string(),
        "account is the reporter"
    );
    assert_eq!(
        answer["target_account"]["id"],
        filed.bob.id.to_string(),
        "target_account is the reported"
    );
    assert_eq!(
        answer["assigned_account"]["id"],
        root.id.to_string(),
        "assigned_account is the owner"
    );
    assert_eq!(
        answer["action_taken_by_account"]["id"],
        root.id.to_string(),
        "action_taken_by_account is the acting moderator"
    );
    for slot in [
        "account",
        "target_account",
        "assigned_account",
        "action_taken_by_account",
    ] {
        assert!(
            answer[slot].get("role").is_some() && answer[slot].get("email").is_some(),
            "`{slot}` is the admin account render"
        );
    }
}

/// The entity's two arrays: the cited status rendered as a full
/// [`Status`] entity for the staff reader although the private post's
/// audience never included them, and the cited rules in the reporter's
/// citation order — not the priority table's — under their three keys.
///
/// [`Status`]: wired_be::entities::status::Status
#[tokio::test]
async fn the_entity_renders_the_cited_statuses_and_rules() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (root, token) = admin(&pool, "mar_arrays_root", "admin:read:reports").await;
    let filed = filed(&pool).await;
    let answer = acted_entity(&pool, &app, &token, &filed, &root).await;

    let statuses = answer["statuses"].as_array().expect("the statuses array");
    assert_eq!(statuses.len(), 1, "one cited status");
    assert_eq!(
        statuses[0]["id"],
        filed.cited.id.to_string(),
        "the cited status renders as a Status entity"
    );
    assert_eq!(
        statuses[0]["visibility"], "private",
        "its own visibility reads"
    );
    assert!(
        statuses[0]["content"]
            .as_str()
            .is_some_and(|text| text.contains("the reported post")),
        "the private post renders for staff although the reader has no leave on it: {:?}",
        statuses[0]["content"]
    );
    let rules = answer["rules"].as_array().expect("the rules array");
    assert_eq!(rules.len(), 2, "both cited rules");
    assert_eq!(
        keys(&rules[0]),
        BTreeSet::from(["hint", "id", "text"]),
        "the rule entry's three keys"
    );
    assert_eq!(
        rules[0]["id"],
        filed.first.id.to_string(),
        "in the reporter's citation order, not the priority table's"
    );
    assert_eq!(rules[0]["text"], "No harassment", "the rule text");
    assert_eq!(rules[0]["hint"], "keep it civil", "and its hint");
    assert_eq!(
        rules[1]["id"],
        filed.second.id.to_string(),
        "the second-cited rule follows, the table's earlier row"
    );
    assert_eq!(rules[1]["hint"], "", "the hintless rule reads empty");
}

/// An untouched filing answers the same fourteen keys with the empties
/// where moderation has not been: both moderator slots `null` (never
/// omitted), no action, no stamp, empty arrays.
#[tokio::test]
async fn an_unmoderated_report_nulls_the_action_slots() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (root, token) = admin(&pool, "mar_bare_root", "admin:read:reports").await;
    let alice = common::create_local_user(&pool, "mar_bare_alice").await;
    let row = file(&pool, &root, &alice, &filing("nothing cited")).await;

    let answer = read(&app, &format!("{ADMIN}/{}", row.id), &token).await;
    assert_eq!(
        keys(&answer),
        ENTITY_KEYS.iter().copied().collect::<BTreeSet<&str>>(),
        "the unmoderated entity carries all fourteen keys too"
    );
    assert_eq!(answer["action_taken"], false, "open is no action");
    assert_eq!(answer["action_taken_at"], json!(null), "null, not absent");
    assert_eq!(
        answer["assigned_account"],
        json!(null),
        "the same for the assignee"
    );
    assert_eq!(
        answer["action_taken_by_account"],
        json!(null),
        "and for the actor that has not acted"
    );
    assert_eq!(answer["category"], "other", "the default category");
    assert_eq!(answer["forwarded"], false, "and the default answer");
    assert_eq!(answer["statuses"], json!([]), "nothing cited");
    assert_eq!(answer["rules"], json!([]), "no rules cited");
}

/// `resolve` moves the state, stamps the action, names the acting
/// moderator in the entity and the row, and logs the `report_update`
/// entry — and, run a second time by a different moderator, keeps the
/// first stamp and the first actor.
#[tokio::test]
async fn resolving_stamps_the_action_and_names_the_actor() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (moddy, moddy_token) = moderator(&pool, "mar_res_moddy", "admin:write:reports").await;
    let (_, root_token) = admin(&pool, "mar_res_root", "admin:write:reports").await;
    let alice = common::create_local_user(&pool, "mar_res_alice").await;
    let bob = common::create_local_user(&pool, "mar_res_bob").await;
    let row = file(&pool, &alice, &bob, &filing("harassment")).await;

    let answer = write_entity(&app, &moddy_token, row.id, "resolve").await;
    assert_eq!(answer["id"], row.id.to_string(), "the resolved report");
    assert_eq!(answer["action_taken"], true, "resolved is an action");
    assert_ne!(
        answer["action_taken_at"],
        json!(null),
        "and the write stamped the moment"
    );
    assert_eq!(
        answer["action_taken_by_account"]["id"],
        moddy.id.to_string(),
        "the entity names the acting moderator"
    );
    let stored_row = stored(&pool, row.id).await;
    assert_eq!(stored_row.state, ReportState::Resolved, "the row moved");
    assert_eq!(
        stored_row.action_taken_by,
        Some(moddy.id),
        "and the row names the actor"
    );
    assert_eq!(
        stored_row.assigned_to, None,
        "resolving is not an assignment"
    );
    let lines = log_lines(&pool, "report_update").await;
    assert_eq!(lines.len(), 1, "one entry for the move");
    assert_eq!(lines[0].0, "mar_res_moddy", "the actor is the writer");
    assert!(
        lines[0].1.contains(&format!("report ##{}", row.id))
            && lines[0].1.contains("( on user @mar_res_bob)")
            && lines[0].1.contains("with 'resolved' state"),
        "Pleroma's sentence: {}",
        lines[0].1
    );

    let again = write_entity(&app, &root_token, row.id, "resolve").await;
    assert_eq!(
        again["action_taken_at"], answer["action_taken_at"],
        "resolving twice keeps the first stamp"
    );
    assert_eq!(
        again["action_taken_by_account"]["id"],
        moddy.id.to_string(),
        "and the first actor with it"
    );
    assert_eq!(
        log_lines(&pool, "report_update").await.len(),
        2,
        "and logs twice"
    );
}

/// `reopen` moves the state back and clears both action fields — the
/// entity answers `action_taken` false with both slots null, the
/// assignment untouched — beside its own `report_update` entry.
#[tokio::test]
async fn reopening_clears_the_action_fields() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (root, token) = admin(&pool, "mar_re_root", "admin:write:reports").await;
    let alice = common::create_local_user(&pool, "mar_re_alice").await;
    let bob = common::create_local_user(&pool, "mar_re_bob").await;
    let row = file(&pool, &alice, &bob, &filing("reopened")).await;
    let resolved = write_entity(&app, &token, row.id, "resolve").await;
    let _assigned = report::assign(&pool, row.id, Some(root.id))
        .await
        .expect("the fixture assigns");

    let answer = write_entity(&app, &token, row.id, "reopen").await;
    assert_eq!(
        answer["id"], resolved["id"],
        "the same report the resolve answered"
    );
    assert_eq!(answer["action_taken"], false, "open is no action again");
    assert_eq!(
        answer["action_taken_at"],
        json!(null),
        "the stamp is cleared"
    );
    assert_eq!(
        answer["action_taken_by_account"],
        json!(null),
        "and the actor with it"
    );
    assert_eq!(
        answer["assigned_account"]["id"],
        root.id.to_string(),
        "the assignment is not an action field and stays"
    );
    let stored_row = stored(&pool, row.id).await;
    assert_eq!(stored_row.state, ReportState::Open, "the row moved back");
    assert_eq!(stored_row.action_taken_at, None, "its stamp went");
    assert_eq!(stored_row.action_taken_by, None, "and its actor with it");
    let lines = log_lines(&pool, "report_update").await;
    assert_eq!(lines.len(), 2, "one entry per move");
    assert!(
        lines[1].1.contains("with 'open' state"),
        "the second entry names the reopening: {}",
        lines[1].1
    );
}

/// `assign_to_self` makes the reading staff member the report's owner
/// without touching the state and logs the `report_assigned` entry naming
/// them; `unassign` hands the report back to the queue with its own entry.
#[tokio::test]
async fn assigning_to_self_and_unassigning_move_only_the_assignment() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (moddy, token) = moderator(&pool, "mar_as_moddy", "admin:write:reports").await;
    let alice = common::create_local_user(&pool, "mar_as_alice").await;
    let bob = common::create_local_user(&pool, "mar_as_bob").await;
    let row = file(&pool, &alice, &bob, &filing("assigned")).await;
    let _resolved = report::set_state(&pool, row.id, ReportState::Resolved, None)
        .await
        .expect("the fixture resolves");

    let answer = write_entity(&app, &token, row.id, "assign_to_self").await;
    assert_eq!(
        answer["assigned_account"]["id"],
        moddy.id.to_string(),
        "the writer owns the report now"
    );
    assert_eq!(
        answer["action_taken"], true,
        "the resolved one stays resolved"
    );
    assert_eq!(
        stored(&pool, row.id).await.state,
        ReportState::Resolved,
        "the state did not move"
    );
    assert_eq!(
        stored(&pool, row.id).await.assigned_to,
        Some(moddy.id),
        "the row says so"
    );
    let lines = log_lines(&pool, "report_assigned").await;
    assert_eq!(lines.len(), 1, "one entry for the assignment");
    assert_eq!(lines[0].0, "mar_as_moddy", "the actor is the writer");
    assert!(
        lines[0]
            .1
            .contains(&format!("assigned report ##{}", row.id))
            && lines[0].1.contains("( on user @mar_as_bob)")
            && lines[0].1.contains("to user mar_as_moddy"),
        "Pleroma's sentence: {}",
        lines[0].1
    );

    let back = write_entity(&app, &token, row.id, "unassign").await;
    assert_eq!(
        back["assigned_account"],
        json!(null),
        "the queue holds it again"
    );
    assert_eq!(
        stored(&pool, row.id).await.assigned_to,
        None,
        "on the row too"
    );
    assert_eq!(
        stored(&pool, row.id).await.state,
        ReportState::Resolved,
        "and the state moved neither way"
    );
    let unassigned = log_lines(&pool, "report_unassigned").await;
    assert_eq!(unassigned.len(), 1, "the release has its own entry");
    assert!(
        unassigned[0]
            .1
            .contains(&format!("unassigned report ##{}", row.id))
            && unassigned[0].1.contains("( on user @mar_as_bob)")
            && unassigned[0].1.contains("from a user"),
        "Pleroma's sentence: {}",
        unassigned[0].1
    );
}

/// Text that is no id and an id that names no report are the one 404 the
/// ticket pins — on the show route and on all four writes — and none of
/// the misses writes a row.
#[tokio::test]
async fn every_miss_is_the_one_record_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (root, read_token) = admin(&pool, "mar_404_root", "admin:read:reports").await;
    let write_token = common::create_app_and_token(&pool, &root, "admin:write:reports").await;
    let unknown = Uuid::now_v7();

    let shown = get(&app, &format!("{ADMIN}/{unknown}"), Some(&read_token)).await;
    common::assert_answer(&shown, StatusCode::NOT_FOUND, "the show of an unknown id");
    assert_eq!(shown.json_value()["error"], NOT_FOUND, "the pinned body");

    let junk = get(&app, &format!("{ADMIN}/not-a-uuid"), Some(&read_token)).await;
    common::assert_answer(&junk, StatusCode::NOT_FOUND, "the show of no id");
    assert_eq!(junk.json_value()["error"], NOT_FOUND, "the same one answer");

    for suffix in ["resolve", "reopen", "assign_to_self", "unassign"] {
        let missing = write(&app, Some(&write_token), unknown, suffix).await;
        common::assert_answer(
            &missing,
            StatusCode::NOT_FOUND,
            &format!("the write on {suffix}"),
        );
        assert_eq!(
            missing.json_value()["error"],
            NOT_FOUND,
            "`{suffix}` misses with the same body"
        );
        let unparsable = TestRequest::post(&format!("{ADMIN}/not-a-uuid/{suffix}"))
            .bearer(&write_token)
            .send(&app)
            .await;
        common::assert_answer(
            &unparsable,
            StatusCode::NOT_FOUND,
            &format!("the write on no id ({suffix})"),
        );
        assert_eq!(
            unparsable.json_value()["error"],
            NOT_FOUND,
            "and it is the same miss"
        );
    }
    let rows = sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM reports")
        .fetch_one(&pool)
        .await
        .expect("the count reads");
    assert_eq!(rows.0, 0, "the misses wrote nothing");
}
