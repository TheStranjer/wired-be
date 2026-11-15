//! Signed event deliveries over HTTP (`src/services/webhooks.rs`, WBE-212):
//! the events the real endpoints fire, the staff letter a filed report writes,
//! the admin state change that announces `report.updated`, and one delivery
//! performed end to end against a mock endpoint.
//!
//! The fan-out and the signing are unit-tested in the service; what these
//! tests pin is that the routes reach it. Filing a report
//! (`POST /api/v1/reports`) queues one `webhooks.deliver` job per subscribed
//! enabled webhook, carrying the Mastodon report entity as `data.report`, and
//! — with a relay configured — one `mailer.send` letter per mailable staff
//! account, the reporter's own excluded. Registering an account
//! (`POST /api/v1/accounts`) queues the same shape with `data.account`, the
//! admin render of the new account. The admin's
//! `PATCH /api/v1/pleroma/admin/reports` moves every report it names, logs the
//! `report_update` entry and queues `report.updated`; it answers Pleroma's 204
//! when all of them moved and Pleroma's 400 `{"id": …, "error": …}` list for
//! the elements that moved nothing — a state off `open`/`closed`/`resolved` or
//! an unknown report — while the reports that did move stay moved. Being the
//! admin API, no token is 401, a moderator is 403 "User is not an admin." and
//! an admin without the write scope is 403 on insufficient permissions. And a
//! queued job really delivers: the mock endpoint sees the four-key envelope,
//! the three `X-Wired-*` headers, and an HMAC-SHA256 over
//! `"{timestamp}.{body}"` that its own recomputation from the stored secret
//! accepts.

#![cfg(test)]

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use hmac::{Hmac, KeyInit as _, Mac as _};
use serde_json::{Value, json};
use sha2_011::Sha256;
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};
use uuid::Uuid;
use wired_be::{
    models::{
        job::Job,
        report,
        user::{Flag, NewUser, User},
        webhook,
    },
    services::webhooks,
};

use crate::common;

/// The report filing route every report test posts to.
const REPORTS: &str = "/api/v1/reports";
/// The admin route that moves a report through moderation.
const ADMIN_REPORTS: &str = "/api/v1/pleroma/admin/reports";
/// The comment every fixture filing writes, so a payload is recognisable.
const COMMENT: &str = "spam again";
/// The password the registration test submits (over the 8-character minimum).
const PASSWORD: &str = "correct horse battery staple";

/// A local user named `nickname` with a `write:reports` token.
async fn reporter(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, "write:reports").await;
    (created, token)
}

/// A mailable admin named `nickname` with a token granting `scopes`.
async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        email: Some(format!("{nickname}@example.org")),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A mailable moderator named `nickname` with a token granting `scopes`.
async fn moderator(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_moderator: Flag::new(true),
        email: Some(format!("{nickname}@example.org")),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A subscribed, enabled webhook for `event` pointing at `url`.
async fn subscribed(pool: &PgPool, event: &str, url: &str) -> webhook::Webhook {
    webhook::create(pool, url, &[event.to_owned()], true)
        .await
        .expect("the webhook fixture inserts")
}

/// Every delivery job on the queue, oldest first.
async fn deliveries(pool: &PgPool) -> Vec<Job> {
    jobs(pool, webhooks::KIND_DELIVER).await
}

/// The queued jobs that announce `event`.
fn announcing<'a>(queued: &'a [Job], event: &str) -> Vec<&'a Job> {
    queued
        .iter()
        .filter(|job| job.args["event"] == event)
        .collect()
}

/// A filing over HTTP, asserted 200 and answered as JSON.
async fn filed(app: &Router, token: &str, target: &User) -> Value {
    let response = common::TestRequest::post(REPORTS)
        .bearer(token)
        .json(&json!({"account_id": target.id.to_string(), "comment": COMMENT}))
        .send(app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the report files");
    response.json_value()
}

/// `PATCH /api/v1/pleroma/admin/reports` as `token`.
async fn move_reports(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::patch(ADMIN_REPORTS)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// Filing a report queues the `report.created` event once per subscribed
/// enabled webhook, with the report entity as its payload and the ticket's
/// queue, budget and dedup key on the job.
#[tokio::test]
async fn filing_a_report_queues_the_report_created_event() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (who, token) = reporter(&pool, "whfile").await;
    let target = common::create_local_user(&pool, "whfile-target").await;
    let hook = subscribed(
        &pool,
        webhooks::EVENT_REPORT_CREATED,
        "https://example.org/h",
    )
    .await;
    let _other_event = subscribed(
        &pool,
        webhooks::EVENT_ACCOUNT_CREATED,
        "https://example.org/o",
    )
    .await;
    let _disabled = {
        let created = subscribed(
            &pool,
            webhooks::EVENT_REPORT_CREATED,
            "https://example.org/off",
        )
        .await;
        webhook::set_enabled(&pool, created.id, false)
            .await
            .expect("the disable runs")
            .expect("the webhook exists")
    };

    let entity = filed(&app, &token, &target).await;

    let queued = deliveries(&pool).await;
    assert_eq!(
        queued.len(),
        1,
        "one job, for the one subscribed enabled webhook"
    );
    let job = queued.first().expect("the job");
    assert_eq!(job.state, "available", "waiting for a worker");
    assert_eq!(job.queue, "default", "on the default queue");
    assert_eq!(job.max_attempts, webhooks::MAX_ATTEMPTS, "five tries");
    assert_eq!(job.kind, webhooks::KIND_DELIVER, "the delivery kind");
    assert_eq!(job.args["webhook_id"], hook.id.to_string());
    assert_eq!(job.args["event"], webhooks::EVENT_REPORT_CREATED);
    let event_id = Uuid::parse_str(job.args["event_id"].as_str().expect("an event id"))
        .expect("the event id is a decision 4 id");
    assert_eq!(
        job.unique_key.as_deref(),
        Some(format!("webhook:{}:{}", hook.id, event_id).as_str()),
        "keyed webhook:<webhook id>:<event id>"
    );
    assert_eq!(
        job.args["data"]["report"]["id"], entity["id"],
        "the payload is the report the endpoint answered"
    );
    assert_eq!(job.args["data"]["report"]["comment"], COMMENT);
    assert_eq!(
        job.args["data"]["report"]["target_account"]["id"],
        target.id.to_string(),
        "about the reported account"
    );
    assert_eq!(who.nickname, "whfile", "the fixture's own reporter");
}

/// The same filing writes Pleroma's admin report letter to every mailable
/// staff account, and not to the staffer who filed the report.
#[tokio::test]
async fn filing_a_report_letters_the_staff() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.smtp_host = Some("smtp.example.org".to_owned());
    let app = common::test_app_with_config(pool.clone(), config);
    let _root = admin(&pool, "whroot", "read").await;
    let (modder, mod_token) = moderator(&pool, "whmod", "write:reports").await;
    let (_, plain_token) = reporter(&pool, "whplain").await;
    let target = common::create_local_user(&pool, "whspammer").await;
    let _silent_mod = common::create_local_user_with(&pool, "whsilent", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;

    // A moderator files: the admin is written to, the reporter is not, and the
    // moderator without an address cannot be.
    let _first = filed(&app, &mod_token, &target).await;
    let letters = jobs(&pool, wired_be::mailer::KIND_SEND).await;
    assert_eq!(letters.len(), 1, "one letter, to the admin alone");
    let letter = letters.first().expect("the letter job");
    assert_eq!(letter.args["to"], "whroot@example.org", "to the admin");
    assert_eq!(
        letter.args["subject"], "Wired Report",
        "the letter's subject"
    );
    assert!(
        letter.args["text_body"]
            .as_str()
            .is_some_and(|body| body.contains("@whmod")
                && body.contains("@whspammer")
                && body.contains(COMMENT)),
        "the letter says who reported whom: {}",
        letter.args["text_body"]
    );

    // A member's filing reaches both mailable staffers.
    let _second = filed(&app, &plain_token, &target).await;
    let more = jobs(&pool, wired_be::mailer::KIND_SEND).await;
    assert_eq!(more.len(), 3, "two more letters, one per staffer");
    let recipients: Vec<&str> = more
        .iter()
        .map(|job| {
            job.args["to"]
                .as_str()
                .expect("every letter names its recipient")
        })
        .collect();
    assert!(
        recipients.contains(&"whroot@example.org") && recipients.contains(&"whmod@example.org"),
        "both mailable staff accounts are written to: {recipients:?}"
    );
    assert!(
        !recipients.contains(&"whsilent@example.org"),
        "an addressless staffer never is: {recipients:?}"
    );
    assert!(
        !recipients.contains(&"whplain@example.org"),
        "and a plain member is no staff account: {recipients:?}"
    );
    assert_eq!(modder.nickname, "whmod", "the fixture's own moderator");
}

/// With no relay configured the same filing queues no letter at all — the
/// suite's default config is the mail-disabled one the fan-out reads.
#[tokio::test]
async fn a_disabled_relay_queues_no_staff_letter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = reporter(&pool, "whnomail").await;
    let target = common::create_local_user(&pool, "whnomail-target").await;
    let _root = admin(&pool, "whroot2", "read").await;

    let _filed = filed(&app, &token, &target).await;

    assert!(
        jobs(&pool, wired_be::mailer::KIND_SEND).await.is_empty(),
        "SMTP_HOST unset means no letter is queued"
    );
}

/// Registering an account queues `account.created` with the admin render of
/// the new account.
#[tokio::test]
async fn registering_an_account_queues_the_account_created_event() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let hook = subscribed(
        &pool,
        webhooks::EVENT_ACCOUNT_CREATED,
        "https://example.org/new",
    )
    .await;
    let token = common::create_app_only_token(&pool, "read write").await;

    let response = common::TestRequest::post("/api/v1/accounts")
        .bearer(&token)
        .json(&json!({
            "username": "whjoined",
            "email": "whjoined@example.org",
            "password": PASSWORD,
            "agreement": true,
        }))
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the account registers");

    let queued = deliveries(&pool).await;
    assert_eq!(queued.len(), 1, "the one subscriber got one job");
    let job = queued.first().expect("the job");
    assert_eq!(job.args["webhook_id"], hook.id.to_string());
    assert_eq!(job.args["event"], webhooks::EVENT_ACCOUNT_CREATED);
    assert_eq!(job.args["data"]["account"]["nickname"], "whjoined");
    assert_eq!(
        job.args["data"]["account"]["email"], "whjoined@example.org",
        "the admin render the event carries"
    );
    assert_eq!(job.args["data"]["account"]["roles"]["admin"], false);
}

/// A report's state change through the admin API queues `report.updated`,
/// answers Pleroma's 204, moves the row and logs the moderation entry.
#[tokio::test]
async fn an_admin_state_change_queues_the_report_updated_event() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = reporter(&pool, "whmove").await;
    let (_, admin_token) = admin(&pool, "whmove-boss", "admin:write:reports").await;
    let target = common::create_local_user(&pool, "whmove-target").await;
    let hook = subscribed(
        &pool,
        webhooks::EVENT_REPORT_UPDATED,
        "https://example.org/u",
    )
    .await;
    let entity = filed(&app, &token, &target).await;
    assert_eq!(
        deliveries(&pool).await.len(),
        0,
        "no subscriber takes report.created here"
    );

    let response = move_reports(
        &app,
        &admin_token,
        json!({"reports": [{"id": entity["id"], "state": "resolved"}]}),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::NO_CONTENT,
        "Pleroma's `update` answers 204"
    );

    let queued = deliveries(&pool).await;
    let updated = announcing(&queued, webhooks::EVENT_REPORT_UPDATED);
    assert_eq!(updated.len(), 1, "the state change announced itself once");
    let job = updated.first().expect("the job");
    assert_eq!(job.args["webhook_id"], hook.id.to_string());
    assert_eq!(job.args["data"]["report"]["id"], entity["id"]);
    assert_eq!(
        job.args["data"]["report"]["action_taken"], true,
        "resolution is an action taken"
    );
    assert!(
        job.args["data"]["report"]["action_taken_at"].is_string(),
        "and it is stamped"
    );

    let stored = report::get(&pool, report_id(&entity))
        .await
        .expect("the read runs")
        .expect("the report exists");
    assert_eq!(stored.state, report::ReportState::Resolved, "the row moved");
    assert_eq!(logged_actions(&pool, "report_update").await, 1);
}

/// The bulk form moves every report the body names, each announcing itself.
#[tokio::test]
async fn a_state_change_moves_every_report_it_names() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = reporter(&pool, "whbulk").await;
    let (_, admin_token) = admin(&pool, "whbulk-boss", "admin:write").await;
    let target = common::create_local_user(&pool, "whbulk-target").await;
    let _hook = subscribed(
        &pool,
        webhooks::EVENT_REPORT_UPDATED,
        "https://example.org/b",
    )
    .await;
    let one = filed(&app, &token, &target).await;
    let two = filed(&app, &token, &target).await;

    let response = move_reports(
        &app,
        &admin_token,
        json!({"reports": [
            {"id": one["id"], "state": "closed"},
            {"id": two["id"], "state": "open"},
        ]}),
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT, "both moved");

    let queued = deliveries(&pool).await;
    let updated = announcing(&queued, webhooks::EVENT_REPORT_UPDATED);
    assert_eq!(updated.len(), 2, "one announcement per moved report");
    let ids: Vec<&str> = updated
        .iter()
        .map(|job| {
            job.args["data"]["report"]["id"]
                .as_str()
                .expect("the payload names its report")
        })
        .collect();
    assert!(
        ids.contains(&one["id"].as_str().expect("an id"))
            && ids.contains(&two["id"].as_str().expect("an id")),
        "both reports are named: {ids:?}"
    );
    for job in updated {
        assert!(
            job.unique_key.is_some(),
            "every delivery job carries its dedup key"
        );
    }
}

/// Pleroma's 400 names every element that moved nothing — an unsupported
/// state, an unknown report, an id that is no id — and the mixed request still
/// moves and announces the report that did.
#[tokio::test]
async fn a_refused_state_change_names_every_element_that_did_not_move() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = reporter(&pool, "whrefused").await;
    let (_, admin_token) = admin(&pool, "whrefused-boss", "admin:write:reports").await;
    let target = common::create_local_user(&pool, "whrefused-target").await;
    let _hook = subscribed(
        &pool,
        webhooks::EVENT_REPORT_UPDATED,
        "https://example.org/r",
    )
    .await;
    let entity = filed(&app, &token, &target).await;

    let response = move_reports(
        &app,
        &admin_token,
        json!({"reports": [
            {"id": entity["id"], "state": "deleted"},
            {"id": "01934567-89ab-7cde-f012-3456789abcde", "state": "closed"},
            {"id": "nonsense", "state": "closed"},
        ]}),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "Pleroma's 400");
    assert_eq!(
        response.json_value(),
        json!([
            {"id": entity["id"], "error": "Unsupported state"},
            {"id": "01934567-89ab-7cde-f012-3456789abcde", "error": "not_found"},
            {"id": "nonsense", "error": "not_found"},
        ]),
        "the failure list repeats each id as it was sent"
    );
    assert!(
        announcing(&deliveries(&pool).await, webhooks::EVENT_REPORT_UPDATED).is_empty(),
        "nothing moved, so nothing was announced"
    );

    let mixed = move_reports(
        &app,
        &admin_token,
        json!({"reports": [
            {"id": "nonsense", "state": "closed"},
            {"id": entity["id"], "state": "closed"},
        ]}),
    )
    .await;
    assert_eq!(
        mixed.status,
        StatusCode::BAD_REQUEST,
        "one failure still answers 400"
    );
    assert_eq!(
        announcing(&deliveries(&pool).await, webhooks::EVENT_REPORT_UPDATED).len(),
        1,
        "and the report that did move is still announced"
    );
    let moved = report::get(&pool, report_id(&entity))
        .await
        .expect("the read runs")
        .expect("the report exists");
    assert_eq!(moved.state, report::ReportState::Closed, "it moved");
}

/// An empty list moves nothing and answers the 204 Pleroma's `Enum.map` over
/// no reports gives.
#[tokio::test]
async fn an_empty_request_moves_and_announces_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, admin_token) = admin(&pool, "whempty-boss", "admin:write:reports").await;
    let _hook = subscribed(
        &pool,
        webhooks::EVENT_REPORT_UPDATED,
        "https://example.org/e",
    )
    .await;

    let response = move_reports(&app, &admin_token, json!({"reports": []})).await;

    assert_eq!(response.status, StatusCode::NO_CONTENT, "nothing to move");
    assert!(
        deliveries(&pool).await.is_empty(),
        "and nothing to announce"
    );
}

/// The gate: no token is 401 "Invalid credentials", a moderator is 403 "User
/// is not an admin.", an admin holding only the read scope is 403 on
/// insufficient permissions, and no refusal moved or queued anything.
#[tokio::test]
async fn the_admin_state_change_demands_an_admin_write_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (who, token) = reporter(&pool, "whgate").await;
    let target = common::create_local_user(&pool, "whgate-target").await;
    let entity = filed(&app, &token, &target).await;
    let (_, mod_token) = moderator(&pool, "whgate-mod", "admin:write:reports").await;
    let (_, read_token) = admin(&pool, "whgate-read", "admin:read:reports").await;
    let body = json!({"reports": [{"id": entity["id"], "state": "closed"}]});

    let anonymous = common::TestRequest::patch(ADMIN_REPORTS)
        .json(&body)
        .send(&app)
        .await;
    let as_moderator = move_reports(&app, &mod_token, body.clone()).await;
    let as_reader = move_reports(&app, &read_token, body).await;

    common::assert_error(&anonymous, "Invalid credentials", "no token");
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "401");
    common::assert_answer(&as_moderator, StatusCode::FORBIDDEN, "a moderator");
    assert_eq!(
        as_moderator.json_value()["error"],
        "User is not an admin.",
        "Pleroma's admin-plug wording: {}",
        as_moderator.json_value()
    );
    common::assert_answer(&as_reader, StatusCode::FORBIDDEN, "a read-only admin");
    assert!(
        deliveries(&pool).await.is_empty(),
        "a refused state change queues nothing"
    );
    let stored = report::get(&pool, report_id(&entity))
        .await
        .expect("the read runs")
        .expect("the report exists");
    assert_eq!(
        stored.state,
        report::ReportState::Open,
        "and nothing moved: {who:?} filed it"
    );
}

/// One delivery performed: the job a real filing queued runs to a mock
/// endpoint that sees the envelope, the three event headers and a signature its
/// own recomputation from the stored secret verifies.
#[tokio::test]
async fn a_queued_delivery_arrives_signed_at_the_endpoint() {
    let (url, seen, server) = mock_endpoint().await;
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let config = common::test_config();
    let app = common::test_app_with_config(pool.clone(), config.clone());
    let state = common::test_app_state(pool.clone(), config);
    let hook = subscribed(&pool, webhooks::EVENT_REPORT_CREATED, &url).await;
    let (who, token) = reporter(&pool, "whe2e").await;
    let target = common::create_local_user(&pool, "whe2e-target").await;

    let entity = filed(&app, &token, &target).await;
    let run = common::run_all_jobs(&state).await;
    server.abort();
    assert!(run >= 1, "the queued delivery ran, jobs run: {run}");

    let queued = deliveries(&pool).await;
    let job = queued.first().expect("the delivery job");
    assert_eq!(
        job.state, "completed",
        "the endpoint's 202 completed the job"
    );

    let headers = header_map(&seen);
    let body = captured_body(&seen);
    assert_eq!(
        headers.get("content-type").map(String::as_str),
        Some("application/json"),
        "the body is labelled"
    );
    assert_eq!(
        headers.get("x-wired-event").map(String::as_str),
        Some(webhooks::EVENT_REPORT_CREATED),
        "the event is named"
    );
    assert!(
        headers
            .get("user-agent")
            .is_some_and(|agent| agent.starts_with("Wired (")),
        "the instance identifies itself: {:?}",
        headers.get("user-agent")
    );
    let envelope: Value = serde_json::from_slice(&body).expect("the endpoint received JSON");
    assert_eq!(envelope["id"], job.args["event_id"], "the event id");
    assert_eq!(envelope["type"], webhooks::EVENT_REPORT_CREATED);
    assert_eq!(envelope["data"]["report"]["id"], entity["id"]);
    assert_eq!(envelope["data"]["report"]["comment"], COMMENT);
    assert!(
        envelope["created_at"]
            .as_str()
            .is_some_and(|stamp| stamp.ends_with('Z')),
        "an RFC 3339 stamp: {}",
        envelope["created_at"]
    );
    let stamp = headers
        .get("x-wired-timestamp")
        .cloned()
        .expect("the delivery carries its timestamp");
    assert!(
        stamp.parse::<i64>().is_ok(),
        "the stamp is unix seconds: {stamp}"
    );
    assert_eq!(
        headers.get("x-wired-signature").cloned(),
        Some(receiver_signature(&hook.secret, &stamp, &body)),
        "the digest verifies over exactly what the endpoint received"
    );
    assert_eq!(
        hook.events,
        vec![webhooks::EVENT_REPORT_CREATED.to_owned()],
        "the subscriber's own list, and @{} filed the report it was told about",
        who.nickname
    );
}

/// The report id an entity answered with.
fn report_id(entity: &Value) -> Uuid {
    let text = entity["id"].as_str().expect("the entity carries its id");
    Uuid::parse_str(text).expect("the entity id is an id")
}

/// Every moderation-log entry recorded with `action`.
async fn logged_actions(pool: &PgPool, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM moderation_log WHERE action = $1")
        .bind(action)
        .fetch_one(pool)
        .await
        .expect("the log read runs")
}

/// Every queued job of `kind`, oldest first.
async fn jobs(pool: &PgPool, kind: &str) -> Vec<Job> {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(kind)
        .fetch_all(pool)
        .await
        .expect("the job read runs")
}

/// What one mock request looked like.
#[derive(Clone, Debug)]
struct Captured {
    /// The request headers, lowercased.
    headers: Vec<(String, String)>,
    /// The raw body the signature covered.
    body: Vec<u8>,
}

/// The capture every mock endpoint writes into.
type Seen = Arc<Mutex<Vec<Captured>>>;

/// The mock endpoint's handler: record the request, answer 202.
async fn catch_request(State(seen): State<Seen>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let pairs = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    seen.lock().expect("an unpoisoned lock").push(Captured {
        headers: pairs,
        body: body.to_vec(),
    });
    StatusCode::ACCEPTED
}

/// A mock webhook endpoint answering 202 on a fresh loopback port:
/// `(url, capture, server)`.
async fn mock_endpoint() -> (String, Seen, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port to serve on");
    let url = format!(
        "http://{}/hook",
        listener.local_addr().expect("the bound address")
    );
    let seen: Seen = Arc::default();
    let app = Router::new()
        .route("/hook", post(catch_request))
        .with_state(Arc::clone(&seen));
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("test webhook endpoint stopped: {err}");
        }
    });
    (url, seen, server)
}

/// The headers of the mock's last request, by lowercased name.
fn header_map(seen: &Seen) -> std::collections::HashMap<String, String> {
    let last = seen
        .lock()
        .expect("an unpoisoned lock")
        .last()
        .cloned()
        .expect("the mock served a request");
    last.headers.into_iter().collect()
}

/// The mock's last request body, verbatim.
fn captured_body(seen: &Seen) -> Vec<u8> {
    let last = seen
        .lock()
        .expect("an unpoisoned lock")
        .last()
        .cloned()
        .expect("the mock served a request");
    last.body
}

/// A receiver's own HMAC-SHA256 over `"{timestamp}.{body}"`, in the header's
/// `sha256=<hex>` spelling.
fn receiver_signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("a secret of any length signs");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    format!(
        "sha256={}",
        wired_be::util::token::to_hex(&mac.finalize().into_bytes())
    )
}
