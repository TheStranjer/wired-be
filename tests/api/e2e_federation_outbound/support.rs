//! Shared scaffolding for every submodule of the `e2e_federation_outbound`
//! area: the handles, media types, route roots and backoff consts every
//! scenario quotes, the fixtures that stand up a signing local account and
//! this instance's own actor, the thin wrappers that drive the peer's
//! `Follow` and this instance's writes over HTTP, and the readers of the
//! `federator.deliver` and `federator.publish` rows the scenarios assert.
//! Everything here is `pub(super)`; the scenario files hold scenarios
//! alone, so a helper even one of them shares lives here.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    federation::delivery::KIND_DELIVER,
    models::{
        job::Job,
        user::{NewUser, User},
    },
    services::instance_actor::ensure_instance_actor,
    state::AppState,
    util::password::hash_password,
};

use crate::{common, remote};

/// The mock's follower account, `bob@127.0.0.1:{port}`.
pub(super) const BOB: &str = "bob";
/// The mock's second account, `dave@127.0.0.1:{port}`.
pub(super) const DAVE: &str = "dave";
/// The `ActivityStreams` public address, as the note builder spells it.
pub(super) const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The scopes every local caller carries: `read write` covers each sub-scope
/// the posts, interactions, lists and reports in these scenarios ask for.
pub(super) const SCOPES: &str = "read write";
/// The media type a delivery carries and asks for.
pub(super) const ACTIVITY_JSON: &str = "application/activity+json";
/// The percent-encoded `🎉` of the reaction routes' path segment.
pub(super) const PARTY: &str = "%F0%9F%8E%89";
/// A `PNG`: the magic bytes the profile-image detector names. A profile
/// image is never decoded, so the header is the whole fixture.
pub(super) const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
/// The status write.
pub(super) const STATUSES: &str = "/api/v1/statuses";
/// The account routes the scenarios read and write.
pub(super) const ACCOUNTS: &str = "/api/v1/accounts";
/// The legacy follow write keyed by handle or actor URL.
pub(super) const FOLLOWS: &str = "/api/v1/follows";
/// The `backoff(1)` step the queue applies after a first failed attempt.
pub(super) const FIRST_BACKOFF_SECS: i64 = 15;

// --- the fixtures ----------------------------------------------------------------------

/// A local account named `nickname` that can sign its own deliveries, with a
/// bearer token granting [`SCOPES`].
pub(super) async fn local(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = remote::keyed_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, SCOPES).await;
    (created, token)
}

/// A key-carrying local account that can also authorise the move write: the
/// same fixture with a password hash on the row.
pub(super) async fn keyed_with_password(pool: &PgPool, nickname: &str, password: &str) -> User {
    let hash = hash_password(password).expect("the fixture password hashes");
    remote::keyed_local_user_with(pool, nickname, |user| NewUser {
        password_hash: Some(hash),
        ..user
    })
    .await
}

/// This instance's own actor: the signer of a forwarded `Flag`.
pub(super) async fn instance_actor(state: &AppState) -> User {
    ensure_instance_actor(&state.db, &common::test_config())
        .await
        .expect("the instance actor stands")
}

/// The followers-collection address a delivered document names for `actor`.
pub(super) fn followers_of(actor: &User) -> String {
    format!("{}/followers", actor.ap_id)
}

/// The mock's recorded path of the peer account `nick`'s personal inbox.
pub(super) fn personal(peer: &remote::MockPeer, nick: &str) -> String {
    peer.inbox_path(&peer.actor(nick))
}

// --- driving the peer and the queues -----------------------------------------------------

/// Delivers the mock's signed `Follow` of `target` by the peer account
/// `nick` through this instance's shared inbox and drains the fold plus the
/// `Accept` answer it queues.
pub(super) async fn peer_follows(
    app: &Router,
    state: &AppState,
    peer: &remote::MockPeer,
    nick: &str,
    target: &User,
) {
    let name = format!("{nick}-follows-{}", target.nickname);
    let follow = peer.follow_of(&name, &peer.actor(nick), &target.ap_id);
    let folded = peer
        .deliver(app, &peer.actor(nick), remote::INBOX, &follow)
        .await;
    assert_eq!(
        folded.status,
        StatusCode::ACCEPTED,
        "the inbox folds the mock's Follow"
    );
    let _drained = common::run_all_jobs(state).await;
}

/// The answer of `POST /api/v1/statuses` to `body`, asserted 200.
pub(super) async fn post(app: &Router, token: &str, body: Value) -> Value {
    let response = common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the status write");
    response.json_value()
}

/// `POST /api/v1/statuses` with `text` at `visibility` (`None` for the
/// default), answered as the created entity.
pub(super) async fn post_text(
    app: &Router,
    token: &str,
    text: &str,
    visibility: Option<&str>,
) -> Value {
    let mut body = json!({"status": text});
    if let Some(scope) = visibility {
        body["visibility"] = json!(scope);
    }
    post(app, token, body).await
}

/// The answer of `PUT /api/v1/statuses/{id}` editing to `status`, asserted 200.
pub(super) async fn edit(app: &Router, token: &str, id: &str, status: &str) -> Value {
    let response = common::TestRequest::put(&format!("{STATUSES}/{id}"))
        .bearer(token)
        .json(&json!({"status": status}))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the edit write");
    response.json_value()
}

/// `DELETE /api/v1/statuses/{id}`, asserted 200.
pub(super) async fn destroy(app: &Router, token: &str, id: &str) {
    let response = common::TestRequest::delete(&format!("{STATUSES}/{id}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the delete write");
}

/// `POST /api/v1/statuses/{id}/{verb}` (`reblog`, `unreblog`, `favourite`,
/// `unfavourite`), optionally with a JSON `body`; asserted 200.
pub(super) async fn status_action(
    app: &Router,
    token: &str,
    id: &str,
    verb: &str,
    body: Option<Value>,
) -> Value {
    let request = common::TestRequest::post(&format!("{STATUSES}/{id}/{verb}")).bearer(token);
    let request = match body {
        Some(found) => request.json(&found),
        None => request,
    };
    let response = request.send(app).await;
    common::assert_answer(&response, StatusCode::OK, &format!("the {verb} write"));
    response.json_value()
}

/// The action `verb` (`PUT` or `DELETE`) of Pleroma's reaction route for
/// `emoji` on status `id`, asserted 200.
pub(super) async fn reaction_write(app: &Router, token: &str, id: &str, verb: &str, emoji: &str) {
    let path = format!("/api/v1/pleroma/statuses/{id}/reactions/{emoji}");
    let request = match verb {
        "DELETE" => common::TestRequest::delete(&path),
        _ => common::TestRequest::put(&path),
    };
    let response = request.bearer(token).send(app).await;
    common::assert_answer(&response, StatusCode::OK, &format!("the reaction {verb}"));
}

/// `POST /api/v1/follows` of `uri`, asserted 200 and answered with the
/// target's `Account` entity.
pub(super) async fn follow_by_uri(app: &Router, token: &str, uri: &str) -> Value {
    let response = common::TestRequest::post(FOLLOWS)
        .bearer(token)
        .json(&json!({"uri": uri}))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the follow write");
    response.json_value()
}

/// `GET` of `path` as `token`, asserted 200 and parsed.
pub(super) async fn get_json(app: &Router, token: &str, path: &str) -> Value {
    let response = common::TestRequest::get(path).bearer(token).send(app).await;
    common::assert_answer(&response, StatusCode::OK, &format!("the read of `{path}`"));
    response.json_value()
}

/// `GET /api/v1/accounts/lookup` for `acct`, asserted 200 and parsed.
pub(super) async fn lookup(app: &Router, acct: &str) -> Value {
    let response = common::TestRequest::get(&format!("{ACCOUNTS}/lookup?acct={acct}"))
        .send(app)
        .await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        &format!("the lookup of `{acct}`"),
    );
    response.json_value()
}

// --- reading the delivery queue -----------------------------------------------------------

/// The one `federator.deliver` job queued for `activity_id`: the row whose
/// `state`, `attempts` and `last_error` a retry scenario reads.
///
/// # Panics
///
/// If no delivery job names that activity, or the read fails.
pub(super) async fn delivery_job(pool: &PgPool, activity_id: &str) -> Job {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 AND args->>'activity_id' = $2")
        .bind(KIND_DELIVER)
        .bind(activity_id)
        .fetch_one(pool)
        .await
        .expect("the delivery job reads")
}

/// The whole seconds `job` sits out before its next attempt, measured
/// against the statement that scheduled it: `run_at - updated_at` is the
/// delay the queue wrote, whatever the clock now says.
///
/// # Panics
///
/// If the read fails.
pub(super) async fn seconds_until_due(pool: &PgPool, job: &Job) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT EXTRACT(EPOCH FROM (run_at - updated_at))::bigint FROM jobs WHERE id = $1",
    )
    .bind(job.id)
    .fetch_one(pool)
    .await
    .expect("the due-difference read runs")
}

/// Makes every scheduled `federator.deliver` retry due at once: the test's
/// stand-in for time passing, without waiting for any of it.
pub(super) async fn make_retries_due(pool: &PgPool) {
    let _hit = sqlx::query("UPDATE jobs SET run_at = now() - interval '1 minute' WHERE kind = $1")
        .bind(KIND_DELIVER)
        .execute(pool)
        .await
        .expect("the backdating runs");
}

/// How many `federator.publish` jobs the queue holds under `unique_key`,
/// whatever their state: the debounce assertions count rows, not attempts.
///
/// # Panics
///
/// If the count read fails.
pub(super) async fn publishes_with_key(pool: &PgPool, unique_key: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM jobs WHERE kind = 'federator.publish' AND unique_key = $1",
    )
    .bind(unique_key)
    .fetch_one(pool)
    .await
    .expect("the publish count reads")
}
