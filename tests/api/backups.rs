//! Account backups over HTTP (`/api/v1/pleroma/backups`, WBE-193): the
//! `GET` list and the `POST` request, against the real router.
//!
//! Backups are strictly per-account: the list holds only the caller's own
//! requests, newest first, each rendered with the six Pleroma
//! `BackupView` keys. A request the job has not built yet says so —
//! `processed: false`, an empty `url`, `file_size: 0`; once
//! `backup::mark_processed` has put an archive behind the row, the entity
//! carries its `/media/...` download URL. `POST` records one row and
//! queues one `backups.run` job on the `background` queue, then answers
//! the whole list; Pleroma's `permitted?` gate refuses a second request
//! inside `BACKUP_LIMIT_DAYS` with the 400 `Last backup was less than {N}
//! days ago` (`0` disables the rule). The gating follows Pleroma's plugs:
//! `read:backups` to look, `write:backups` to request, and both against a
//! user token.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use chrono::{TimeDelta, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;
use wired_be::{
    api::backups::KIND_RUN,
    jobs::{AttemptInfo, QUEUE_BACKGROUND, backups::handle_run},
    models::{backup, job::Job, user::User},
    util::time::parse_time,
};

use crate::common;

/// The backups route.
const ROUTE: &str = "/api/v1/pleroma/backups";
/// The 401 body `AuthUser` sends when no token was presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 wording of the read gate.
const NO_READ: &str = "Insufficient permissions: read:backups.";
/// The 403 wording of the write gate.
const NO_WRITE: &str = "Insufficient permissions: write:backups.";
/// Where the test instance serves stored files from.
const MEDIA_BASE_URL: &str = "https://test.example/media";
/// The default `BACKUP_LIMIT_DAYS` the refusal message counts.
const LIMIT_DAYS: usize = 7;
/// Every JSON key the entity must carry, sorted.
const EXPECTED_KEYS: [&str; 6] = [
    "content_type",
    "file_size",
    "id",
    "inserted_at",
    "processed",
    "url",
];
/// The attempt info a test hands the run handler: the first pass of a job
/// with an ordinary budget.
const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: 5,
};

/// A local user named `nickname` with a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` who may list and request backups.
async fn backup_owner(pool: &PgPool, nickname: &str) -> (User, String) {
    fixture(pool, nickname, "read:backups write:backups").await
}

/// `GET /api/v1/pleroma/backups` as `token`.
async fn list(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(ROUTE)
        .bearer(token)
        .send(app)
        .await
}

/// A `GET` a test only needs to have succeeded, answered as JSON.
async fn listed(app: &Router, token: &str) -> Vec<Value> {
    let response = list(app, token).await;
    common::assert_answer(&response, StatusCode::OK, "the list reads");
    response
        .json_value()
        .as_array()
        .expect("the answer is an array")
        .clone()
}

/// `POST /api/v1/pleroma/backups` as `token`, with no body.
async fn request_backup(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::post(ROUTE)
        .bearer(token)
        .send(app)
        .await
}

/// A `POST` a test only needs to have succeeded, answered as JSON.
async fn requested(app: &Router, token: &str) -> Vec<Value> {
    let response = request_backup(app, token).await;
    common::assert_answer(&response, StatusCode::OK, "the request lands");
    response
        .json_value()
        .as_array()
        .expect("the answer is an array")
        .clone()
}

/// The stored backups of `owner`, newest first.
async fn stored(pool: &PgPool, owner: &User) -> Vec<backup::Backup> {
    backup::list_for_user(pool, owner.id)
        .await
        .expect("the stored rows read")
}

/// The stored `backups.run` job rows, oldest first.
async fn queued_jobs(pool: &PgPool) -> Vec<Job> {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(KIND_RUN)
        .fetch_all(pool)
        .await
        .expect("the queued jobs read")
}

/// Moves the backup `id`'s `inserted_at` `days` days into the past, so a
/// rate-limit test is not a race with the clock. Asserts the row was
/// there to move.
async fn age(pool: &PgPool, id: Uuid, days: i64) {
    let moved = sqlx::query("UPDATE backups SET inserted_at = $1 WHERE id = $2")
        .bind(Utc::now() - TimeDelta::days(days))
        .bind(id)
        .execute(pool)
        .await
        .expect("the fixture row ages")
        .rows_affected();
    assert_eq!(moved, 1, "the row to rewind exists");
}

/// The keys of a rendered entity, sorted — the exact shape the ticket pins.
fn entity_keys(backup: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = backup
        .as_object()
        .expect("backups render objects")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The ids of a rendered entity array, in answer order.
fn ids(body: &[Value]) -> Vec<&str> {
    body.iter()
        .map(|entry| entry["id"].as_str().unwrap_or_default())
        .collect()
}

/// The exact 400 body of the rate limit for the day count `days`.
fn refusal(days: usize) -> String {
    format!("Last backup was less than {days} days ago")
}

#[tokio::test]
async fn the_routes_refuse_requests_without_a_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);

    for response in [
        common::TestRequest::get(ROUTE).send(&app).await,
        common::TestRequest::post(ROUTE).send(&app).await,
    ] {
        common::assert_answer(&response, StatusCode::UNAUTHORIZED, "no token");
        common::assert_error(&response, NO_TOKEN, "no token");
    }
}

#[tokio::test]
async fn the_routes_gate_on_the_backup_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());

    // A token with no `read` behind it cannot see the list, and a token
    // with no `write:backups` behind it cannot request one.
    let (_, write_only) = fixture(&pool, "writer", "write").await;
    let (_, read_only) = fixture(&pool, "reader", "read").await;
    let (_, read_backups) = fixture(&pool, "narrow", "read:backups").await;

    let refused = [
        list(&app, &write_only).await,
        request_backup(&app, &read_only).await,
        request_backup(&app, &read_backups).await,
    ];
    common::assert_answer(&refused[0], StatusCode::FORBIDDEN, "GET without read");
    common::assert_error(&refused[0], NO_READ, "GET without read");
    common::assert_answer(
        &refused[1],
        StatusCode::FORBIDDEN,
        "POST without write:backups",
    );
    common::assert_error(&refused[1], NO_WRITE, "POST without write:backups");
    common::assert_answer(
        &refused[2],
        StatusCode::FORBIDDEN,
        "POST with a read-only backup scope",
    );
    common::assert_error(&refused[2], NO_WRITE, "POST with a read-only backup scope");

    // The roots satisfy the sub-scopes too (decision 9), so a plain
    // `read write` token lists and requests.
    let (_, rooted) = fixture(&pool, "rooted", "read write").await;
    let answers = [
        list(&app, &rooted).await,
        request_backup(&app, &rooted).await,
    ];
    for response in answers {
        common::assert_answer(
            &response,
            StatusCode::OK,
            "the root scopes satisfy the pair",
        );
    }
}

#[tokio::test]
async fn a_fresh_account_lists_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = backup_owner(&pool, "alice").await;

    let response = list(&app, &token).await;
    common::assert_answer(&response, StatusCode::OK, "an account with no requests");
    assert_eq!(
        response.json_value(),
        Value::Array(Vec::new()),
        "the answer is []"
    );
    assert_eq!(
        stored(&pool, &alice).await.len(),
        0,
        "and the listing read records nothing"
    );
}

#[tokio::test]
async fn an_unprocessed_request_lists_without_a_file() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = backup_owner(&pool, "alice").await;

    // The row the job has not got round to: created straight through the
    // model, exactly as `POST` records it before enqueueing its job.
    let row = backup::create(&pool, &alice)
        .await
        .expect("the fixture backup creates");

    let body = listed(&app, &token).await;
    assert_eq!(body.len(), 1, "her one request");
    let entry = &body[0];
    assert_eq!(entity_keys(entry), EXPECTED_KEYS, "exactly the six keys");
    assert_eq!(entry["id"], row.id.to_string(), "the hyphenated UUIDv7");
    assert_eq!(entry["content_type"], "application/zip");
    assert_eq!(entry["url"], "", "no archive, no download");
    assert_eq!(entry["file_size"], 0, "and no bytes behind it");
    assert_eq!(entry["processed"], false, "the request waits unprocessed");
    let stamp = entry["inserted_at"]
        .as_str()
        .expect("inserted_at is a string");
    assert_eq!(stamp.len(), 24, "the Mastodon millisecond format: {stamp}");
    assert!(parse_time(stamp).is_some(), "and it parses: {stamp}");
}

#[tokio::test]
async fn a_processed_request_lists_its_download_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    // The same uploads configuration `common::test_config_with_uploads_dir`
    // builds and `common::test_app_with_uploads_dir` mounts.
    let app = common::test_app_with_uploads_dir(pool.clone(), uploads.path());
    let (alice, token) = backup_owner(&pool, "alice").await;
    let row = backup::create(&pool, &alice)
        .await
        .expect("the fixture backup creates");
    let key = format!("backups/{}", row.file_name);

    let marked = backup::mark_processed(&pool, row.id, 4_096, &key)
        .await
        .expect("the mark runs")
        .expect("the requested row is there");
    assert_eq!(marked.storage_key.as_deref(), Some(key.as_str()));

    let body = listed(&app, &token).await;
    assert_eq!(body.len(), 1, "her one request");
    let entry = &body[0];
    assert_eq!(entity_keys(entry), EXPECTED_KEYS, "still exactly six");
    assert_eq!(entry["processed"], true, "the job landed");
    assert_eq!(entry["file_size"], 4_096, "with its measured size");
    assert_eq!(
        entry["url"],
        format!("{MEDIA_BASE_URL}/{key}"),
        "the stored archive's /media URL"
    );
}

#[tokio::test]
async fn post_records_the_row_queues_the_job_and_answers_the_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = backup_owner(&pool, "alice").await;

    let body = requested(&app, &token).await;
    assert_eq!(body.len(), 1, "the answer is the one-element list");
    let entry = &body[0];
    assert_eq!(entity_keys(entry), EXPECTED_KEYS, "the same six keys");
    assert_eq!(
        entry["processed"], false,
        "the archive is the job's business"
    );
    assert_eq!(entry["url"], "", "and until it runs there is no download");

    let rows = stored(&pool, &alice).await;
    assert_eq!(rows.len(), 1, "one row recorded");
    assert_eq!(entry["id"], rows[0].id.to_string(), "and it is hers");
    let stamp = rows[0]
        .file_name
        .strip_prefix("archive-alice-")
        .and_then(|rest| rest.strip_suffix(".zip"))
        .expect("the name is alice's archive");
    assert_eq!(stamp.len(), 14, "the stamp is YYYYMMDDHHMMSS, {stamp}");
    assert!(
        stamp.chars().all(|c| c.is_ascii_digit()),
        "and it is all digits, not {stamp}"
    );

    let jobs = queued_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "one job queued");
    assert_eq!(jobs[0].queue, QUEUE_BACKGROUND, "on the background queue");
    assert_eq!(jobs[0].kind, KIND_RUN, "the backup runner");
    assert_eq!(
        jobs[0].args["backup_id"],
        rows[0].id.to_string(),
        "carrying the row's id"
    );

    // Pleroma's create re-renders the whole list: the answer and a
    // following GET are the same array.
    assert_eq!(listed(&app, &token).await, body, "GET shows the same list");
}

#[tokio::test]
async fn post_is_refused_inside_the_limit_window_and_permitted_past_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = backup_owner(&pool, "alice").await;

    let _first_body = requested(&app, &token).await;
    let first = stored(&pool, &alice)
        .await
        .first()
        .expect("the POST recorded a row")
        .clone();

    // A fresh request is inside the window, and so is one aged to six
    // days: both refusals are the 400 with the exact wording, and both
    // record nothing.
    for days in [0, 6] {
        if days > 0 {
            age(&pool, first.id, days).await;
        }
        let response = request_backup(&app, &token).await;
        common::assert_answer(&response, StatusCode::BAD_REQUEST, "inside the window");
        common::assert_error(&response, &refusal(LIMIT_DAYS), "inside the window");
        assert_eq!(stored(&pool, &alice).await.len(), 1, "and nothing recorded");
        assert_eq!(queued_jobs(&pool).await.len(), 1, "and nothing queued");
    }

    // Exactly the limit is out of the window's far edge; a request aged
    // past it lands and answers the two-element list.
    age(
        &pool,
        first.id,
        i64::try_from(LIMIT_DAYS).expect("seven is a usize count"),
    )
    .await;
    let body = requested(&app, &token).await;
    assert_eq!(body.len(), 2, "the whole list again, newest first");
    let rows = stored(&pool, &alice).await;
    assert_eq!(rows.len(), 2, "two requests recorded");
    assert_eq!(
        ids(&body),
        vec![rows[0].id.to_string(), first.id.to_string()]
    );
    assert_eq!(queued_jobs(&pool).await.len(), 2, "and two jobs queued");
}

#[tokio::test]
async fn a_zero_limit_permits_back_to_back_requests() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.backup_limit_days = 0;
    let app = common::test_app_with_config(pool.clone(), config);
    let (alice, token) = backup_owner(&pool, "alice").await;

    let first = requested(&app, &token).await;
    let second = requested(&app, &token).await;
    assert_eq!(first.len(), 1, "the first request lands");
    assert_eq!(second.len(), 2, "and the second stands right beside it");
    assert_eq!(stored(&pool, &alice).await.len(), 2, "two rows, no window");
    assert_eq!(queued_jobs(&pool).await.len(), 2, "two jobs, no window");
}

#[tokio::test]
async fn backups_are_per_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = backup_owner(&pool, "alice").await;
    let (_bob, bob_token) = backup_owner(&pool, "bob").await;

    let _alice_body = requested(&app, &alice_token).await;

    // Bob sees none of Alice's requests — and her window is not his: his
    // own first POST lands straight away.
    let empty_for_bob = listed(&app, &bob_token).await;
    assert!(
        empty_for_bob.is_empty(),
        "Bob's list holds no foreign backup"
    );
    let one_for_bob = requested(&app, &bob_token).await;
    assert_eq!(one_for_bob.len(), 1, "his own request lands beside nothing");

    let for_alice = listed(&app, &alice_token).await;
    assert_eq!(for_alice.len(), 1, "and hers holds only her own");
    assert_eq!(
        ids(&for_alice),
        vec![stored(&pool, &alice).await[0].id.to_string()]
    );
    assert!(
        !ids(&for_alice).contains(&one_for_bob[0]["id"].as_str().unwrap_or_default()),
        "Bob's id never appears in Alice's list"
    );
}

#[tokio::test]
async fn the_run_job_behind_a_post_makes_the_archive_downloadable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let app = common::test_app_with_uploads_dir(pool.clone(), uploads.path());
    let (alice, token) = backup_owner(&pool, "alice").await;

    // The whole path, in order: the endpoint records the request and
    // queues the job with its arguments...
    let body = requested(&app, &token).await;
    assert_eq!(body.len(), 1, "one request recorded");
    assert_eq!(
        body[0]["processed"], false,
        "the POST itself builds nothing"
    );
    let row = stored(&pool, &alice)
        .await
        .into_iter()
        .next()
        .expect("the POST recorded a row");
    let jobs = queued_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "one job queued");

    // ...the worker runs it, over the very arguments the endpoint wrote...
    let state = common::test_app_state(
        pool.clone(),
        common::test_config_with_uploads_dir(uploads.path()),
    );
    handle_run(state, FIRST_ATTEMPT, jobs[0].args.clone())
        .await
        .expect("the run job completes on the endpoint's own arguments");

    // ...and the listing shows the processed entity with its download URL.
    let done = listed(&app, &token).await;
    let entry = &done[0];
    assert_eq!(entry["id"], row.id.to_string(), "the same request");
    assert_eq!(entry["processed"], true, "the job landed");
    let size = entry["file_size"].as_i64().expect("file_size is a number");
    assert!(size > 0, "the archive carries bytes, not {size}");
    let key = format!("backups/{}/{}", alice.id, row.file_name);
    let url = entry["url"].as_str().expect("url is a string");
    assert_eq!(
        url,
        format!("{MEDIA_BASE_URL}/{key}"),
        "the stored archive's /media URL"
    );
    assert!(
        url.contains("/media/backups/"),
        "the download stands under /media/backups/: {url}"
    );

    // The bytes live at that key in the uploads store, sized as the
    // entity says, and the media route serves the URL the entity names.
    let stored_file = uploads.path().join(&key);
    assert!(stored_file.is_file(), "the archive is on disk: {key}");
    assert_eq!(
        std::fs::metadata(&stored_file)
            .expect("the stored file is there")
            .len(),
        u64::try_from(size).expect("a file size fits a u64"),
        "and file_size describes the stored file"
    );
    let fetched = common::TestRequest::get(&format!("/media/{key}"))
        .send(&app)
        .await;
    common::assert_answer(
        &fetched,
        StatusCode::OK,
        "the entity's URL serves the archive",
    );
    assert!(
        fetched.body.starts_with(b"PK"),
        "the served bytes open as a zip"
    );
}
