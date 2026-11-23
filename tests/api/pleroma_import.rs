//! CSV imports over HTTP (`/api/pleroma/follow_import`,
//! `/api/pleroma/blocks_import`, `/api/pleroma/mutes_import`, WBE-138)
//! against the real router.
//!
//! The tests pin Pleroma's `UserImportController` contract the ticket
//! fixes: the `list` field arrives as a file upload, a multipart text
//! field or a JSON / urlencoded parameter alike; the answer is the JSON
//! string `"jobs started"`; the accepted list becomes one
//! `imports.follows` / `imports.blocks` / `imports.mutes` job on the
//! `background` queue carrying `{user_id, accts, options}`; and both
//! caps (1 MiB of content, 5000 entries) are the 422
//! `{"error":"Import too large"}`. The gating follows Pleroma's plugs:
//! `follow | write:follows`, `follow | write:blocks`,
//! `follow | write:mutes`. The handlers themselves are exercised in
//! `src/jobs/imports.rs`.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use bytes::Bytes;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::{job::Job, user::User};

use crate::common;

/// The follow import route.
const FOLLOW: &str = "/api/pleroma/follow_import";
/// The block import route.
const BLOCKS: &str = "/api/pleroma/blocks_import";
/// The mute import route.
const MUTES: &str = "/api/pleroma/mutes_import";
/// Boundary the multipart bodies are framed with.
const BOUNDARY: &str = "wired-import-test";
/// The ticket's per-list content cap.
const MAX_IMPORT_BYTES: usize = 1024 * 1024;
/// The ticket's per-list entry cap.
const MAX_IMPORT_ENTRIES: usize = 5000;
/// A Mastodon follows export: header, a row keeping the boosts default,
/// a row with a leading `@` and `False`, CRLF endings and a blank line.
const FOLLOWS_CSV: &str = "Account address,Show boosts,Notify on new posts,Languages\r\n\
                           bob@remote.example,,,\r\n\
                           @carol@other.example,False,1,en\r\n\
                           \r\n";

/// The 422 both caps answer, the ticket's exact wording.
const TOO_LARGE: &str = "Import too large";

/// Inserts `alice` and returns her row with a token granting `scopes`.
async fn fixture(pool: &PgPool, scopes: &str) -> (User, String) {
    let alice = common::create_local_user(pool, "alice").await;
    let token = common::create_app_and_token(pool, &alice, scopes).await;
    (alice, token)
}

/// `POST`s `parts` as multipart to `path` with `token`.
async fn post_multipart(
    app: &Router,
    path: &str,
    token: &str,
    parts: &[common::MultipartPart],
) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .multipart(BOUNDARY, parts)
        .send(app)
        .await
}

/// A `list` file part carrying `csv`. (Built through the struct, not
/// [`common::MultipartPart::file`], whose parameters are `&'static str`
/// and cannot carry the lists the limit tests generate.)
fn file_part(csv: &str) -> common::MultipartPart {
    common::MultipartPart {
        name: "list",
        filename: Some("following_accounts.csv"),
        content_type: Some("text/csv"),
        bytes: Bytes::from(csv.as_bytes().to_vec()),
    }
}

/// A `list` text part carrying `csv`.
fn text_part(csv: &str) -> common::MultipartPart {
    common::MultipartPart {
        name: "list",
        filename: None,
        content_type: None,
        bytes: Bytes::from(csv.as_bytes().to_vec()),
    }
}

/// The stored job rows of `kind`, oldest first.
async fn jobs(pool: &PgPool, kind: &str) -> Vec<Job> {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(kind)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Asserts the 200 `"jobs started"` answer, and hands back nothing else:
/// Pleroma's body is a JSON string, not an object.
fn assert_started(response: &common::TestResponse, who: &str) {
    common::assert_answer(response, StatusCode::OK, who);
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "{who}: JSON like Pleroma's `json(conn, \"jobs started\")`"
    );
    assert_eq!(
        response.json_value(),
        Value::String("jobs started".to_owned()),
        "{who}: the body is the JSON string Pleroma answers with"
    );
}

/// The full import flow through a `list` **file upload**: the parse
/// rules (`@` stripped, blank lines and the header dropped, extra
/// columns ignored) reach the queued job, and the follows' second column
/// becomes the per-entry `show_reblogs` option.
#[tokio::test]
async fn follow_import_accepts_a_file_and_enqueues_the_job() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "write:follows").await;
    let app = common::test_app(pool.clone());

    let response = post_multipart(&app, FOLLOW, &token, &[file_part(FOLLOWS_CSV)]).await;

    assert_started(&response, "follow_import with a file");
    let queued = jobs(&pool, "imports.follows").await;
    assert_eq!(queued.len(), 1, "one job per accepted list");
    let job = &queued[0];
    assert_eq!(job.queue, "background", "on the background queue");
    assert_eq!(job.state, "available", "due for a worker at once");
    assert_eq!(
        job.args["user_id"],
        json!(alice.id.to_string()),
        "the importer is named"
    );
    assert_eq!(
        job.args["accts"],
        json!(["bob@remote.example", "carol@other.example"]),
        "handles trimmed, `@` stripped, blank and header lines dropped"
    );
    assert_eq!(
        job.args["options"],
        json!([
            {"show_reblogs": null},
            {"show_reblogs": false},
        ]),
        "the Show boosts column travels per row; a blank one keeps the default"
    );
}

/// A `list` sent as a multipart **text field** (no file) imports the same
/// way — the ticket's "or a text field with the CSV content".
#[tokio::test]
async fn follow_import_accepts_a_multipart_text_field() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "write:follows").await;
    let app = common::test_app(pool.clone());

    let response = post_multipart(
        &app,
        FOLLOW,
        &token,
        &[text_part("Account address\nbob@x.example\n")],
    )
    .await;

    assert_started(&response, "follow_import with a text field");
    assert_eq!(
        jobs(&pool, "imports.follows").await[0].args["accts"],
        json!(["bob@x.example"]),
        "the text part is the list"
    );
}

/// Blocks and mutes accept the non-multipart bodies of decision 10 too,
/// enqueue their own kind, and carry an empty options list.
#[tokio::test]
async fn blocks_and_mutes_accept_json_and_form_bodies() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "write:blocks write:mutes").await;
    let app = common::test_app(pool.clone());

    let block_response = common::TestRequest::post(BLOCKS)
        .bearer(&token)
        .json(&json!({"list": "Account address\n@bob@remote.example\n"}))
        .send(&app)
        .await;
    assert_started(&block_response, "blocks_import as JSON");

    let mute_response = common::TestRequest::post(MUTES)
        .bearer(&token)
        .form(&[("list", "Account address\n@bob@remote.example\n")])
        .send(&app)
        .await;
    assert_started(&mute_response, "mutes_import as a form");

    let block_jobs = jobs(&pool, "imports.blocks").await;
    let mute_jobs = jobs(&pool, "imports.mutes").await;
    assert_eq!(block_jobs.len(), 1, "one block import job");
    assert_eq!(mute_jobs.len(), 1, "one mute import job");
    assert_eq!(
        block_jobs[0].args["accts"],
        json!(["bob@remote.example"]),
        "the blocks list parsed like the follows one, minus the options"
    );
    assert_eq!(
        block_jobs[0].args["options"],
        json!([]),
        "blocks carry no options"
    );
    assert_eq!(mute_jobs[0].args["options"], json!([]), "neither do mutes");
}

/// Past either cap the answer is the 422 with the ticket's exact body —
/// by entry count (5001 handles inside the byte cap) and by content size
/// (1 MiB + 1 byte in a file and in a text field). Nothing is enqueued.
#[tokio::test]
async fn oversized_imports_are_rejected_with_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "write:follows write:blocks write:mutes").await;
    let app = common::test_app(pool.clone());

    let too_many = (0..=MAX_IMPORT_ENTRIES)
        .map(|i| format!("user{i}@remote.example"))
        .collect::<Vec<_>>()
        .join("\n");
    let by_entries = post_multipart(&app, FOLLOW, &token, &[text_part(&too_many)]).await;
    common::assert_answer(
        &by_entries,
        StatusCode::UNPROCESSABLE_ENTITY,
        "5001 entries",
    );
    common::assert_error(&by_entries, TOO_LARGE, "5001 entries");

    let oversize_file = vec![b'a'; MAX_IMPORT_BYTES + 1];
    let big_file = common::MultipartPart::file(
        "list",
        "big.csv",
        "text/csv",
        Bytes::from(oversize_file.clone()),
    );
    let by_bytes = post_multipart(&app, BLOCKS, &token, &[big_file]).await;
    common::assert_answer(
        &by_bytes,
        StatusCode::UNPROCESSABLE_ENTITY,
        "1 MiB + 1 file",
    );
    common::assert_error(&by_bytes, TOO_LARGE, "1 MiB + 1 file");

    let oversize_text = String::from_utf8(oversize_file).unwrap();
    let by_text = post_multipart(&app, MUTES, &token, &[text_part(&oversize_text)]).await;
    common::assert_answer(
        &by_text,
        StatusCode::UNPROCESSABLE_ENTITY,
        "1 MiB + 1 text field",
    );
    common::assert_error(&by_text, TOO_LARGE, "1 MiB + 1 text field");

    assert!(
        jobs(&pool, "imports.follows").await.is_empty(),
        "the entry-limit job never ran"
    );
    assert!(
        jobs(&pool, "imports.blocks").await.is_empty(),
        "nor the oversize-file one"
    );
    assert!(
        jobs(&pool, "imports.mutes").await.is_empty(),
        "nor the oversize-text one"
    );
}

/// The caps are inclusive: exactly 5000 entries import.
#[tokio::test]
async fn exactly_five_thousand_entries_are_accepted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "follow").await;
    let app = common::test_app(pool.clone());
    let csv = (0..MAX_IMPORT_ENTRIES)
        .map(|i| format!("user{i}@remote.example"))
        .collect::<Vec<_>>()
        .join("\n");

    let response = post_multipart(&app, FOLLOW, &token, &[text_part(&csv)]).await;

    assert_started(&response, "exactly 5000 entries");
    let queued = jobs(&pool, "imports.follows").await;
    assert_eq!(
        queued[0].args["accts"].as_array().map(Vec::len),
        Some(MAX_IMPORT_ENTRIES),
        "the whole list is in the job"
    );
}

/// A body without a `list` anywhere is Pleroma's missing-parameter
/// refusal; an import of nothing answers like one with entries but
/// enqueues no job (Pleroma's `[_ | _]` clauses).
#[tokio::test]
async fn missing_and_empty_lists() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "write:follows").await;
    let app = common::test_app(pool.clone());

    let missing = common::TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({"nope": true}))
        .send(&app)
        .await;
    common::assert_answer(&missing, StatusCode::BAD_REQUEST, "no list");
    common::assert_error(&missing, "Missing parameter: list", "no list");

    let empty = post_multipart(
        &app,
        FOLLOW,
        &token,
        &[text_part("Account address,Show boosts\n\n")],
    )
    .await;
    assert_started(&empty, "an empty list");
    assert!(
        jobs(&pool, "imports.follows").await.is_empty(),
        "nothing is enqueued"
    );
}

/// Every route refuses an anonymous request and a token outside Pleroma's
/// `follow | write:X` pair for that action, and answers with the shared
/// 403 wording naming the pair.
#[tokio::test]
async fn auth_and_scopes_gate_every_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, wrong) = fixture(&pool, "write:accounts").await;
    let app = common::test_app(pool.clone());

    for path in [FOLLOW, BLOCKS, MUTES] {
        let response = common::TestRequest::post(path)
            .json(&json!({"list": "a@b.example"}))
            .send(&app)
            .await;
        common::assert_answer(&response, StatusCode::UNAUTHORIZED, "anonymous import");
        common::assert_error(&response, "Invalid credentials", "anonymous import");
    }

    for (path, needed) in [
        (FOLLOW, "follow | write:follows"),
        (BLOCKS, "follow | write:blocks"),
        (MUTES, "follow | write:mutes"),
    ] {
        let response = common::TestRequest::post(path)
            .bearer(&wrong)
            .json(&json!({"list": "a@b.example"}))
            .send(&app)
            .await;
        common::assert_answer(&response, StatusCode::FORBIDDEN, "wrong-scope import");
        common::assert_error(
            &response,
            &format!("Insufficient permissions: {needed}."),
            "wrong-scope import",
        );
    }
    assert!(
        jobs(&pool, "imports.follows").await.is_empty()
            && jobs(&pool, "imports.blocks").await.is_empty()
            && jobs(&pool, "imports.mutes").await.is_empty(),
        "a gated request enqueues nothing"
    );
}

/// Pleroma's first scope alternative — the broad `follow` scope alone —
/// opens all three routes.
#[tokio::test]
async fn the_follow_scope_opens_every_import() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "follow").await;
    let app = common::test_app(pool.clone());

    for path in [FOLLOW, BLOCKS, MUTES] {
        let response = common::TestRequest::post(path)
            .bearer(&token)
            .json(&json!({"list": "Account address\nbob@remote.example\n"}))
            .send(&app)
            .await;
        assert_started(&response, path);
    }
    for kind in ["imports.follows", "imports.blocks", "imports.mutes"] {
        assert_eq!(jobs(&pool, kind).await.len(), 1, "`{kind}` was enqueued");
    }
}

/// With the limiter on, the default `imports` budget (one per hour) lets
/// a user's first import through and refuses the second with the
/// limiter's 429, on the same route and across the other two routes; the
/// refused request enqueues nothing. The window is an hour wide, so no
/// test waits for it.
#[tokio::test]
async fn a_second_import_inside_the_window_is_throttled() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "follow").await;
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("imports=1:3600"),
    );
    let body = json!({"list": "Account address\nbob@remote.example\n"});

    let first = common::TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&body)
        .send(&app)
        .await;
    assert_started(&first, "the first import");
    assert_eq!(first.header("x-ratelimit-limit"), Some("1"), "the budget");
    assert_eq!(
        first.header("x-ratelimit-remaining"),
        Some("0"),
        "the budget is spent"
    );

    for path in [FOLLOW, BLOCKS, MUTES] {
        let again = common::TestRequest::post(path)
            .bearer(&token)
            .json(&body)
            .send(&app)
            .await;
        common::assert_answer(&again, StatusCode::TOO_MANY_REQUESTS, path);
        common::assert_error(&again, "Throttled", path);
    }
    assert_eq!(
        jobs(&pool, "imports.follows").await.len(),
        1,
        "only the first import was enqueued"
    );
    assert!(
        jobs(&pool, "imports.blocks").await.is_empty()
            && jobs(&pool, "imports.mutes").await.is_empty(),
        "the throttled requests never reached a handler"
    );
}

/// The shipped default table carries the `imports` bucket: with the
/// limiter on and `RATE_LIMITS` unset, the second import is refused.
#[tokio::test]
async fn the_default_table_throttles_imports() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "follow").await;
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("search=30:60"),
    );
    let body = json!({"list": "bob@remote.example"});

    let first = common::TestRequest::post(MUTES)
        .bearer(&token)
        .json(&body)
        .send(&app)
        .await;
    assert_started(&first, "the first import");
    let second = common::TestRequest::post(MUTES)
        .bearer(&token)
        .json(&body)
        .send(&app)
        .await;
    common::assert_answer(&second, StatusCode::TOO_MANY_REQUESTS, "the second import");
}

/// The limiter is per user: another user's import is not affected by
/// alice having spent her budget.
#[tokio::test]
async fn the_import_budget_is_per_user() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, alice_token) = fixture(&pool, "follow").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let bob_token = common::create_app_and_token(&pool, &bob, "follow").await;
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("imports=1:3600"),
    );
    let body = json!({"list": "carol@remote.example"});

    for token in [&alice_token, &bob_token] {
        let response = common::TestRequest::post(BLOCKS)
            .bearer(token)
            .json(&body)
            .send(&app)
            .await;
        assert_started(&response, "each user's first import");
    }
}

/// With rate limits disabled (the test config's default) repeated imports
/// all succeed and carry no `x-ratelimit-*` headers.
#[tokio::test]
async fn imports_are_not_throttled_when_limits_are_disabled() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "follow").await;
    let app = common::test_app(pool.clone());

    for round in 0..2 {
        let response = common::TestRequest::post(FOLLOW)
            .bearer(&token)
            .json(&json!({"list": "bob@remote.example"}))
            .send(&app)
            .await;
        assert_started(&response, &format!("import {round}"));
        assert_eq!(
            response.header("x-ratelimit-limit"),
            None,
            "a disabled limiter adds no headers"
        );
    }
    assert_eq!(
        jobs(&pool, "imports.follows").await.len(),
        2,
        "both imports were enqueued"
    );
}
