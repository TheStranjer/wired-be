//! The admin foundation over HTTP (`/api/v1/pleroma/admin`, WBE-194): the
//! [`AdminUser`] and [`StaffUser`] extractors, the moderation-log listing
//! and its filters and pagination, and the visibility `stats`, all against
//! the real router (plus a two-route test router that exercises the
//! [`StaffUser`] gate, which guards no production route yet). The admin
//! config store (WBE-206) — `GET`/`POST /config` over the whitelist
//! registry, the `:mrf`/`:instance` consumers and the `config_change`
//! log line — joins at the bottom of this file.
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

/// The log listing, stats and config-store routes all demand an admin: a
/// moderator is refused the role, an admin without `admin:read` the
/// scope, and nobody without a token at all.
#[tokio::test]
async fn production_routes_demand_an_admin_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, mod_read) = moderator(&pool, "mod-no-admin", "admin:read").await;
    let (_, admin_no_scope) = admin(&pool, "admin-no-scope", "read").await;

    for route in [LOG, STATS, CONFIG] {
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

// ---------------------------------------------------------------------------
// The admin config store (WBE-206): `GET`/`POST /config` over the
// whitelist registry — set/merge/delete round-trips, the wire's tuple
// form, the `:mrf*` chain the MRF loader reads, the `:instance` overrides
// the instance documents show, the `need_reboot` flag and the
// `config_change` log line.
// ---------------------------------------------------------------------------

/// The config store route.
const CONFIG: &str = "/api/v1/pleroma/admin/config";
/// The 403 body of a missing `admin:write` grant on the POST route.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";

/// The `{"tuple": [key, value]}` array the API exchanges keyword lists
/// as (Pleroma's `ConfigDB.to_json_types` shape).
fn wire(pairs: &[(&str, Value)]) -> Vec<Value> {
    pairs
        .iter()
        .map(|(key, value)| json!({"tuple": [key, value]}))
        .collect()
}

/// The stored value `value` carries under wire-form `key`, `None` when no
/// such tuple entry stands in the array.
fn tuple_value<'v>(value: &'v Value, key: &str) -> Option<&'v Value> {
    value.as_array()?.iter().find_map(|entry| {
        let pair = entry.get("tuple").and_then(Value::as_array)?;
        if pair.first() == Some(&json!(key)) {
            pair.get(1)
        } else {
            None
        }
    })
}

/// `POST /config` with one `configs` batch.
async fn post_configs(app: &Router, token: &str, items: Value) -> common::TestResponse {
    TestRequest::post(CONFIG)
        .bearer(token)
        .json(&json!({ "configs": items }))
        .send(app)
        .await
}

/// An admin holding both config scopes: the round-trip tests GET what
/// they POST.
async fn config_admin(pool: &PgPool, nickname: &str) -> String {
    admin(pool, nickname, "admin:read:config admin:write:config")
        .await
        .1
}

/// A set renders the wire tuple form, names its `db` subkeys, and the
/// effective GET merges the row over the env defaults — every whitelisted
/// key answers, and `need_reboot` stays down for the live keys.
#[tokio::test]
async fn config_set_renders_the_wire_shape_and_the_effective_view() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configroot").await;

    let first = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("name", json!("Renamed")), ("email", json!("staff@example.test"))]),
        }]),
    )
    .await;
    common::assert_answer(&first, StatusCode::OK, "the first write");
    let body = first.json_value();
    assert_eq!(body["configs"][0]["group"], ":pleroma", "the group");
    assert_eq!(body["configs"][0]["key"], ":instance", "the key");
    assert_eq!(
        body["configs"][0]["value"],
        json!(wire(&[
            ("email", json!("staff@example.test")),
            ("name", json!("Renamed"))
        ])),
        "the stored row renders as its tuple array"
    );
    assert_eq!(
        body["configs"][0]["db"],
        json!(["email", "name"]),
        "db names the stored subkeys"
    );
    assert_eq!(body["need_reboot"], json!(false), ":instance changes live");

    let merged = TestRequest::get(CONFIG).bearer(&token).send(&app).await;
    common::assert_answer(&merged, StatusCode::OK, "the effective GET");
    let view = merged.json_value();
    assert_eq!(
        view["configs"].as_array().map(Vec::len),
        Some(8),
        "every whitelisted key answers without only_db"
    );
    let instance = &view["configs"][0];
    assert_eq!(
        tuple_value(&instance["value"], "name"),
        Some(&json!("Renamed")),
        "the row's name is the effective one"
    );
    assert_eq!(
        tuple_value(&instance["value"], "description"),
        Some(&json!("A Wired instance, compatible with Pleroma clients")),
        "the untouched sibling keeps the env default"
    );
    assert_eq!(instance["db"], json!(["email", "name"]), "and db stays");
    assert_eq!(
        view["configs"][1].get("db"),
        None,
        "an entry with no row renders no db"
    );
}

/// A second set merges into the stored row: the new subkey writes
/// through, the first batch's subkey survives, and `only_db` answers the
/// stored rows alone.
#[tokio::test]
async fn config_second_write_merges_into_the_stored_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configmerge").await;

    let opening = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("name", json!("Renamed")), ("email", json!("staff@example.test"))]),
        }]),
    )
    .await;
    common::assert_answer(&opening, StatusCode::OK, "the first write");

    let second = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("name", json!("Again"))]),
        }]),
    )
    .await;
    let body = second.json_value();
    assert_eq!(
        tuple_value(&body["configs"][0]["value"], "name"),
        Some(&json!("Again")),
        "the re-set writes through"
    );
    assert_eq!(
        tuple_value(&body["configs"][0]["value"], "email"),
        Some(&json!("staff@example.test")),
        "and the earlier subkey survives the merge"
    );
    assert_eq!(
        body["configs"][0]["db"],
        json!(["email", "name"]),
        "both subkeys are stored now"
    );

    let only_db = TestRequest::get(&format!("{CONFIG}?only_db=true"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&only_db, StatusCode::OK, "the only_db GET");
    let rows = only_db.json_value();
    assert_eq!(
        rows["configs"].as_array().map(Vec::len),
        Some(1),
        "only_db answers the stored rows alone"
    );
    assert_eq!(rows["configs"][0]["key"], ":instance", "the one row");
}

/// The deletes: the subkey delete leaves the rest of the row standing,
/// the whole delete takes the row out of the store, and the effective GET
/// falls back to the env values once nothing stands.
#[tokio::test]
async fn config_delete_takes_subkeys_then_the_whole_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configdel").await;

    let opening = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("name", json!("Renamed")), ("email", json!("staff@example.test"))]),
        }]),
    )
    .await;
    common::assert_answer(&opening, StatusCode::OK, "the seeded write");

    let subkey_delete = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "delete": true,
            "subkeys": ["name"],
        }]),
    )
    .await;
    let left = subkey_delete.json_value();
    assert_eq!(
        left["configs"][0]["value"],
        json!(wire(&[("email", json!("staff@example.test"))])),
        "the deleted subkey is out and the row keeps the rest"
    );
    assert_eq!(left["configs"][0]["db"], json!(["email"]), "db shrank too");

    let whole_delete = post_configs(
        &app,
        &token,
        json!([{ "group": ":pleroma", "key": ":instance", "delete": true }]),
    )
    .await;
    let gone = whole_delete.json_value();
    assert_eq!(
        gone["configs"],
        json!([]),
        "a whole delete leaves nothing to render"
    );
    let only_db = TestRequest::get(&format!("{CONFIG}?only_db=true"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        only_db.json_value()["configs"],
        json!([]),
        "and the row is gone from the store"
    );
    let effective = TestRequest::get(CONFIG).bearer(&token).send(&app).await;
    let fallback = &effective.json_value()["configs"][0];
    assert_eq!(
        tuple_value(&fallback["value"], "name"),
        Some(&json!("Wired")),
        "the effective name fell back to the env default"
    );
    assert_eq!(fallback.get("db"), None, "with no row there is no db");
}

/// The whitelist is the write gate: an unknown key or group is the 400
/// naming the pair, the whole batch is refused before any of it writes,
/// and an upsert without a value is refused too.
#[tokio::test]
async fn config_whitelist_refuses_unknown_pairs() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configgate").await;

    let mixed = post_configs(
        &app,
        &token,
        json!([
            { "group": ":pleroma", "key": ":instance", "value": wire(&[("name", json!("Sneaky"))]) },
            { "group": ":pleroma", "key": ":database_config", "value": json!([]) },
        ]),
    )
    .await;
    common::assert_answer(&mixed, StatusCode::BAD_REQUEST, "the mixed batch");
    common::assert_error(
        &mixed,
        "Config not allowed: :pleroma/:database_config",
        "an unknown key in a batch that leads with a valid pair",
    );
    let only_db = TestRequest::get(&format!("{CONFIG}?only_db=true"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        only_db.json_value()["configs"],
        json!([]),
        "the refused batch wrote nothing, not even its valid first item"
    );

    let wrong_group = post_configs(
        &app,
        &token,
        json!([{ "group": ":wired", "key": ":instance", "value": json!([]) }]),
    )
    .await;
    common::assert_error(
        &wrong_group,
        "Config not allowed: :wired/:instance",
        "a real key in an unregistered group",
    );

    let valueless = post_configs(
        &app,
        &token,
        json!([{ "group": ":pleroma", "key": ":mrf" }]),
    )
    .await;
    common::assert_answer(
        &valueless,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an upsert without a value",
    );
}

/// The MRF chain is a live consumer of the store: after a first read has
/// filled the pipeline's cache, a `POST /config` setting `:mrf` policies
/// and `:mrf_simple` lists takes effect at once — the instance document's
/// federation block names `SimplePolicy` and publishes the stored list.
#[tokio::test]
async fn setting_mrf_policies_through_the_api_activates_simple_policy() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configmrf").await;

    let before = TestRequest::get("/api/v1/instance").send(&app).await;
    assert_eq!(
        before.json_value()["pleroma"]["metadata"]["federation"]["mrf_policies"],
        json!([]),
        "the fresh instance runs no policy, and this read fills the cache"
    );

    let written = post_configs(
        &app,
        &token,
        json!([
            {
                "group": ":pleroma",
                "key": ":mrf",
                "value": wire(&[("policies", json!(["simple"]))]),
            },
            {
                "group": ":pleroma",
                "key": ":mrf_simple",
                "value": wire(&[("reject", json!([["evil.example", "bots"]]))]),
            },
        ]),
    )
    .await;
    common::assert_answer(&written, StatusCode::OK, "the :mrf batch");
    assert_eq!(
        written.json_value()["need_reboot"],
        json!(false),
        "the chain reloads without a restart"
    );

    let after = TestRequest::get("/api/v1/instance").send(&app).await;
    let federation = after.json_value()["pleroma"]["metadata"]["federation"].clone();
    assert_eq!(
        federation["mrf_policies"],
        json!(["SimplePolicy"]),
        "the POST cleared the cache and the new chain answers"
    );
    assert_eq!(
        federation["mrf_simple"]["reject"],
        json!(["evil.example"]),
        "the :mrf_simple entry is the policy's own lists"
    );

    let deleted = post_configs(
        &app,
        &token,
        json!([
            { "group": ":pleroma", "key": ":mrf", "delete": true },
            { "group": ":pleroma", "key": ":mrf_simple", "delete": true },
        ]),
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the delete batch");
    let reverted = TestRequest::get("/api/v1/instance").send(&app).await;
    assert_eq!(
        reverted.json_value()["pleroma"]["metadata"]["federation"]["mrf_policies"],
        json!([]),
        "and with the rows gone the env fallback runs the empty chain again"
    );
}

/// The `:instance` overrides ride the public documents: the v1 and v2
/// instance titles, the v1 contact email and registration flags, and the
/// `NodeInfo` name and `openRegistrations` all answer from the stored
/// row until it is deleted.
#[tokio::test]
async fn instance_overrides_show_in_the_public_documents() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configname").await;

    let written = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[
                ("name", json!("Renamed Radio")),
                ("email", json!("desk@example.test")),
                ("registrations_open", json!(false)),
                ("account_approval_required", json!(true)),
            ]),
        }]),
    )
    .await;
    common::assert_answer(&written, StatusCode::OK, "the :instance write");

    let v1 = TestRequest::get("/api/v1/instance").send(&app).await;
    let document = v1.json_value();
    assert_eq!(document["title"], "Renamed Radio", "the v1 title");
    assert_eq!(document["email"], "desk@example.test", "the v1 email");
    assert_eq!(document["registrations"], json!(false), "the v1 flag");
    assert_eq!(
        document["approval_required"],
        json!(true),
        "account_approval_required rides the approval gate"
    );

    let v2 = TestRequest::get("/api/v2/instance").send(&app).await;
    assert_eq!(v2.json_value()["title"], "Renamed Radio", "the v2 title");
    assert_eq!(
        v2.json_value()["registrations"]["enabled"],
        json!(false),
        "and the v2 registration block"
    );

    let nodeinfo = TestRequest::get("/nodeinfo/2.0").send(&app).await;
    let summary = nodeinfo.json_value();
    assert_eq!(
        summary["metadata"]["nodeName"], "Renamed Radio",
        "the NodeInfo name"
    );
    assert_eq!(
        summary["openRegistrations"],
        json!(false),
        "and its registration answer"
    );

    let deleted = post_configs(
        &app,
        &token,
        json!([{ "group": ":pleroma", "key": ":instance", "delete": true }]),
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the delete");
    let back = TestRequest::get("/api/v1/instance").send(&app).await;
    assert_eq!(
        back.json_value()["title"],
        "Wired",
        "the env name is back once the row is gone"
    );
    assert_eq!(
        back.json_value()["registrations"],
        json!(true),
        "and so is the env's open door"
    );
}

/// The `:media_proxy` entry of a GET `/config` answer.
fn media_proxy_entry(page: &Value) -> Value {
    page["configs"]
        .as_array()
        .and_then(|configs| {
            configs
                .iter()
                .find(|entry| {
                    entry["key"]
                        .as_str()
                        .is_some_and(|key| key == ":media_proxy")
                })
                .cloned()
        })
        .expect("the media proxy entry answers")
}

/// The `need_reboot` flag follows the registry: a live key's write keeps
/// it down and a restart-flagged key's write raises it — in the POST
/// answer and in every later GET while the row stands.
#[tokio::test]
async fn config_need_reboot_follows_the_changed_keys() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configboot").await;

    let instance_write = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("description", json!("Live change"))]),
        }]),
    )
    .await;
    assert_eq!(
        instance_write.json_value()["need_reboot"],
        json!(false),
        ":instance takes effect without a restart"
    );

    let proxy_write = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":media_proxy",
            "value": wire(&[("max_bytes", json!(1024))]),
        }]),
    )
    .await;
    assert_eq!(
        proxy_write.json_value()["need_reboot"],
        json!(true),
        "the media proxy waits for the restart"
    );

    let standing = TestRequest::get(CONFIG).bearer(&token).send(&app).await;
    assert_eq!(
        standing.json_value()["need_reboot"],
        json!(true),
        "and the flag holds while the stored row stands"
    );
    let page = standing.json_value();
    assert_eq!(
        tuple_value(&media_proxy_entry(&page)["value"], "max_bytes"),
        Some(&json!(1024)),
        "the stored value is the effective one"
    );
}

/// The restart flag clears with the row: deleting the stored
/// `:media_proxy` is itself still a restart-flagged change, and once it
/// stands no more the GET answers the env default and `need_reboot` is
/// down.
#[tokio::test]
async fn deleting_a_restart_flagged_row_lets_the_flag_fall() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = config_admin(&pool, "configreboot").await;

    let opening = post_configs(
        &app,
        &token,
        json!([{
            "group": ":pleroma",
            "key": ":media_proxy",
            "value": wire(&[("max_bytes", json!(1024))]),
        }]),
    )
    .await;
    common::assert_answer(&opening, StatusCode::OK, "the seeded write");

    let deleted = post_configs(
        &app,
        &token,
        json!([{ "group": ":pleroma", "key": ":media_proxy", "delete": true }]),
    )
    .await;
    assert_eq!(
        deleted.json_value()["need_reboot"],
        json!(true),
        "deleting a restart key is still a change waiting for the restart"
    );

    let after = TestRequest::get(CONFIG).bearer(&token).send(&app).await;
    assert_eq!(
        after.json_value()["need_reboot"],
        json!(false),
        "with the row gone nothing waits any more"
    );
    let page = after.json_value();
    assert_eq!(
        tuple_value(&media_proxy_entry(&page)["value"], "max_bytes"),
        Some(&json!(50_000_000)),
        "and the env default is back"
    );
}

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

/// The config routes' gate: the GET asks for `admin:read` and the POST
/// for `admin:write`, each accepting its narrow `admin:*:config` alias;
/// the write scope never grants the read and vice versa, a moderator is
/// refused the role, and no token is 401.
#[tokio::test]
async fn config_routes_answer_the_admin_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, no_scope) = admin(&pool, "plainadmin", "read write").await;
    let (_, read_only) = admin(&pool, "configread", "admin:read:config").await;
    let (_, write_only) = admin(&pool, "configwrite", "admin:write:config").await;
    let (_, mod_write) = moderator(&pool, "configmod", "admin:write:config").await;

    let refused = TestRequest::get(CONFIG).bearer(&no_scope).send(&app).await;
    common::assert_all_refused(
        &[refused],
        StatusCode::FORBIDDEN,
        NO_ADMIN_READ,
        "an admin without any admin:read grant",
    );

    let posted = post_configs(
        &app,
        &write_only,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[("name", json!("WriteOnly Works"))]),
        }]),
    )
    .await;
    common::assert_answer(&posted, StatusCode::OK, "the narrow write scope posts");

    let read_with_write = TestRequest::get(CONFIG)
        .bearer(&write_only)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[read_with_write],
        StatusCode::FORBIDDEN,
        NO_ADMIN_READ,
        "the write scope never grants the read",
    );

    let post_with_read = post_configs(
        &app,
        &read_only,
        json!([{ "group": ":pleroma", "key": ":instance", "delete": true }]),
    )
    .await;
    common::assert_all_refused(
        &[post_with_read],
        StatusCode::FORBIDDEN,
        NO_ADMIN_WRITE,
        "the read scope never grants the write",
    );

    let read_ok = TestRequest::get(CONFIG).bearer(&read_only).send(&app).await;
    common::assert_answer(&read_ok, StatusCode::OK, "the narrow read scope reads");

    let not_admin = post_configs(
        &app,
        &mod_write,
        json!([{ "group": ":pleroma", "key": ":instance", "delete": true }]),
    )
    .await;
    common::assert_all_refused(
        &[not_admin],
        StatusCode::FORBIDDEN,
        NOT_ADMIN,
        "a moderator with the write scope",
    );

    let anonymous = TestRequest::get(CONFIG).send(&app).await;
    common::assert_all_refused(
        &[anonymous],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "an anonymous GET",
    );
    let anonymous_post = post_configs(&app, "no-such-token", json!([])).await;
    common::assert_all_refused(
        &[anonymous_post],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "an unknown token on the POST",
    );
}
