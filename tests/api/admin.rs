//! The admin foundation over HTTP (`/api/v1/pleroma/admin`, WBE-194): the
//! [`AdminUser`] and [`StaffUser`] extractors, the moderation-log listing
//! and its filters and pagination, and the visibility `stats`, all against
//! the real router (plus a two-route test router that exercises the
//! [`StaffUser`] gate, which guards no production route yet). The admin
//! config store (WBE-206) — `GET`/`POST /config` over the whitelist
//! registry, the `:mrf`/`:instance` consumers and the `config_change`
//! log line — joins at the bottom of this file, and with it the admin
//! invites (WBE-199): `users/invite_token`, `users/invites`,
//! `users/revoke_invite` and `users/email_invite`, their `invite_create`
//! and `invite_revoke` log lines, and the `GET /registration/{token}`
//! redirect the invitation letters carry.
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
    config::{Config, Toggle},
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

// ---------------------------------------------------------------------------
// Admin invites (WBE-199): `POST /users/invite_token`, `GET /users/invites`,
// `POST /users/revoke_invite`, `POST /users/email_invite` and the root-level
// `GET /registration/{token}` link. Pleroma's `AdminAPI.InviteController`
// gates the read with `admin:read:invites` and the three POSTs with
// `admin:write:invites`; this instance logs the two state changes into the
// moderation log and mails the registration link the frontend serves.
// ---------------------------------------------------------------------------

/// The invite-creation route.
const INVITE_CREATE: &str = "/api/v1/pleroma/admin/users/invite_token";
/// The invite-listing route.
const INVITE_LIST: &str = "/api/v1/pleroma/admin/users/invites";
/// The invite-revocation route.
const INVITE_REVOKE: &str = "/api/v1/pleroma/admin/users/revoke_invite";
/// The emailed-invite route.
const INVITE_EMAIL: &str = "/api/v1/pleroma/admin/users/email_invite";
/// The registration-link route, merged at the app root.
const REGISTRATION: &str = "/registration";
/// The 400 body of the email route while `INVITES_ENABLED` is off.
const INVITES_DISABLED: &str =
    "To send invites you need to set the `invites_enabled` option to true.";
/// The 400 body of the email route when the body names no `email`.
const MISSING_EMAIL: &str = "Missing parameter: email";
/// The 422 body of the revoke route when the body names no `token`.
const MISSING_TOKEN: &str = "Missing parameter: token";
/// The 404 body of the revoke route for a token no invite carries.
const NOT_FOUND: &str = "Not found";

/// An admin holding both invite scopes, as its bearer token.
async fn invite_admin(pool: &PgPool, nickname: &str) -> String {
    admin(pool, nickname, "admin:read:invites admin:write:invites")
        .await
        .1
}

/// `POST`s `body` to an invite route as `token`, over JSON.
async fn post_invite(app: &Router, token: &str, path: &str, body: Value) -> common::TestResponse {
    TestRequest::post(path)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// A [`Config`] with invites on and mail up: the gate opens, and
/// `deliver_later` really queues the letter.
fn invites_open_config() -> Config {
    let mut config = common::test_config();
    config.invites_enabled = Toggle::new(true);
    config.smtp_host = Some("smtp.example.org".to_owned());
    config
}

/// The `invite_tokens` rows as `(id, token, invite_type)` texts, id order.
async fn stored_invites(pool: &PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as("SELECT id::text, token, invite_type FROM invite_tokens ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("the invite table reads back")
}

/// The queued `mailer.send` jobs' arguments, oldest first.
async fn queued_letters(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT args FROM jobs WHERE queue = 'mailer' AND kind = 'mailer.send' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("the mailer queue reads back")
}

/// Seeds one `invite_tokens` row under a controlled `id`, so the listing's
/// order is fixed instead of riding on the millisecond the server minted
/// its `UUIDv7`s in.
async fn seed_invite(pool: &PgPool, id: u128, token: &str) {
    let _seeded = sqlx::query(
        "INSERT INTO invite_tokens (id, token, invite_type) VALUES ($1, $2, 'reusable')",
    )
    .bind(Uuid::from_u128(id))
    .bind(token)
    .execute(pool)
    .await
    .expect("the seed invite inserts");
}

/// A freshly minted entity answers its own seven whole keys: a uuid id
/// text, the model's 22-character secret, and an unserved, unspent row.
fn assert_fresh_entity(entity: &Value) {
    assert_eq!(entity["used"], false, "a fresh invite is unspent");
    assert_eq!(entity["uses"], 0, "and has served nothing");
    let id = entity["id"].as_str().expect("the id is text");
    assert!(Uuid::parse_str(id).is_ok(), "the id is uuid text: {id}");
    assert_eq!(
        entity["token"].as_str().map(str::len),
        Some(22),
        "the minted secret is the model's 22 characters"
    );
    assert_eq!(
        entity.as_object().map(serde_json::Map::len),
        Some(7),
        "exactly the view's seven keys, no created_at: {entity}"
    );
}

/// The `POST /users/invite_token` derivation: neither limit is the
/// one-time invite, a cap is the reusable one, a date the date-limited
/// one, and both together the combination — each answered as the entity
/// with its stored limits, and each standing in the table. A
/// form-encoded numeric string reads as the number it spells.
#[tokio::test]
async fn invite_tokens_derive_their_type_from_the_limits() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = invite_admin(&pool, "invitecreator").await;

    let plain = post_invite(&app, &token, INVITE_CREATE, json!({})).await;
    common::assert_answer(&plain, StatusCode::OK, "the plain invite");
    let entity = plain.json_value();
    assert_eq!(entity["invite_type"], "one_time", "neither limit");
    assert_eq!(entity["expires_at"], Value::Null, "no expiry to name");
    assert_eq!(entity["max_use"], Value::Null, "and no cap");
    assert_fresh_entity(&entity);

    let capped = post_invite(&app, &token, INVITE_CREATE, json!({"max_use": 5})).await;
    let capped = capped.json_value();
    assert_eq!(capped["invite_type"], "reusable", "a cap alone");
    assert_eq!(capped["max_use"], 5, "and the cap round-trips");

    let dated = post_invite(
        &app,
        &token,
        INVITE_CREATE,
        json!({"expires_at": "2099-01-01"}),
    )
    .await;
    let dated = dated.json_value();
    assert_eq!(dated["invite_type"], "date_limited", "a date alone");
    assert_eq!(
        dated["expires_at"], "2099-01-01",
        "the plain YYYY-MM-DD day round-trips"
    );

    let both = post_invite(
        &app,
        &token,
        INVITE_CREATE,
        json!({"max_use": "3", "expires_at": "2099-01-01"}),
    )
    .await;
    let both = both.json_value();
    assert_eq!(
        both["invite_type"], "reusable_date_limited",
        "both limits together"
    );
    assert_eq!(both["max_use"], 3, "the string cap read as a number");
    assert_eq!(both["expires_at"], "2099-01-01", "beside the date");

    let form = TestRequest::post(INVITE_CREATE)
        .bearer(&token)
        .form(&[("max_use", "2")])
        .send(&app)
        .await;
    common::assert_answer(&form, StatusCode::OK, "the form-encoded cap");
    assert_eq!(
        form.json_value()["invite_type"],
        "reusable",
        "and the same derivation from a urlencoded body"
    );

    let mut stored_types: Vec<String> = stored_invites(&pool)
        .await
        .into_iter()
        .map(|(_, _, kind)| kind)
        .collect();
    stored_types.sort_unstable();
    assert_eq!(
        stored_types,
        vec![
            "date_limited".to_owned(),
            "one_time".to_owned(),
            "reusable".to_owned(),
            "reusable".to_owned(),
            "reusable_date_limited".to_owned(),
        ],
        "every answer stands in the table with its derived type"
    );
}

/// The `GET /users/invites` answer: `{"invites": […]}`, newest first on
/// the id's creation order, every entity whole.
#[tokio::test]
async fn the_invite_list_answers_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = invite_admin(&pool, "invitelisten").await;

    let empty = TestRequest::get(INVITE_LIST)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&empty, StatusCode::OK, "the fresh listing");
    assert_eq!(
        empty.json_value(),
        json!({"invites": []}),
        "an empty table lists as an empty array"
    );

    seed_invite(&pool, 1, "oldest-invite").await;
    seed_invite(&pool, 2, "middle-invite").await;
    seed_invite(&pool, 3, "newest-invite").await;

    let listed = TestRequest::get(INVITE_LIST)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&listed, StatusCode::OK, "the seeded listing");
    let page = listed.json_value();
    let tokens: Vec<&str> = page["invites"]
        .as_array()
        .expect("the invites array")
        .iter()
        .map(|entry| entry["token"].as_str().expect("a token text"))
        .collect();
    assert_eq!(
        tokens,
        vec!["newest-invite", "middle-invite", "oldest-invite"],
        "the newest invite leads the array"
    );
    assert_eq!(
        page["invites"][0],
        json!({
            "id": Uuid::from_u128(3).to_string(),
            "token": "newest-invite",
            "used": false,
            "expires_at": null,
            "uses": 0,
            "max_use": null,
            "invite_type": "reusable",
        }),
        "each entry is the seven-key entity"
    );
}

/// The `POST /users/revoke_invite` lifecycle: the named invite is deleted
/// and answered as the entity it was, it drops out of the listing, a
/// second revoke (or any unknown token) is the 404, and a body without
/// `token` is the 422.
#[tokio::test]
async fn revoking_an_invite_deletes_it_and_answers_the_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = invite_admin(&pool, "inviterevoker").await;

    let created = post_invite(&app, &token, INVITE_CREATE, json!({"max_use": 2})).await;
    common::assert_answer(&created, StatusCode::OK, "the create");
    let entity = created.json_value();

    let revoked = post_invite(
        &app,
        &token,
        INVITE_REVOKE,
        json!({"token": entity["token"].clone()}),
    )
    .await;
    common::assert_answer(&revoked, StatusCode::OK, "the revoke");
    assert_eq!(
        revoked.json_value(),
        entity,
        "the entity exactly as the create answered it"
    );

    let listed = TestRequest::get(INVITE_LIST)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        listed.json_value(),
        json!({"invites": []}),
        "the revoked invite is deleted, not marked used"
    );

    let again = post_invite(
        &app,
        &token,
        INVITE_REVOKE,
        json!({"token": entity["token"].clone()}),
    )
    .await;
    common::assert_all_refused(
        &[again],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "revoking the gone invite again",
    );
    let bogus = post_invite(
        &app,
        &token,
        INVITE_REVOKE,
        json!({"token": "never-minted"}),
    )
    .await;
    common::assert_all_refused(
        &[bogus],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "an unknown token",
    );

    let bodiless = post_invite(&app, &token, INVITE_REVOKE, json!({})).await;
    common::assert_all_refused(
        &[bodiless],
        StatusCode::UNPROCESSABLE_ENTITY,
        MISSING_TOKEN,
        "a revoke without a token",
    );
}

/// The two invite writes land in the moderation log: `invite_create` and
/// `invite_revoke` sentences naming the invite id, each findable by the
/// listing's search with its action and `subject_id` in the data map.
#[tokio::test]
async fn invite_writes_record_the_moderation_log() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "invitelogger", "admin:read admin:write").await;

    let created = post_invite(&app, &token, INVITE_CREATE, json!({})).await;
    let entity = created.json_value();
    let id = entity["id"].as_str().expect("the id is text").to_owned();
    let revoked = post_invite(
        &app,
        &token,
        INVITE_REVOKE,
        json!({"token": entity["token"].clone()}),
    )
    .await;
    common::assert_answer(&revoked, StatusCode::OK, "the revoke");

    for (search, verb, action) in [
        ("created+invite", "created", "invite_create"),
        ("revoked+invite", "revoked", "invite_revoke"),
    ] {
        let found = TestRequest::get(&format!("{LOG}?search={search}"))
            .bearer(&token)
            .send(&app)
            .await;
        let page = found.json_value();
        assert_eq!(page["total"], json!(1), "one {action} line");
        let entry = &page["items"][0];
        assert_eq!(
            entry["message"],
            format!("@invitelogger {verb} invite: {id}"),
            "the sentence names the invite id"
        );
        assert_eq!(entry["data"]["action"], action, "the action");
        assert_eq!(
            entry["data"]["subject_id"], id,
            "and the invite rides in subject_id"
        );
        assert_eq!(
            entry["data"]["actor"]["nickname"], "invitelogger",
            "beside the acting admin, as every entry carries it"
        );
    }
}

/// The `POST /users/email_invite` success: 204 with an empty body, a
/// one-time invite in the table, the `user_invitation` letter on the
/// mailer queue carrying the `{BASE_URL}/registration/{token}` link —
/// and the `invite_create` log line it records. The optional `name` is
/// accepted (Pleroma's recipient display name; this mailer addresses
/// bare recipients).
#[tokio::test]
async fn email_invite_letters_the_registration_link() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), invites_open_config());
    let (_, token) = admin(&pool, "invitemailer", "admin:read admin:write").await;

    let sent = post_invite(
        &app,
        &token,
        INVITE_EMAIL,
        json!({"email": "friend@example.org", "name": "Friend"}),
    )
    .await;
    common::assert_answer(&sent, StatusCode::NO_CONTENT, "the emailed invite");
    assert!(sent.body.is_empty(), "the 204 body is empty");

    let stored = stored_invites(&pool).await;
    assert_eq!(stored.len(), 1, "the letter mints one invite");
    let (invite_id, invite_token, kind) = &stored[0];
    assert_eq!(kind, "one_time", "and it is a one-time invite");

    let letters = queued_letters(&pool).await;
    assert_eq!(letters.len(), 1, "one queued send");
    assert_eq!(
        letters[0]["to"],
        json!("friend@example.org"),
        "the bare addressed recipient"
    );
    assert_eq!(
        letters[0]["subject"],
        json!("Invitation to Wired"),
        "the instance-named subject"
    );
    assert_eq!(
        letters[0]["text_body"],
        json!(format!(
            "You are invited to Wired.\n\n\
             Open the following link to register:\n\n\
             https://test.example/registration/{invite_token}\n"
        )),
        "the letter with its BASE_URL registration link"
    );
    assert_eq!(letters[0]["html_body"], Value::Null, "text only");

    let logged = TestRequest::get(&format!("{LOG}?search=created+invite"))
        .bearer(&token)
        .send(&app)
        .await;
    let page = logged.json_value();
    assert_eq!(page["total"], json!(1), "the emailed create is logged");
    assert_eq!(
        page["items"][0]["message"],
        format!("@invitemailer created invite: {invite_id}"),
        "naming the invite it minted"
    );
}

/// The `POST /users/email_invite` refusals: with `INVITES_ENABLED` off —
/// the suite default — the endpoint answers Pleroma's 400 and mints
/// nothing; with it on, a body without an address (or with only blanks)
/// is the 400 the ticket pins, still minting nothing.
#[tokio::test]
async fn email_invite_refuses_without_the_setting_or_the_address() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let closed = common::test_app(pool.clone());
    let token = invite_admin(&pool, "invitegate").await;

    let disabled = post_invite(
        &closed,
        &token,
        INVITE_EMAIL,
        json!({"email": "friend@example.org"}),
    )
    .await;
    common::assert_all_refused(
        &[disabled],
        StatusCode::BAD_REQUEST,
        INVITES_DISABLED,
        "the mail invite with invites_enabled off",
    );
    assert!(
        stored_invites(&pool).await.is_empty(),
        "the refusal minted no invite"
    );

    let open = common::test_app_with_config(pool.clone(), invites_open_config());
    let missing = post_invite(&open, &token, INVITE_EMAIL, json!({"name": "Friend"})).await;
    common::assert_all_refused(
        &[missing],
        StatusCode::BAD_REQUEST,
        MISSING_EMAIL,
        "the mail invite with no email parameter",
    );
    let blank = post_invite(&open, &token, INVITE_EMAIL, json!({"email": "   "})).await;
    common::assert_all_refused(
        &[blank],
        StatusCode::BAD_REQUEST,
        MISSING_EMAIL,
        "the mail invite with a blank address",
    );
    assert!(
        stored_invites(&pool).await.is_empty(),
        "and neither refusal minted an invite"
    );
}

/// The `GET /registration/{token}` link: a 302 to the same path on
/// `FRONTEND_BASE_URL` — no auth, no lookup, so an unknown token rides
/// the same redirect, a configured frontend splits the target, and the
/// request's own encoding is echoed verbatim.
#[tokio::test]
async fn the_registration_link_redirects_to_the_frontend() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());

    let unknown = TestRequest::get(&format!("{REGISTRATION}/no-such-invite"))
        .send(&app)
        .await;
    common::assert_answer(&unknown, StatusCode::FOUND, "the unauthenticated link");
    assert_eq!(
        unknown.header("location"),
        Some("https://test.example/registration/no-such-invite"),
        "the frontend serves the page; an unknown token redirects the same"
    );

    let mut split = common::test_config();
    split.frontend_base_url = "https://fe.example".to_owned();
    let fe = common::test_app_with_config(pool.clone(), split);
    let away = TestRequest::get(&format!("{REGISTRATION}/abc123"))
        .send(&fe)
        .await;
    assert_eq!(
        away.header("location"),
        Some("https://fe.example/registration/abc123"),
        "a separate frontend takes the link"
    );

    let encoded = TestRequest::get(&format!("{REGISTRATION}/a%2Fb"))
        .send(&app)
        .await;
    assert_eq!(
        encoded.header("location"),
        Some("https://test.example/registration/a%2Fb"),
        "the request's own encoding is echoed, not decoded"
    );
}

/// The invite routes' scope half: the listing asks `admin:read:invites`
/// and the writes `admin:write:invites`, each accepting its narrow alias
/// and refusing the other class.
#[tokio::test]
async fn invite_routes_answer_the_scope_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, read_only) = admin(&pool, "inviteread", "admin:read:invites").await;
    let (_, write_only) = admin(&pool, "invitewrite", "admin:write:invites").await;
    let (_, unscoped) = admin(&pool, "inviteplain", "read write").await;

    let created = post_invite(&app, &write_only, INVITE_CREATE, json!({})).await;
    common::assert_answer(&created, StatusCode::OK, "the narrow write scope creates");
    let emailed = post_invite(
        &app,
        &write_only,
        INVITE_EMAIL,
        json!({"email": "a@b.example"}),
    )
    .await;
    common::assert_all_refused(
        &[emailed],
        StatusCode::BAD_REQUEST,
        INVITES_DISABLED,
        "the write scope reaches the gate on the default instance",
    );

    let listed = TestRequest::get(INVITE_LIST)
        .bearer(&read_only)
        .send(&app)
        .await;
    common::assert_answer(&listed, StatusCode::OK, "the narrow read scope lists");

    let revoke_with_read =
        post_invite(&app, &read_only, INVITE_REVOKE, json!({"token": "x"})).await;
    let create_with_read = post_invite(&app, &read_only, INVITE_CREATE, json!({})).await;
    common::assert_all_refused(
        &[revoke_with_read, create_with_read],
        StatusCode::FORBIDDEN,
        NO_ADMIN_WRITE,
        "the read scope never writes",
    );

    let list_with_write = TestRequest::get(INVITE_LIST)
        .bearer(&write_only)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[list_with_write],
        StatusCode::FORBIDDEN,
        NO_ADMIN_READ,
        "the write scope never lists",
    );

    let unscoped_post = post_invite(&app, &unscoped, INVITE_CREATE, json!({})).await;
    common::assert_all_refused(
        &[unscoped_post],
        StatusCode::FORBIDDEN,
        NO_ADMIN_WRITE,
        "an admin with no admin:write grant",
    );
}

/// The invite routes' role and credential half: a moderator or a regular
/// user holding the invite scopes is refused the role, an app-only token
/// the user, and no token at all the credentials.
#[tokio::test]
async fn invite_routes_answer_the_role_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let scopes = "admin:read:invites admin:write:invites";
    let (_, mod_invites) = moderator(&pool, "invitemod", scopes).await;
    let (_, user_invites) = regular(&pool, "inviteuser", scopes).await;
    let app_only = common::create_app_only_token(&pool, scopes).await;

    for (token, who) in [
        (&mod_invites, "a moderator"),
        (&user_invites, "a regular user"),
    ] {
        let refused = TestRequest::get(INVITE_LIST).bearer(token).send(&app).await;
        common::assert_all_refused(&[refused], StatusCode::FORBIDDEN, NOT_ADMIN, who);
        let posted = post_invite(&app, token, INVITE_CREATE, json!({})).await;
        common::assert_all_refused(&[posted], StatusCode::FORBIDDEN, NOT_ADMIN, who);
    }

    let app_only_refused = TestRequest::get(INVITE_LIST)
        .bearer(&app_only)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[app_only_refused],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "an app-only token",
    );

    let anonymous = TestRequest::get(INVITE_LIST).send(&app).await;
    common::assert_all_refused(
        &[anonymous],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "an anonymous GET",
    );
    let bogus = post_invite(&app, "no-such-token", INVITE_CREATE, json!({})).await;
    common::assert_all_refused(
        &[bogus],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "an unknown token on the POST",
    );
}
