//! Shared consts and helpers for this area's poll-route submodules: the
//! `GET /api/v1/polls/{id}` and `POST /api/v1/polls/{id}/votes` wrappers,
//! the create-through-poll fixture, the 422 bodies the ticket pins and the
//! model-level reads the vote tests check. Everything here is
//! `pub(super)`, and a submodule imports exactly the members it calls.

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::poll::{self, Poll};

use super::support::accepted;
use crate::common;

/// The path prefix both poll routes mount under.
pub(super) const POLLS: &str = "/api/v1/polls";
/// The shared 404 body of both routes.
pub(super) const NOT_FOUND: &str = "Record not found";
/// The 422 body for a poll whose deadline has passed.
pub(super) const FINISHED: &str = "Poll's finished";
/// The 422 body for a voter who already marked the poll.
pub(super) const SECOND_BALLOT: &str = "You have already voted on this poll";
/// The 422 body for the author of the poll's status.
pub(super) const AUTHOR_REFUSAL: &str = "Poll's author can't vote";
/// The 422 body for a vote body that carries no choices.
pub(super) const NO_CHOICES: &str = "Missing parameter: choices";
/// The 422 body for an index no option of the poll holds.
pub(super) const BAD_INDEX: &str = "Invalid indices";
/// The 422 body for a second distinct choice on a single-choice poll.
pub(super) const SECOND_MARK: &str = "Too many choices";
/// The 403 body for an app-only token at a user endpoint.
pub(super) const NO_USER: &str = "This API requires an authenticated user";

/// The poll `poll_id` names over `GET /api/v1/polls/{id}`, `token` the
/// bearer it presents (`None`: anonymously).
pub(super) async fn read_poll(
    app: &Router,
    poll_id: &str,
    token: Option<&str>,
) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("{POLLS}/{poll_id}"));
    let request = match token {
        Some(token) => request.bearer(token),
        None => request,
    };
    request.send(app).await
}

/// `POST /api/v1/polls/{poll_id}/votes` as `token` with the JSON `body`.
pub(super) async fn vote_on(
    app: &Router,
    token: &str,
    poll_id: &str,
    body: Value,
) -> common::TestResponse {
    common::TestRequest::post(&format!("{POLLS}/{poll_id}/votes"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// A vote that carries exactly the `choices` array.
pub(super) async fn marks(
    app: &Router,
    token: &str,
    poll_id: &str,
    choices: Value,
) -> common::TestResponse {
    vote_on(app, token, poll_id, json!({ "choices": choices })).await
}

/// A public poll of `["tea", "coffee"]` posted through the create —
/// `extra` merged into the `poll` parameters — answered as the created
/// status's answer, returning the fresh poll's id.
pub(super) async fn ballot(app: &Router, token: &str, extra: Value) -> String {
    let mut poll = json!({ "options": ["tea", "coffee"] });
    if let (Some(base), Some(extra)) = (poll.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            let _over_wins: Option<Value> = base.insert(key.clone(), value.clone());
        }
    }
    let value = accepted(
        app,
        token,
        json!({ "status": "cast a ballot", "poll": poll }),
    )
    .await;
    value["poll"]["id"]
        .as_str()
        .expect("a poll create answers the poll entity")
        .to_owned()
}

/// A vote asserted 422 with exactly the message `refusal.0` as its
/// `{"error": …}` body, for the case named by `refusal.1`.
pub(super) async fn refused_vote(
    app: &Router,
    token: &str,
    poll_id: &str,
    body: Value,
    refusal: (&str, &str),
) {
    let response = vote_on(app, token, poll_id, body).await;
    common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, refusal.1);
    common::assert_error(&response, refusal.0, refusal.1);
}

/// The stored `polls` row `poll_id` names, demanded.
pub(super) async fn poll_row(pool: &PgPool, poll_id: &str) -> Poll {
    poll::get_by_id(pool, parse_marked(poll_id))
        .await
        .expect("the poll reads")
        .expect("the poll is stored")
}

/// The answer's poll id text as a [`Uuid`].
fn parse_marked(text: &str) -> Uuid {
    Uuid::parse_str(text).expect("the answer carries the poll's id text")
}

/// Moves the poll's deadline an hour into the past: the state a finished
/// poll is in, which no create body can post (the window floor is five
/// minutes out).
pub(super) async fn close_poll(pool: &PgPool, poll_id: &str) {
    let moved =
        sqlx::query("UPDATE polls SET expires_at = now() - interval '1 hour' WHERE id = $1")
            .bind(parse_marked(poll_id))
            .execute(pool)
            .await
            .expect("the deadline moves");
    assert_eq!(moved.rows_affected(), 1, "the fixture poll is stored");
}

/// How many marks `poll_id`'s `poll_votes` rows hold — what a refusal
/// never added to.
pub(super) async fn mark_rows(pool: &PgPool, poll_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM poll_votes WHERE poll_id = $1")
        .bind(parse_marked(poll_id))
        .fetch_one(pool)
        .await
        .expect("the marks count reads")
}
