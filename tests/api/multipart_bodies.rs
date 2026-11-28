//! `multipart/form-data` bodies on endpoints that take `ApiBody` /
//! `ApiParams` (WBE-907): `POST /api/v1/statuses` and
//! `POST /api/v1/accounts/{id}/follow` answer a `FormData` request like the
//! JSON one, text parts nesting by the form key rules, while a file part on
//! these endpoints is the 422 naming it.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use bytes::Bytes;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow,
        media_attachment::{self, NewMediaAttachment},
        user::User,
    },
};

use crate::common::{self, MultipartPart};

/// Boundary the test bodies are framed with.
const BOUNDARY: &str = "wired-multipart-bodies";
/// The statuses route.
const STATUSES: &str = "/api/v1/statuses";

/// A text part named `name` holding `value`, which need not be static.
fn text(name: &'static str, value: &str) -> MultipartPart {
    MultipartPart {
        name,
        filename: None,
        content_type: None,
        bytes: Bytes::from(value.to_owned()),
    }
}

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// An unposted upload by `author`, a `media_ids` entry a create may claim.
async fn upload(pool: &PgPool, author: &User) -> Uuid {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            status_id: None,
            url: format!("{TEST_BASE_URL}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("the fixture upload inserts")
    .id
}

/// POSTs the multipart `parts` to the statuses route as `token`.
async fn post_multipart(
    app: &Router,
    token: &str,
    parts: &[MultipartPart],
) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .bearer(token)
        .multipart(BOUNDARY, parts)
        .send(app)
        .await
}

/// POSTs the JSON `body` to the statuses route as `token`.
async fn post_json(app: &Router, token: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// A multipart create with a poll answers the entity the JSON create does:
/// `poll[options][]` and `poll[expires_in]` nest like the form keys.
#[tokio::test]
async fn a_multipart_status_with_a_poll_matches_the_json_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "write:statuses").await;
    let app = common::test_app(pool.clone());

    let multipart = post_multipart(
        &app,
        &token,
        &[
            text("status", "ballot"),
            text("visibility", "unlisted"),
            text("poll[options][]", "yes"),
            text("poll[options][]", "no"),
            text("poll[expires_in]", "3600"),
        ],
    )
    .await;
    common::assert_answer(&multipart, StatusCode::OK, "the multipart poll create");
    let json_call = post_json(
        &app,
        &token,
        &json!({
            "status": "ballot",
            "visibility": "unlisted",
            "poll": {"options": ["yes", "no"], "expires_in": 3600},
        }),
    )
    .await;
    common::assert_answer(&json_call, StatusCode::OK, "the JSON poll create");

    let (form, plain) = (multipart.json_value(), json_call.json_value());
    for field in ["content", "visibility"] {
        assert_eq!(form[field], plain[field], "`{field}` is the same");
    }
    assert_eq!(form["visibility"], "unlisted", "the part reached the post");
    for field in ["options", "multiple", "expired", "votes_count"] {
        assert_eq!(
            form["poll"][field], plain["poll"][field],
            "the poll's `{field}` is the same"
        );
    }
    assert_eq!(
        form["poll"]["options"],
        json!([
            {"title": "yes", "votes_count": 0},
            {"title": "no", "votes_count": 0},
        ]),
        "the options arrived in order"
    );
}

/// A multipart create with `media_ids[]` claims the upload the way the JSON
/// create does.
#[tokio::test]
async fn a_multipart_status_with_media_ids_claims_the_upload() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:statuses").await;
    let app = common::test_app(pool.clone());
    let first = upload(&pool, &alice).await;
    let second = upload(&pool, &alice).await;

    let multipart = post_multipart(
        &app,
        &token,
        &[
            text("status", "pictured"),
            text("visibility", "unlisted"),
            text("media_ids[]", &first.to_string()),
        ],
    )
    .await;
    common::assert_answer(&multipart, StatusCode::OK, "the multipart media create");
    let json_call = post_json(
        &app,
        &token,
        &json!({
            "status": "pictured",
            "visibility": "unlisted",
            "media_ids": [second.to_string()],
        }),
    )
    .await;
    common::assert_answer(&json_call, StatusCode::OK, "the JSON media create");

    let (form, plain) = (multipart.json_value(), json_call.json_value());
    assert_eq!(form["content"], plain["content"], "the same text");
    assert_eq!(form["visibility"], "unlisted", "the visibility part landed");
    assert_eq!(
        form["media_attachments"].as_array().map(Vec::len),
        plain["media_attachments"].as_array().map(Vec::len),
        "one attachment each"
    );
    assert_eq!(
        form["media_attachments"][0]["id"],
        json!(first.to_string()),
        "the multipart create claimed the upload it named"
    );
}

/// A file part on a `ApiBody` endpoint is refused with a 422 naming it, and
/// nothing is posted.
#[tokio::test]
async fn a_file_part_on_the_statuses_route_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "write:statuses").await;
    let app = common::test_app(pool.clone());

    let response = post_multipart(
        &app,
        &token,
        &[
            text("status", "with a file"),
            MultipartPart::file(
                "attachment",
                "cat.png",
                "image/png",
                Bytes::from_static(b"PNG"),
            ),
        ],
    )
    .await;
    common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, "a file part");
    assert!(
        response.error_text().contains("attachment"),
        "the 422 names the part: {}",
        response.error_text()
    );
    let posted: (i64,) = sqlx::query_as("SELECT count(*) FROM statuses")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(posted.0, 0, "a refused body posts nothing");
}

/// `POST /api/v1/accounts/{id}/follow` as multipart with `reblogs=false`
/// follows, and the stored relationship hides the target's reblogs.
#[tokio::test]
async fn a_multipart_follow_honours_the_reblogs_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:follows").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(&format!("/api/v1/accounts/{}/follow", bob.id))
        .bearer(&token)
        .multipart(BOUNDARY, &[text("reblogs", "false")])
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the multipart follow");
    let body = response.json_value();
    assert_eq!(body["following"], true, "the follow was granted");
    assert_eq!(body["showing_reblogs"], false, "the flag is in the answer");
    let row = follow::get(&pool, alice.id, bob.id)
        .await
        .unwrap()
        .expect("the relationship exists");
    assert!(!row.show_reblogs, "and stored");
}
