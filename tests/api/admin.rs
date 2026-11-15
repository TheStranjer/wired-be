//! The admin foundation over HTTP (`/api/v1/pleroma/admin`, WBE-194): the
//! [`AdminUser`] and [`StaffUser`] extractors, the moderation-log listing
//! and its filters and pagination, and the visibility `stats`, all against
//! the real router (plus a two-route test router that exercises the
//! [`StaffUser`] gate, which guards no production route yet).
//!
//! The extractor matrix follows Pleroma's `UserIsAdminPlug` /
//! `UserIsStaffPlug` order — a missing token is 401, an unusable one
//! 401/403, a valid non-staff token 403 naming the role, and only then the
//! `admin:read` scope gate. The log entries are seeded through the model
//! helpers (the write endpoints arrive with the later admin tickets), and
//! time is made controllable by back-dating `inserted_at` rather than
//! sleeping.
//!
//! [`AdminUser`]: wired_be::auth::admin::AdminUser
//! [`StaffUser`]: wired_be::auth::admin::StaffUser

#![cfg(test)]

use axum::{Json, Router, http::StatusCode, routing::get};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    auth::admin::{AdminUser, StaffUser},
    error::AppError,
    models::{
        moderation_log,
        status::{self, NewStatus, Visibility},
        user::{Flag, NewUser, User},
    },
    state::AppState,
};

use crate::common::{self, TestRequest};

/// The moderation-log route.
const LOG: &str = "/api/v1/pleroma/admin/moderation_log";
/// The stats route.
const STATS: &str = "/api/v1/pleroma/admin/stats";
/// The 401 body when no token is presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body when a presented token does not resolve.
const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body for an app-only (user-unbound) token.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body for a valid token whose user is not an admin.
const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body for a valid token whose user is not staff.
const NOT_STAFF: &str = "User is not a staff member.";
/// The 403 body of a missing `admin:read` grant on these read routes.
const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The window-start margin of the date-filter test, hours before the
/// two-day-old entry.
const WINDOW_HOURS_BACK_START: i64 = 60;
/// The window-end margin, safely between the one- and two-day-old rows.
const WINDOW_HOURS_BACK_END: i64 = 25;

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

/// A two-route test router guarding each extractor in turn: `AdminUser`
/// then its `admin:read` gate on `/admin`, and `StaffUser` then the same
/// gate on `/staff`. `StaffUser` guards no production route yet, so this is
/// the matrix harness, mirroring `auth.rs`.
fn extractor_app(pool: PgPool) -> Router {
    Router::new()
        .route("/admin", get(admin_view))
        .route("/staff", get(staff_view))
        .with_state(app_state(pool))
}

/// The shared [`AppState`] every test router runs on.
fn app_state(pool: PgPool) -> AppState {
    common::test_app_state(pool, common::test_config())
}

/// Reports the resolved admin and the token's scopes.
async fn admin_view(admin: AdminUser) -> Result<Json<Value>, AppError> {
    admin.require("read", "accounts")?;
    Ok(Json(json!({
        "nickname": admin.0.nickname,
        "is_admin": admin.0.is_admin.get(),
        "scopes": admin.1.scopes,
    })))
}

/// Reports the resolved staff user and the token's scopes.
async fn staff_view(staff: StaffUser) -> Result<Json<Value>, AppError> {
    staff.require("read", "accounts")?;
    Ok(Json(json!({
        "nickname": staff.0.nickname,
        "is_admin": staff.0.is_admin.get(),
        "is_moderator": staff.0.is_moderator.get(),
        "scopes": staff.1.scopes,
    })))
}

/// The extractor matrix on `/admin`: an admin passes, a moderator and a
/// regular user are refused as not-admin, an app-only token and no token
/// keep the shared rejections.
#[tokio::test]
async fn admin_extractor_matrix_over_http() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = extractor_app(pool.clone());
    let (_, admin_read) = admin(&pool, "root", "admin:read").await;
    let (_, mod_read) = moderator(&pool, "halfroot", "admin:read").await;
    let (_, plain) = regular(&pool, "user", "admin:read").await;
    let app_only = common::create_app_only_token(&pool, "admin:read").await;

    let granted = TestRequest::get("/admin")
        .bearer(&admin_read)
        .send(&app)
        .await;
    common::assert_answer(&granted, StatusCode::OK, "an admin reaches /admin");
    assert_eq!(
        granted.json_value()["nickname"],
        "root",
        "identity resolves"
    );

    for (token, who) in [(&mod_read, "a moderator"), (&plain, "a regular user")] {
        let refused = TestRequest::get("/admin").bearer(token).send(&app).await;
        common::assert_all_refused(&[refused], StatusCode::FORBIDDEN, NOT_ADMIN, who);
    }

    let app_only_refused = TestRequest::get("/admin")
        .bearer(&app_only)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[app_only_refused],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "an app-only token",
    );

    let anonymous = TestRequest::get("/admin").send(&app).await;
    let bogus = TestRequest::get("/admin")
        .bearer("no-such-token")
        .send(&app)
        .await;
    common::assert_all_refused(
        &[anonymous],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "a request with no token",
    );
    common::assert_all_refused(
        &[bogus],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "an unknown token",
    );
}

/// The scope half of the gate on `/admin`: a valid admin whose token
/// grants no `admin:read` is refused with the `admin:read` wording, and
/// the narrow `admin:read:accounts` alias passes the area it names.
#[tokio::test]
async fn admin_scope_gate_over_http() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = extractor_app(pool.clone());
    let (_, no_scope) = admin(&pool, "noscope", "read write").await;
    let (_, narrow) = admin(&pool, "narrow", "admin:read:accounts").await;

    let refused = TestRequest::get("/admin")
        .bearer(&no_scope)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[refused],
        StatusCode::FORBIDDEN,
        NO_ADMIN_READ,
        "an admin with no admin scope",
    );

    let granted = TestRequest::get("/admin").bearer(&narrow).send(&app).await;
    common::assert_answer(&granted, StatusCode::OK, "the narrow area scope passes");
}

/// The [`StaffUser`] matrix on `/staff`: admin and moderator both pass, a
/// regular user is refused as not staff, and the token-less rejections
/// are the shared ones.
#[tokio::test]
async fn staff_extractor_matrix_over_http() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = extractor_app(pool.clone());
    let (_, admin_read) = admin(&pool, "boss", "admin:read").await;
    let (_, mod_read) = moderator(&pool, "deputy", "admin:read").await;
    let (_, plain) = regular(&pool, "peon", "admin:read").await;
    let app_only = common::create_app_only_token(&pool, "admin:read").await;

    for (token, who, is_mod) in [
        (&admin_read, "an admin", false),
        (&mod_read, "a moderator", true),
    ] {
        let granted = TestRequest::get("/staff").bearer(token).send(&app).await;
        common::assert_answer(&granted, StatusCode::OK, &format!("{who} reaches /staff"));
        assert_eq!(
            granted.json_value()["is_moderator"],
            Value::Bool(is_mod),
            "{who} resolves with the right role flags"
        );
    }

    let refused = TestRequest::get("/staff").bearer(&plain).send(&app).await;
    common::assert_all_refused(
        &[refused],
        StatusCode::FORBIDDEN,
        NOT_STAFF,
        "a regular user",
    );

    let app_only_refused = TestRequest::get("/staff")
        .bearer(&app_only)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[app_only_refused],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "an app-only token",
    );

    let anonymous = TestRequest::get("/staff").send(&app).await;
    common::assert_all_refused(
        &[anonymous],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "a request with no token",
    );
}

/// The log listing and stats routes both demand an admin: a moderator is
/// refused the role, an admin without `admin:read` the scope, and nobody
/// without a token at all.
#[tokio::test]
async fn production_routes_demand_an_admin_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, mod_read) = moderator(&pool, "mod-no-admin", "admin:read").await;
    let (_, admin_no_scope) = admin(&pool, "admin-no-scope", "read").await;

    for route in [LOG, STATS] {
        let not_admin = TestRequest::get(route).bearer(&mod_read).send(&app).await;
        common::assert_all_refused(
            &[not_admin],
            StatusCode::FORBIDDEN,
            NOT_ADMIN,
            "a moderator",
        );

        let no_scope = TestRequest::get(route)
            .bearer(&admin_no_scope)
            .send(&app)
            .await;
        common::assert_all_refused(
            &[no_scope],
            StatusCode::FORBIDDEN,
            NO_ADMIN_READ,
            "an admin without admin:read",
        );

        let anonymous = TestRequest::get(route).send(&app).await;
        common::assert_all_refused(
            &[anonymous],
            StatusCode::UNAUTHORIZED,
            NO_TOKEN,
            "an anonymous request",
        );
    }
}

/// Moves log entry `id`'s `inserted_at` `days` into the past, so the
/// listing tests command the clock.
async fn age_entry(pool: &PgPool, id: Uuid, days: i32) {
    let _moved = sqlx::query(
        "UPDATE moderation_log SET inserted_at = now() - make_interval(days => $2) WHERE id = $1",
    )
    .bind(id)
    .bind(days)
    .execute(pool)
    .await
    .expect("the backdate runs");
}

/// The three seeded entries' ids and acting admins: `deactivate` by
/// "one" three days ago, `activate` by "two" two days ago, `delete` by
/// "one" one day ago.
#[derive(Debug)]
struct SeededLog {
    /// The admin of the two-day-old entry.
    actor_two: User,
    /// The oldest entry, three days back.
    oldest: Uuid,
    /// The middle entry, two days back.
    middle: Uuid,
    /// The newest entry, one day back.
    newest: Uuid,
}

/// Seeds the three-entry log through the model helpers, back-dated for a
/// deterministic newest-first order.
async fn seed_log(pool: &PgPool) -> SeededLog {
    let (one, _) = admin(pool, "one", "admin:read").await;
    let (two, _) = admin(pool, "two", "admin:read").await;
    let subject = common::create_local_user(pool, "subject").await;
    let oldest = moderation_log::deactivate(pool, &one, std::slice::from_ref(&subject))
        .await
        .expect("records");
    let middle = moderation_log::activate(pool, &two, std::slice::from_ref(&subject))
        .await
        .expect("records");
    let newest = moderation_log::delete(pool, &one, std::slice::from_ref(&subject))
        .await
        .expect("records");
    age_entry(pool, oldest.id, 3).await;
    age_entry(pool, middle.id, 2).await;
    age_entry(pool, newest.id, 1).await;
    SeededLog {
        actor_two: two,
        oldest: oldest.id,
        middle: middle.id,
        newest: newest.id,
    }
}

/// The listing shape: `{items, total}`, newest first, each item the
/// Pleroma `{id, data, time, message}` entry with the actor map inside
/// `data`.
#[tokio::test]
async fn moderation_log_lists_newest_first_in_pleroma_shape() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let seeded = seed_log(&pool).await;
    let (_, token) = admin(&pool, "viewer", "admin:read").await;

    let response = TestRequest::get(LOG).bearer(&token).send(&app).await;
    common::assert_answer(&response, StatusCode::OK, "the full log reads");
    let body = response.json_value();
    assert_eq!(body["total"], 3, "three entries");
    assert_eq!(
        body["items"]
            .as_array()
            .expect("items array")
            .iter()
            .map(|item| item["id"].as_str().expect("id text"))
            .collect::<Vec<&str>>(),
        vec![
            seeded.newest.to_string().as_str(),
            seeded.middle.to_string().as_str(),
            seeded.oldest.to_string().as_str()
        ],
        "newest first"
    );

    let first = &body["items"].as_array().expect("items")[0];
    assert_eq!(
        first["message"], "@one deleted users: @subject",
        "the stored sentence, top level and in data"
    );
    assert_eq!(
        first["data"]["message"], first["message"],
        "one message twice"
    );
    assert_eq!(
        first["data"]["action"], "delete",
        "the action in the data map"
    );
    assert_eq!(first["data"]["actor"]["nickname"], "one", "the actor map");
    assert_eq!(
        first["data"]["actor"]["type"], "user",
        "Pleroma stamps the actor type"
    );
    assert!(
        first["data"]["actor"]["id"].as_str().is_some(),
        "the actor id rides as a JSON string: {}",
        first["data"]["actor"]["id"]
    );
    assert!(
        first["time"].as_i64().expect("unix seconds") > 0,
        "time is unix seconds"
    );
}

/// The listing filters: `user_id` selects one actor's entries, `search`
/// matches the message case-insensitively.
#[tokio::test]
async fn moderation_log_filters_by_actor_and_message() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let seeded = seed_log(&pool).await;
    let (_, token) = admin(&pool, "filterer", "admin:read").await;

    let by_actor = TestRequest::get(&format!("{LOG}?user_id={}", seeded.actor_two.id))
        .bearer(&token)
        .send(&app)
        .await;
    let body = by_actor.json_value();
    assert_eq!(body["total"], 1, "actor two has one entry");
    assert_eq!(
        body["items"][0]["id"],
        seeded.middle.to_string(),
        "and it is his activate"
    );

    let searched = TestRequest::get(&format!("{LOG}?search=DEACTIV"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        searched.json_value()["items"][0]["id"],
        seeded.oldest.to_string(),
        "the search is a case-insensitive substring of the message"
    );

    let no_match = TestRequest::get(&format!("{LOG}?search=relay"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        no_match.json_value()["total"],
        0,
        "a word nothing says matches nothing"
    );
    assert_eq!(
        no_match.json_value()["items"],
        Value::Array(Vec::new()),
        "and the empty page answers an empty items array"
    );
}

/// The page window and the time window: `page`/`page_size` slice the
/// newest-first order with the total ignoring the window, `start_date` and
/// `end_date` bound it inclusively, and `page_size` above the bound is
/// capped rather than refused.
#[tokio::test]
async fn moderation_log_pages_and_windows_by_time() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let seeded = seed_log(&pool).await;
    let (_, token) = admin(&pool, "pager", "admin:read").await;

    let paged = TestRequest::get(&format!("{LOG}?page=2&page_size=2"))
        .bearer(&token)
        .send(&app)
        .await;
    let body = paged.json_value();
    assert_eq!(body["total"], 3, "the total ignores the window");
    assert_eq!(
        body["items"].as_array().expect("items").len(),
        1,
        "one row on page two"
    );
    assert_eq!(
        body["items"][0]["id"],
        seeded.oldest.to_string(),
        "the oldest"
    );

    let window = TestRequest::get(&format!(
        "{LOG}?start_date={}&end_date={}",
        hours_ago(WINDOW_HOURS_BACK_START),
        hours_ago(WINDOW_HOURS_BACK_END)
    ))
    .bearer(&token)
    .send(&app)
    .await;
    let w = window.json_value();
    assert_eq!(
        w["total"], 1,
        "only the two-day-old entry stands in the window"
    );
    assert_eq!(
        w["items"][0]["id"],
        seeded.middle.to_string(),
        "and it is named"
    );

    let capped = TestRequest::get(&format!("{LOG}?page_size=5000"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        capped.json_value()["items"]
            .as_array()
            .expect("items")
            .len(),
        3,
        "an oversized page comes back capped, not refused"
    );

    let junk_page = TestRequest::get(&format!("{LOG}?page=abc&page_size=0"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        junk_page.json_value()["total"],
        3,
        "unusable numbers fall to the defaults"
    );
}

/// A full ISO `Z`-suffixed timestamp `hours` before now, for the inclusive
/// window. `Z` (not `+00:00`) so the query string carries no `+` that
/// would decode to a space.
fn hours_ago(hours: i64) -> String {
    chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::hours(hours))
        .expect("the bound is representable")
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Bad filter values are refused: an unparsable `user_id` is the 400
/// `Invalid id`, a broken `start_date` is a 422 naming the parameter.
#[tokio::test]
async fn moderation_log_validation_errors() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "checker", "admin:read").await;

    let bad_id = TestRequest::get(&format!("{LOG}?user_id=not-a-uuid"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[bad_id],
        StatusCode::BAD_REQUEST,
        "Invalid id",
        "an unparsable user_id",
    );

    let bad_date = TestRequest::get(&format!("{LOG}?start_date=31/01/2026"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        bad_date.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a broken start_date is a 422: {}",
        bad_date.text()
    );
    common::assert_error(
        &bad_date,
        "start_date must be an ISO 8601 date or timestamp, not 31/01/2026",
        "a broken start_date",
    );
}

/// The `stats` counters: local live non-reblog statuses by visibility with
/// no `instance`, one domain's own with it, and an unknown domain all
/// zeros.
#[tokio::test]
async fn stats_counts_by_visibility() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;

    let _public = common::create_status(&pool, &alice, "hi", Visibility::Public).await;
    let _unlisted = common::create_status(&pool, &alice, "hi", Visibility::Unlisted).await;
    let _private = common::create_status(&pool, &alice, "hi", Visibility::Private).await;
    let _direct = common::create_status(&pool, &alice, "hi", Visibility::Direct).await;
    let _remote = common::create_status(&pool, &mallory, "hi", Visibility::Public).await;
    seed_excluded(&pool, &alice).await;

    let (_, token) = admin(&pool, "counter", "admin:read").await;

    let local = TestRequest::get(STATS).bearer(&token).send(&app).await;
    common::assert_answer(&local, StatusCode::OK, "the local stats read");
    assert_eq!(
        local.json_value()["status_visibility"],
        json!({"public": 2, "unlisted": 1, "private": 1, "direct": 1}),
        "alice's live posts, the boost and the tombstone excluded"
    );

    let by_instance = TestRequest::get(&format!("{STATS}?instance=remote.example"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        by_instance.json_value()["status_visibility"],
        json!({"public": 1, "unlisted": 0, "private": 0, "direct": 0}),
        "mallory's domain alone"
    );

    let unknown = TestRequest::get(&format!("{STATS}?instance=nowhere.example"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        unknown.json_value()["status_visibility"],
        json!({"public": 0, "unlisted": 0, "private": 0, "direct": 0}),
        "a domain no account came from is all zeros"
    );
}

/// Two statuses the count must never see: a reblog row of alice's and a
/// soft-deleted one.
async fn seed_excluded(pool: &PgPool, alice: &User) {
    let original = common::create_status(pool, alice, "to boost", Visibility::Public).await;
    let _reblog = status::insert(
        pool,
        &common::test_config().base_url,
        &NewStatus {
            reblog_of_id: Some(original.id),
            ..NewStatus::local(alice, "")
        },
    )
    .await
    .expect("a reblog inserts");
    let doomed = common::create_status(pool, alice, "doomed", Visibility::Public).await;
    let _tombstone = status::soft_delete(pool, doomed.id)
        .await
        .expect("the soft delete runs");
}
