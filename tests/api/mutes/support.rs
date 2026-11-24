//! The fixtures, request helpers and assertions the mute tests share. The
//! test modules are its children, so they reach every helper through
//! `use super::*`.

mod gates_and_expiry;
mod listing;
mod writes;

use axum::{Router, http::StatusCode};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    jobs::{AttemptInfo, mutes::handle_expire},
    models::{
        mute::{self, MuteOptions},
        user::User,
    },
    util::time::mastodon_time,
};

use crate::common;

/// The muted-listing path, as the `Link` header links back to it.
const MUTES: &str = "/api/v1/mutes";
/// The 422 body `POST …/mute` answers for the caller's own id: the wording
/// this endpoint's ticket pins.
const SELF_MUTE: &str = r#"{"error":"Can not mute yourself"}"#;
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body of a write whose token grants neither accepted scope;
/// `require_any` joins the alternatives with the `|` operator.
const NO_WRITE_SCOPE: &str = r#"{"error":"Insufficient permissions: write:mutes | follow."}"#;
/// The 403 body of a listing whose token lacks `read:mutes`.
const NO_READ_SCOPE: &str = r#"{"error":"Insufficient permissions: read:mutes."}"#;
/// The 404 body every account route shares.
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 400 body a malformed page cursor gets from the extractor.
const INVALID_ID: &str = r#"{"error":"Invalid id"}"#;
/// The prefix of the 422 body a parameter of the wrong shape gets from the
/// extractor (decision 10), whatever the detail serde added.
const INVALID_PARAMS: &str = r#"{"error":"Invalid parameters: "#;
/// The two write actions, so the auth and scope loops drive both.
const ACTIONS: [&str; 2] = ["mute", "unmute"];
/// The attempt info a test hands the sweep handler: the first pass of a job
/// with an ordinary budget, which is all [`handle_expire`] looks at.
const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: 5,
};

/// One fixture mute: the `mutes` row id the listing keys on, next to the muted
/// account's id as its entity shows it and the expiry as `mute_expires_at`
/// renders it (`None` for an indefinite mute).
type Listed = (Uuid, String, Option<String>);

/// The relationship JSON for `target` with nothing recorded about them: every
/// flag at the documented default — `showing_reblogs` true, `note` empty, the
/// rest false.
fn relationship_default(target: &Uuid) -> Value {
    json!({
        "id": target.to_string(),
        "following": false,
        "requested": false,
        "endorsed": false,
        "followed_by": false,
        "blocking": false,
        "blocked_by": false,
        "muting": false,
        "muting_notifications": false,
        "notifying": false,
        "showing_reblogs": true,
        "domain_blocking": false,
        "note": "",
        "subscribing": false,
    })
}

/// [`relationship_default`] with the flags in `computed` overwritten: the
/// tests name exactly what the write changed.
fn relationship(target: &Uuid, computed: &Value) -> Value {
    let mut value = relationship_default(target);
    let base = value.as_object_mut().expect("the default is an object");
    let overrides = computed.as_object().expect("the flags are a JSON object");
    for (key, flag) in overrides {
        let replaced = base.insert(key.clone(), flag.clone());
        assert!(
            replaced.is_some(),
            "`{key}` overrides a documented default, not a typo"
        );
    }
    value
}

/// A request for one of the ticket's three routes: `GET` the listing, `POST`
/// either action path.
fn request(path: &str) -> common::TestRequest {
    if path == MUTES {
        common::TestRequest::get(path)
    } else {
        common::TestRequest::post(path)
    }
}

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A `GET` of `path` with `token` (`None` anonymous).
async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    match token {
        Some(found) => common::TestRequest::get(path).bearer(found).send(app).await,
        None => common::TestRequest::get(path).send(app).await,
    }
}

/// The mute action route of account `target`, `mute` or `unmute`.
fn action_path(target: &Uuid, action: &str) -> String {
    format!("/api/v1/accounts/{target}/{action}")
}

/// The same route with `id` as the raw path segment, so a test can feed
/// something that is not an id.
fn raw_action_path(id: &str, action: &str) -> String {
    format!("/api/v1/accounts/{id}/{action}")
}

/// Mutes `target` as `token` with no parameters and answers the relationship
/// the ticket pins for it — `muting` and `muting_notifications` both true: the
/// fixture every later test starts from.
async fn mute_account(app: &Router, token: &str, target: &Uuid) {
    let response = common::TestRequest::post(&action_path(target, "mute"))
        .bearer(token)
        .send(app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the mute is written");
    assert_eq!(
        response.json_value(),
        relationship(
            target,
            &json!({ "muting": true, "muting_notifications": true })
        ),
        "the answer states the stored fact with both of its flags"
    );
}

/// Mutes `target` for `muter` straight through the model, with `options`: the
/// fixture a test needs in a state the endpoints cannot write in one call (or
/// needs before it checks what a call changes).
async fn seed_mute(pool: &PgPool, muter: &User, target: &User, options: MuteOptions) {
    let row = mute::mute(pool, muter, target, options)
        .await
        .expect("the fixture mute stores");
    assert_eq!(row.muted_id, target.id, "the row is the pair asked for");
}

/// Moves the pair's `expires_at` to `seconds` from now (negative for the past)
/// in one statement: the state a timed mute reaches when its duration runs out,
/// which no endpoint writes and every read has to honour.
async fn set_expiry(pool: &PgPool, muter: &User, target: &User, seconds: i64) {
    let sql = "UPDATE mutes SET expires_at = now() + make_interval(secs => $3)
                WHERE muter_id = $1 AND muted_id = $2";
    let moved = sqlx::query(sql)
        .bind(muter.id)
        .bind(target.id)
        .bind(seconds)
        .execute(pool)
        .await
        .expect("the expiry moves");
    assert_eq!(moved.rows_affected(), 1, "the pair's row was there to move");
}

/// The muted ids of `stored`, in order.
fn muted_ids(stored: &[Listed]) -> Vec<String> {
    stored.iter().map(|(_, muted, _)| muted.clone()).collect()
}

/// One `mutes` row as the fixture reads it back: the row's own id, the muted
/// account's id, and the expiry as the database stored it.
type MuteRow = (Uuid, Uuid, Option<DateTime<Utc>>);

/// The stored rows of `muter`'s mutes, newest first — read from storage so
/// same-millisecond `UUIDv7`s cannot make an expectation depend on insert
/// timing (decision 4) — with the expiry rendered in the same format the
/// listing sends it in.
async fn stored_mutes(pool: &PgPool, muter: &User) -> Vec<Listed> {
    let sql = "SELECT id, muted_id, expires_at FROM mutes WHERE muter_id = $1 ORDER BY id DESC";
    let rows: Vec<MuteRow> = sqlx::query_as::<_, MuteRow>(sql)
        .bind(muter.id)
        .fetch_all(pool)
        .await
        .expect("the stored rows read");
    rows.into_iter()
        .map(|(id, muted, expires_at)| {
            (
                id,
                muted.to_string(),
                expires_at.map(|at| mastodon_time(&at)),
            )
        })
        .collect()
}

/// The number of rows in the table.
async fn mute_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM mutes")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// The relationship `token`'s account holds to `target`, as the batch endpoint
/// answers it — how a test pins the flags from the other end of a pair.
async fn relationship_of(app: &Router, token: &str, target: &Uuid) -> Value {
    let response = get(
        app,
        Some(token),
        &format!("/api/v1/accounts/relationships?id%5B%5D={target}"),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "the batch answers");
    assert_eq!(
        response.json_value().as_array().expect("an array").len(),
        1,
        "one relationship for the one requested id"
    );
    response.json_value()[0].clone()
}

/// The ids whose notifications `muter` hides: the shorter sibling of a
/// read-back assertion that says "nothing" without spelling out a `Vec<Uuid>`.
async fn notification_ids(pool: &PgPool, muter: Uuid) -> Vec<Uuid> {
    mute::notification_muted_ids(pool, muter)
        .await
        .expect("the read works")
}

/// One entry of the mute listing: the `Account` entity under the muted
/// account's id, with `mute_expires_at` beside it — the row's own expiry, or
/// `null` for an indefinite mute.
fn assert_muted_entry(entry: &Value, account_id: &str, expires: Option<String>) {
    assert_eq!(
        entry["id"],
        json!(account_id),
        "an entry is a rendered `Account`, keyed on the muted account's id"
    );
    assert!(
        entry["username"].is_string(),
        "with the account's own fields beside it"
    );
    assert_eq!(
        entry["mute_expires_at"],
        expires.map_or(Value::Null, |text| json!(text)),
        "the expiry the row carries, or null for an indefinite mute"
    );
}
