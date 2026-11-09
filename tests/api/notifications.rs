//! Flat notifications over HTTP: `GET /api/v1/notifications` and its
//! single-row read, `POST /api/v1/notifications/clear`, the two
//! dismissals, `DELETE /api/v1/notifications/destroy_multiple` and `POST
//! /api/pleroma/notifications/read` (`src/api/notifications.rs`).
//!
//! Every test seeds the `notifications` rows directly through the
//! `create_notification` fixture and observes them through both the flat
//! v1 listing and the grouped v2 one (`src/api/notifications_v2.rs`
//! shares the rows and the soft stamp): the listing answers newest first
//! with the `Link` header keyed on the row ids, the `types[]` /
//! `exclude_types[]` / `include_types[]` / `account_id` filters and the
//! `with_muted` exclusions narrow it, dismissed rows vanish from every
//! read while the rows stay, the `read` clauses mark seen by id order
//! (`id` reads that row and its elders, `max_id` reads up to the cursor
//! and answers the marked array), deleting a status takes its
//! notifications with it, the reads demand `read:notifications` and every
//! write route demands `write:notifications`.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    models::{
        block, conversation_mute, domain_block,
        mute::{self, MuteOptions},
        notification::Notification,
        status::Visibility,
        user::User,
    },
    util::time::mastodon_time,
};

use crate::common;

/// The v1 routes' mount point: the listing, the read and the dismissals.
const V1: &str = "/api/v1/notifications";
/// Pleroma's read route.
const READ_ROUTE: &str = "/api/pleroma/notifications/read";
/// The scope the two reads demand.
const READ: &str = "read:notifications";
/// The scope every write route in this file demands.
const WRITE: &str = "write:notifications";
/// Both halves, for fixtures that also observe through the v2 listing.
const READ_WRITE: &str = "read:notifications write:notifications";
/// The message of the shared `AppError::NotFound` body.
const NOT_FOUND: &str = "Record not found";
/// The `Link` header's base, `Config::for_tests`' `BASE_URL`.
const BASE: &str = "https://test.example";

/// A local user with a `scopes`-granted bearer token: `(user, token)`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// The group keys of the v2 listing, in answer order (newest first).
async fn listed_keys(app: &Router, token: &str) -> Vec<String> {
    let response = common::TestRequest::get("/api/v2/notifications")
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the grouped listing");
    response.json_value()["notification_groups"]
        .as_array()
        .expect("notification_groups is a list")
        .iter()
        .map(|group| group["group_key"].as_str().expect("a group key").to_owned())
        .collect()
}

/// The `ungrouped-<id>` key of `note`.
fn ungrouped(note: &Notification) -> String {
    format!("ungrouped-{}", note.id)
}

/// `GET /api/v2/notifications/unread_count`'s count for `token`.
async fn unread(app: &Router, token: &str) -> i64 {
    let response = common::TestRequest::get("/api/v2/notifications/unread_count")
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the unread count");
    response.json_value()["count"].as_i64().expect("a count")
}

/// The stored row of `id`, whatever its state, for the state assertions.
async fn stored_row(pool: &PgPool, id: Uuid) -> Option<Notification> {
    sqlx::query_as::<_, Notification>("SELECT * FROM notifications WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .expect("the state read runs")
}

/// Whether the stored row reads seen.
async fn is_seen(pool: &PgPool, id: Uuid) -> bool {
    stored_row(pool, id).await.expect("the row is stored").seen
}

/// The `POST` of `path` with a JSON `body`, as `token`'s bearer.
async fn post_json(app: &Router, path: &str, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The `{}` answer of the dismissal routes.
fn assert_empty_object(response: &common::TestResponse, who: &str) {
    common::assert_answer(response, StatusCode::OK, who);
    assert_eq!(response.json_value(), json!({}), "{who} answers `{{}}`");
}

/// Alice's two notifications, oldest first: a favourite of `post` (held on
/// the seed) by bob, and a follow by carol. Returns the viewer token that
/// also reads the v2 listing.
struct Pair {
    /// Alice's `read:notifications write:notifications` token.
    token: String,
    /// The favourited status.
    post: wired_be::models::status::Status,
    /// The favourite (older).
    favourite: Notification,
    /// The follow (newer).
    follow: Notification,
}

async fn seed_pair(pool: &PgPool) -> Pair {
    let (alice, token) = fixture(pool, "alice", READ_WRITE).await;
    let erin = common::create_local_user(pool, "erin").await;
    let bob = common::create_local_user(pool, "bob").await;
    let carol = common::create_local_user(pool, "carol").await;
    let post = common::create_status(pool, &erin, "hello", Visibility::Public).await;
    let favourite = common::create_notification(pool, &alice, &bob, "favourite", Some(&post)).await;
    let follow = common::create_notification(pool, &alice, &carol, "follow", None).await;
    Pair {
        token,
        post,
        favourite,
        follow,
    }
}

/// `POST /api/v1/notifications/clear` dismisses every notification of the
/// caller — the rows stay, stamped, and vanish from the listing — and
/// answers `{}`; a second clear still answers `{}`.
#[tokio::test]
async fn clear_dismisses_every_notification_of_the_caller() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_pair(&pool).await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(&format!("{V1}/clear"))
        .bearer(&seed.token)
        .send(&app)
        .await;
    assert_empty_object(&response, "the clear");
    assert!(
        listed_keys(&app, &seed.token).await.is_empty(),
        "dismissed notifications vanish from the listing"
    );
    for id in [seed.favourite.id, seed.follow.id] {
        let row = stored_row(&pool, id).await.expect("the row stays");
        assert!(row.dismissed_at.is_some(), "{id} is stamped, not deleted");
    }

    let repeat = common::TestRequest::post(&format!("{V1}/clear"))
        .bearer(&seed.token)
        .send(&app)
        .await;
    assert_empty_object(&repeat, "a second clear");
}

/// Clear touches nobody else's rows: another account's notification stays
/// undismissed.
#[tokio::test]
async fn clear_is_scoped_to_the_caller() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_pair(&pool).await;
    let dave = common::create_local_user(&pool, "dave").await;
    let mallory = common::create_local_user(&pool, "mallory").await;
    let daves_note = common::create_notification(&pool, &dave, &mallory, "follow", None).await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(&format!("{V1}/clear"))
        .bearer(&seed.token)
        .send(&app)
        .await;
    assert_empty_object(&response, "alice's clear");
    let foreign = stored_row(&pool, daves_note.id)
        .await
        .expect("the foreign row stays");
    assert!(foreign.dismissed_at.is_none(), "and stays undismissed");
}

/// The path dismissal removes exactly the notification named, keeps the
/// row soft-dismissed, and answers `{}` again for a repeat.
#[tokio::test]
async fn dismiss_removes_one_notification_from_the_listing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_pair(&pool).await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(&format!("{V1}/{}/dismiss", seed.favourite.id))
        .bearer(&seed.token)
        .send(&app)
        .await;
    assert_empty_object(&response, "the path dismissal");
    assert_eq!(
        listed_keys(&app, &seed.token).await,
        vec![ungrouped(&seed.follow)],
        "only the named row left the listing"
    );
    let row = stored_row(&pool, seed.favourite.id)
        .await
        .expect("the row stays");
    assert!(row.dismissed_at.is_some(), "dismissal is soft");

    let repeat = common::TestRequest::post(&format!("{V1}/{}/dismiss", seed.favourite.id))
        .bearer(&seed.token)
        .send(&app)
        .await;
    assert_empty_object(&repeat, "a repeat dismissal of an owned row");
}

/// A foreign, unknown or malformed id is the same 404 on both dismissal
/// shapes — the route never says whether somebody else holds that id.
#[tokio::test]
async fn dismissal_refuses_foreign_unknown_and_malformed_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_pair(&pool).await;
    let dave = common::create_local_user(&pool, "dave").await;
    let mallory = common::create_local_user(&pool, "mallory").await;
    let foreign = common::create_notification(&pool, &dave, &mallory, "follow", None).await;
    let unknown = Uuid::now_v7();
    let app = common::test_app(pool);

    for id in [foreign.id, unknown, Uuid::nil()] {
        let by_path = common::TestRequest::post(&format!("{V1}/{id}/dismiss"))
            .bearer(&seed.token)
            .send(&app)
            .await;
        common::assert_answer(
            &by_path,
            StatusCode::NOT_FOUND,
            &format!("dismissal of `{id}`"),
        );
        common::assert_error(&by_path, NOT_FOUND, &format!("dismissal of `{id}`"));
        let by_body = post_json(
            &app,
            &format!("{V1}/dismiss"),
            &seed.token,
            json!({"id": id}),
        )
        .await;
        common::assert_answer(
            &by_body,
            StatusCode::NOT_FOUND,
            &format!("the body dismissal of `{id}`"),
        );
        common::assert_error(
            &by_body,
            NOT_FOUND,
            &format!("the body dismissal of `{id}`"),
        );
    }

    let malformed_path = common::TestRequest::post(&format!("{V1}/not-an-id/dismiss"))
        .bearer(&seed.token)
        .send(&app)
        .await;
    common::assert_answer(
        &malformed_path,
        StatusCode::NOT_FOUND,
        "a malformed path id",
    );
    common::assert_error(&malformed_path, NOT_FOUND, "a malformed path id");
    let malformed_body = post_json(
        &app,
        &format!("{V1}/dismiss"),
        &seed.token,
        json!({"id": "not-an-id"}),
    )
    .await;
    common::assert_answer(
        &malformed_body,
        StatusCode::NOT_FOUND,
        "a malformed body id",
    );
    common::assert_error(&malformed_body, NOT_FOUND, "a malformed body id");
    assert_eq!(
        listed_keys(&app, &seed.token).await.len(),
        2,
        "none of the refusals changed alice's listing"
    );
}

/// The deprecated body dismissal (`POST /api/v1/notifications/dismiss`
/// with `id`) removes the named notification and answers `{}`; a body
/// without `id` is the 400 the ticket words.
#[tokio::test]
async fn the_legacy_dismissal_works_by_body_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_pair(&pool).await;
    let app = common::test_app(pool);

    let response = post_json(
        &app,
        &format!("{V1}/dismiss"),
        &seed.token,
        json!({"id": seed.follow.id}),
    )
    .await;
    assert_empty_object(&response, "the legacy dismissal");
    assert_eq!(
        listed_keys(&app, &seed.token).await,
        vec![format!("favourite-{}", seed.post.id)],
        "the named row left the listing, the favourite kept its stored key"
    );

    for body in [json!({}), json!({"id": null}), json!({"other": "x"})] {
        let missing = post_json(&app, &format!("{V1}/dismiss"), &seed.token, body.clone()).await;
        common::assert_answer(
            &missing,
            StatusCode::BAD_REQUEST,
            &format!("`{body}` carries no id"),
        );
        common::assert_error(
            &missing,
            "Missing parameter: id",
            &format!("`{body}` carries no id"),
        );
    }
    let bare = common::TestRequest::post(&format!("{V1}/dismiss"))
        .bearer(&seed.token)
        .send(&app)
        .await;
    common::assert_answer(&bare, StatusCode::BAD_REQUEST, "an empty body");
    common::assert_error(&bare, "Missing parameter: id", "an empty body");
}

/// The form encoding of the legacy dismissal arrives the same way — the
/// extractor structures `id=…` bodies like JSON (decision 10).
#[tokio::test]
async fn the_legacy_dismissal_accepts_form_bodies() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_pair(&pool).await;
    let app = common::test_app(pool);

    let response = common::TestRequest::post(&format!("{V1}/dismiss"))
        .bearer(&seed.token)
        .form(&[("id", &seed.favourite.id.to_string())])
        .send(&app)
        .await;
    assert_empty_object(&response, "the form dismissal");
    assert_eq!(
        listed_keys(&app, &seed.token).await,
        vec![ungrouped(&seed.follow)],
        "and it dismissed"
    );
}

/// `destroy_multiple` dismisses exactly the listed ids that belong to the
/// caller — from the query string, and the same from a JSON body — and
/// answers `{}` whatever else the list names.
#[tokio::test]
async fn destroy_multiple_dismisses_the_owned_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ_WRITE).await;
    let dave = common::create_local_user(&pool, "dave").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let first = common::create_notification(&pool, &alice, &carol, "follow", None).await;
    let second = common::create_notification(&pool, &alice, &carol, "mention", None).await;
    let kept = common::create_notification(&pool, &alice, &carol, "move", None).await;
    let foreign = common::create_notification(&pool, &dave, &carol, "follow", None).await;
    let app = common::test_app(pool.clone());

    let route = format!(
        "{V1}/destroy_multiple?ids[]={}&ids[]={}&ids[]=not-an-id",
        first.id, foreign.id
    );
    let response = common::TestRequest::delete(&route)
        .bearer(&token)
        .send(&app)
        .await;
    assert_empty_object(&response, "the query-string bulk dismissal");
    assert_eq!(
        listed_keys(&app, &token).await,
        vec![ungrouped(&kept), ungrouped(&second)],
        "the listed row left the listing, newest first, the unlisted stayed"
    );
    let untouched = stored_row(&pool, foreign.id)
        .await
        .expect("the foreign row stays");
    assert!(
        untouched.dismissed_at.is_none(),
        "and dave's row was not touched"
    );

    let by_body = common::TestRequest::delete(&format!("{V1}/destroy_multiple"))
        .bearer(&token)
        .json(&json!({"ids": [second.id, Uuid::now_v7(), second.id]}))
        .send(&app)
        .await;
    assert_empty_object(&by_body, "the json-body bulk dismissal");
    assert_eq!(
        listed_keys(&app, &token).await,
        vec![ungrouped(&kept)],
        "the second row followed it out of the listing"
    );

    let nothing = common::TestRequest::delete(&format!("{V1}/destroy_multiple"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_empty_object(&nothing, "a bulk dismissal that names no id");
}

/// Alice's three notifications for the read tests, oldest first: a follow
/// by erin, a mention by erin, and a favourite of `post` (held on the
/// seed) by bob.
struct ReadSeed {
    /// Alice's `read:notifications write:notifications` token.
    token: String,
    /// The rows, oldest first.
    notes: Vec<Notification>,
    /// Bob, the actor of the newest row.
    bob: User,
    /// The favourited status id.
    status_id: Uuid,
}

async fn seed_read(pool: &PgPool) -> ReadSeed {
    let (alice, token) = fixture(pool, "alice", READ_WRITE).await;
    let erin = common::create_local_user(pool, "erin").await;
    let bob = common::create_local_user(pool, "bob").await;
    let post = common::create_status(pool, &erin, "hello", Visibility::Public).await;
    let oldest = common::create_notification(pool, &alice, &erin, "follow", None).await;
    let middle = common::create_notification(pool, &alice, &erin, "mention", None).await;
    let newest = common::create_notification(pool, &alice, &bob, "favourite", Some(&post)).await;
    ReadSeed {
        token,
        notes: vec![oldest, middle, newest],
        bob,
        status_id: post.id,
    }
}

/// `read` with `id` marks that notification and everything older seen,
/// leaves the newer row unseen, and answers the notification entity —
/// seen flag set, actor expanded, keys as Pleroma's show view renders
/// them.
#[tokio::test]
async fn read_by_id_marks_the_row_and_its_elders() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_read(&pool).await;
    let middle = &seed.notes[1];
    let app = common::test_app(pool.clone());

    let response = post_json(&app, READ_ROUTE, &seed.token, json!({"id": middle.id})).await;
    common::assert_answer(&response, StatusCode::OK, "the read by id");
    let body = response.json_value();
    assert_eq!(
        body["id"],
        middle.id.to_string(),
        "the entity of the named notification"
    );
    assert_eq!(body["type"], "mention", "its stored type");
    assert_eq!(
        body["group_key"],
        ungrouped(middle),
        "the derived ungrouped key of an ungroupable row"
    );
    assert_eq!(
        body["created_at"],
        mastodon_time(&middle.inserted_at),
        "the Mastodon time of the row"
    );
    assert_eq!(body["pleroma"]["is_seen"], true, "seen, as just marked");
    assert_eq!(body["pleroma"]["is_muted"], false, "alice mutes nobody");
    assert_eq!(
        body["account"]["id"],
        middle.from_account_id.to_string(),
        "the actor expanded to a full account"
    );
    assert!(body.get("status").is_none(), "this row names no status");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the entity is JSON"
    );

    for note in &seed.notes[..2] {
        assert!(is_seen(&pool, note.id).await, "{} read", note.id);
    }
    assert!(
        !is_seen(&pool, seed.notes[2].id).await,
        "and the newer row was not"
    );
    assert_eq!(
        unread(&app, &seed.token).await,
        1,
        "the two marked rows left the unread count"
    );
}

/// The favourite's entity renders the named status beside the actor, the
/// read walks all the way back when it starts at the newest row, and
/// `id` wins when both parameters arrive.
#[tokio::test]
async fn read_by_id_renders_a_status_notification() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_read(&pool).await;
    let newest = &seed.notes[2];
    let app = common::test_app(pool.clone());

    let response = post_json(
        &app,
        READ_ROUTE,
        &seed.token,
        json!({"id": newest.id, "max_id": seed.notes[0].id}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the read of the favourite");
    let body = response.json_value();
    assert!(body.is_object(), "the id clause answers the single entity");
    assert_eq!(
        body["group_key"],
        format!("favourite-{}", seed.status_id),
        "the stored group key of a favourite"
    );
    assert_eq!(
        body["status"]["id"],
        seed.status_id.to_string(),
        "the favourited status rendered"
    );
    assert_eq!(
        body["account"]["id"],
        seed.bob.id.to_string(),
        "bob caused it"
    );
    for note in &seed.notes {
        assert!(is_seen(&pool, note.id).await, "{} read", note.id);
    }
    assert_eq!(unread(&app, &seed.token).await, 0, "nothing unread now");
}

/// `read` with `id` answers the 404 for a foreign, unknown or malformed
/// id, and marks nothing on the way.
#[tokio::test]
async fn read_by_id_refuses_rows_the_caller_does_not_own() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_read(&pool).await;
    let dave = common::create_local_user(&pool, "dave").await;
    let mallory = common::create_local_user(&pool, "mallory").await;
    let foreign = common::create_notification(&pool, &dave, &mallory, "follow", None).await;
    let app = common::test_app(pool.clone());

    for id_text in [
        foreign.id.to_string(),
        Uuid::now_v7().to_string(),
        "not-an-id".to_owned(),
    ] {
        let response = post_json(&app, READ_ROUTE, &seed.token, json!({"id": id_text})).await;
        common::assert_answer(
            &response,
            StatusCode::NOT_FOUND,
            &format!("read of `{id_text}`"),
        );
        common::assert_error(&response, NOT_FOUND, &format!("read of `{id_text}`"));
    }
    assert!(
        !is_seen(&pool, foreign.id).await,
        "the foreign row stays unseen"
    );
    for note in &seed.notes {
        assert!(
            !is_seen(&pool, note.id).await,
            "{} stays unseen too",
            note.id
        );
    }
}

/// `read` with `max_id` marks everything up to and including the cursor
/// and answers the array of the rows this call marked, newest first; a
/// repeat marks nothing new and answers `[]`, and the walk onward picks up
/// where it stopped.
#[tokio::test]
async fn read_by_max_id_answers_the_marked_array() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_read(&pool).await;
    let app = common::test_app(pool.clone());

    let response = post_json(
        &app,
        READ_ROUTE,
        &seed.token,
        json!({"max_id": seed.notes[1].id}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the read by max_id");
    let body = response.json_value();
    let ids: Vec<String> = body
        .as_array()
        .expect("the marked notifications are an array")
        .iter()
        .map(|entity| entity["id"].as_str().expect("an id").to_owned())
        .collect();
    assert_eq!(
        ids,
        vec![seed.notes[1].id.to_string(), seed.notes[0].id.to_string()],
        "the cursor itself and its elder, newest first"
    );
    assert_eq!(
        body[0]["pleroma"]["is_seen"], true,
        "the entities read seen"
    );
    assert!(
        !is_seen(&pool, seed.notes[2].id).await,
        "the newer row is out"
    );

    let repeat = post_json(
        &app,
        READ_ROUTE,
        &seed.token,
        json!({"max_id": seed.notes[1].id}),
    )
    .await;
    common::assert_answer(&repeat, StatusCode::OK, "the repeat");
    assert_eq!(repeat.json_value(), json!([]), "answers the empty array");

    let onward = post_json(
        &app,
        READ_ROUTE,
        &seed.token,
        json!({"max_id": seed.notes[2].id}),
    )
    .await;
    common::assert_answer(&onward, StatusCode::OK, "the walk onward");
    let marked = onward.json_value();
    let array = marked.as_array().expect("an array");
    assert_eq!(array.len(), 1, "only the unseen newest row is marked");
    assert_eq!(array[0]["id"], seed.notes[2].id.to_string(), "that one");
}

/// A `max_id` older than everything marks nothing and answers `[]`; one
/// that is not id text is the pagination wording's 400.
#[tokio::test]
async fn read_by_max_id_walks_empty_cursors_and_rejects_garbage() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_read(&pool).await;
    let app = common::test_app(pool);

    let old_cursor = post_json(
        &app,
        READ_ROUTE,
        &seed.token,
        json!({"max_id": Uuid::nil()}),
    )
    .await;
    common::assert_answer(
        &old_cursor,
        StatusCode::OK,
        "a cursor older than everything",
    );
    assert_eq!(old_cursor.json_value(), json!([]), "marks nothing");

    let garbage = post_json(&app, READ_ROUTE, &seed.token, json!({"max_id": "nope"})).await;
    common::assert_answer(
        &garbage,
        StatusCode::BAD_REQUEST,
        "a cursor that is not id text",
    );
    common::assert_error(&garbage, "Invalid id", "a cursor that is not id text");
}

/// `read` given neither parameter is the 400 the ticket words.
#[tokio::test]
async fn read_asks_for_one_of_its_two_parameters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_read(&pool).await;
    let app = common::test_app(pool);

    for body in [json!({}), json!({"other": 1})] {
        let response = post_json(&app, READ_ROUTE, &seed.token, body.clone()).await;
        common::assert_answer(
            &response,
            StatusCode::BAD_REQUEST,
            &format!("`{body}` names no target"),
        );
        common::assert_error(
            &response,
            "Missing parameter: id or max_id",
            &format!("`{body}` names no target"),
        );
    }
}

/// A muted actor reads as `is_muted` on the entity (Pleroma's
/// `User.mutes?` of the reading user).
#[tokio::test]
async fn read_marks_a_muted_actor() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ_WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let _muted = mute::mute(&pool, &alice, &bob, MuteOptions::default())
        .await
        .expect("the mute fixture lands");
    let app = common::test_app(pool);

    let response = post_json(&app, READ_ROUTE, &token, json!({"id": note.id})).await;
    common::assert_answer(&response, StatusCode::OK, "the read of a muted actor");
    assert_eq!(
        response.json_value()["pleroma"]["is_muted"],
        true,
        "the flag reaches the entity"
    );
}

/// Deleting a status removes the notifications that were about it — the
/// rows go, so they vanish from every read — while status-less
/// notifications of the same recipient stay.
#[tokio::test]
async fn deleting_a_status_takes_its_notifications() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (erin, erin_token) = fixture(
        &pool,
        "erin",
        "read:notifications write:notifications write:statuses",
    )
    .await;
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &erin, "take me down", Visibility::Public).await;
    let about = common::create_notification(&pool, &erin, &alice, "favourite", Some(&post)).await;
    let unrelated = common::create_notification(&pool, &erin, &alice, "follow", None).await;
    let app = common::test_app(pool.clone());

    let deleted = common::TestRequest::delete(&format!("/api/v1/statuses/{}", post.id))
        .bearer(&erin_token)
        .send(&app)
        .await;
    common::assert_answer(&deleted, StatusCode::OK, "the status delete");
    assert!(
        stored_row(&pool, about.id).await.is_none(),
        "the notification about the status is gone"
    );
    assert_eq!(
        listed_keys(&app, &erin_token).await,
        vec![ungrouped(&unrelated)],
        "and only the status-less notification is left to list"
    );
}

/// No credential answers 401 "Invalid credentials" on every route here,
/// and an app-only token — a live credential that names no user — answers
/// 403 "This API requires an authenticated user".
#[tokio::test]
async fn every_route_demands_a_user_credential() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool);

    let anonymous = call_every_route(&app, "", note.id).await;
    common::assert_all_refused(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "Invalid credentials",
        "an unauthenticated flat notification request",
    );

    let app_only_refusals = call_every_route(&app, &app_only, note.id).await;
    common::assert_all_refused(
        &app_only_refusals,
        StatusCode::FORBIDDEN,
        "This API requires an authenticated user",
        "an app-only credential on the flat notification API",
    );
}

/// One of the routes under test: verb, path, and whether the request
/// carries an `{"id": …}` body — the handlers that read an id get one, so
/// a missing parameter never outruns the credential check.
#[derive(Clone, Debug)]
struct GatedRoute {
    /// The verb, `"GET"`, `"POST"` or `"DELETE"`.
    verb: &'static str,
    /// The request target, query string included.
    path: String,
    /// Whether to send an `id` body with the request.
    with_id_body: bool,
}

/// The five write routes of this file, all mounted on `id` where they
/// name one.
fn gated_routes(id: Uuid) -> Vec<GatedRoute> {
    vec![
        GatedRoute {
            verb: "POST",
            path: format!("{V1}/clear"),
            with_id_body: false,
        },
        GatedRoute {
            verb: "POST",
            path: format!("{V1}/{id}/dismiss"),
            with_id_body: true,
        },
        GatedRoute {
            verb: "POST",
            path: format!("{V1}/dismiss"),
            with_id_body: true,
        },
        GatedRoute {
            verb: "DELETE",
            path: format!("{V1}/destroy_multiple?ids[]={id}"),
            with_id_body: false,
        },
        GatedRoute {
            verb: "POST",
            path: READ_ROUTE.to_owned(),
            with_id_body: true,
        },
    ]
}

/// The two read routes, both mounted on `id` where they name one.
fn read_routes(id: Uuid) -> Vec<GatedRoute> {
    vec![
        GatedRoute {
            verb: "GET",
            path: V1.to_owned(),
            with_id_body: false,
        },
        GatedRoute {
            verb: "GET",
            path: format!("{V1}/{id}"),
            with_id_body: false,
        },
    ]
}

/// Send `route` with the bearer `token`; empty `token` sends no
/// credential.
async fn call(app: &Router, route: &GatedRoute, token: &str, id: Uuid) -> common::TestResponse {
    let mut request = match route.verb {
        "GET" => common::TestRequest::get(&route.path),
        "DELETE" => common::TestRequest::delete(&route.path),
        _ => common::TestRequest::post(&route.path),
    };
    if !token.is_empty() {
        request = request.bearer(token);
    }
    if route.with_id_body {
        request = request.json(&json!({"id": id}));
    }
    request.send(app).await
}

/// Call every route of the flat API — reads and writes — with `token`
/// (empty: unauthenticated).
async fn call_every_route(app: &Router, token: &str, id: Uuid) -> Vec<common::TestResponse> {
    let mut routes = read_routes(id);
    routes.extend(gated_routes(id));
    let mut responses = Vec::new();
    for route in routes {
        responses.push(call(app, &route, token, id).await);
    }
    responses
}

/// Every write route demands `write:notifications`: a
/// `read:notifications` token is refused with Pleroma's wording, and the
/// root `write` grant passes the hierarchy.
#[tokio::test]
async fn every_route_demands_write_notifications() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let narrow = common::create_app_and_token(&pool, &alice, READ).await;
    let write_root = common::create_app_and_token(&pool, &alice, "write").await;
    let app = common::test_app(pool);

    let mut refused = Vec::new();
    for route in gated_routes(note.id) {
        refused.push(call(&app, &route, &narrow, note.id).await);
    }
    common::assert_all_refused(
        &refused,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: write:notifications.",
        "a read:notifications token on the flat notification routes",
    );

    let granted = common::TestRequest::post(&format!("{V1}/clear"))
        .bearer(&write_root)
        .send(&app)
        .await;
    assert_empty_object(&granted, "`write` covers `write:notifications`");
}

/// The two reads demand `read:notifications`: a `write:notifications`
/// token is refused with Pleroma's wording, and the root `read` grant
/// passes the hierarchy.
#[tokio::test]
async fn the_reads_demand_read_notifications() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let narrow = common::create_app_and_token(&pool, &alice, WRITE).await;
    let read_root = common::create_app_and_token(&pool, &alice, "read").await;
    let app = common::test_app(pool);

    let mut refused = Vec::new();
    for route in read_routes(note.id) {
        refused.push(call(&app, &route, &narrow, note.id).await);
    }
    common::assert_all_refused(
        &refused,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: read:notifications.",
        "a write:notifications token on the flat notification reads",
    );

    for route in read_routes(note.id) {
        let granted = call(&app, &route, &read_root, note.id).await;
        common::assert_answer(
            &granted,
            StatusCode::OK,
            &format!("`read` covers the scope on {}", route.path),
        );
    }
}

/// `GET /api/v1/notifications` as `token`, `query` (a `?…` string or empty)
/// appended verbatim.
async fn list(app: &Router, token: &str, query: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("{V1}{query}"))
        .bearer(token)
        .send(app)
        .await
}

/// The entity ids of a flat listing answer, in answer order.
fn listed_ids(response: &common::TestResponse) -> Vec<String> {
    response
        .json_value()
        .as_array()
        .expect("the flat listing is an array")
        .iter()
        .map(|entity| entity["id"].as_str().expect("an entity id").to_owned())
        .collect()
}

/// The keys of an entity object, sorted, for exact shape assertions.
fn key_set(entity: &Value) -> Vec<String> {
    let mut keys: Vec<String> = entity
        .as_object()
        .expect("entities render objects")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// Assert the listing response answers exactly the ids of `want`, in
/// order — newest first wherever the listing is the one answering.
fn assert_listed(response: &common::TestResponse, want: &[&Notification], who: &str) {
    let ids: Vec<String> = want.iter().map(|note| note.id.to_string()).collect();
    assert_eq!(listed_ids(response), ids, "{who}");
}

/// The `Link` header Pleroma's `add_link_headers` writes for a page whose
/// newest row is `newest` and oldest `oldest`.
fn expected_link(newest: Uuid, oldest: Uuid) -> String {
    format!(
        "<{BASE}{V1}?max_id={oldest}>; rel=\"next\", \
         <{BASE}{V1}?min_id={newest}>; rel=\"prev\""
    )
}

/// `GET /api/v1/notifications` answers the caller's own notifications,
/// newest first, with the `Link` header keyed on the page's first and last
/// row ids.
#[tokio::test]
async fn the_listing_answers_newest_first_with_a_link_header() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_listing(&pool).await;
    let app = common::test_app(pool);

    let response = list(&app, &seed.token, "").await;
    common::assert_answer(&response, StatusCode::OK, "the flat listing");
    assert_eq!(
        listed_ids(&response),
        vec![
            seed.follow.id.to_string(),
            seed.mention.id.to_string(),
            seed.favourite.id.to_string()
        ],
        "newest first, and only alice's rows"
    );
    assert_eq!(
        response.header("link"),
        Some(expected_link(seed.follow.id, seed.favourite.id).as_str()),
        "the cursors walk the page's own row ids"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the entities are JSON"
    );
}

/// The listing's entities carry Pleroma's show-view shape: the row's `id`,
/// `type`, `group_key` and `created_at`, the actor expanded, the `pleroma`
/// flags fresh off the row, and the named status rendered for the rows
/// that have one.
#[tokio::test]
async fn flat_entities_carry_the_show_view_shape() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_listing(&pool).await;
    let app = common::test_app(pool);

    let body = list(&app, &seed.token, "").await.json_value();
    let entities = body.as_array().expect("an array");
    let (follow, favourite) = (&entities[0], &entities[2]);
    assert_eq!(
        key_set(follow),
        [
            "account",
            "created_at",
            "group_key",
            "id",
            "pleroma",
            "type"
        ],
        "the status-less entity's keys"
    );
    assert_eq!(follow["id"], seed.follow.id.to_string(), "id as text");
    assert_eq!(follow["type"], "follow", "the stored type");
    assert_eq!(
        follow["group_key"],
        ungrouped(&seed.follow),
        "the derived key of an ungroupable row"
    );
    assert_eq!(
        follow["created_at"],
        mastodon_time(&seed.follow.inserted_at),
        "the Mastodon time of the row"
    );
    assert_eq!(
        follow["account"]["id"],
        seed.erin_id.to_string(),
        "the actor expanded to a full account"
    );
    assert_eq!(follow["pleroma"]["is_seen"], false, "an unseen row");
    assert_eq!(follow["pleroma"]["is_muted"], false, "alice mutes nobody");
    assert!(
        follow.get("status").is_none(),
        "a status-less row renders none"
    );
    assert_eq!(
        favourite["group_key"],
        format!("favourite-{}", seed.post_id),
        "the stored key of a favourite"
    );
    assert_eq!(
        favourite["status"]["id"],
        seed.post_id.to_string(),
        "the favourited status rendered"
    );
}

/// The listing pages on row ids: `limit` cuts the page, `max_id`,
/// `since_id` and `min_id` walk it — each newest-first — an exhausted walk
/// answers `[]` with no `Link` header, an unusable `limit` falls back to
/// the default, and a cursor that is no id text is the 400.
#[tokio::test]
async fn the_listing_pages_with_its_cursors() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let notes = ladder(&pool, &alice, &bob).await;
    let app = common::test_app(pool);

    let page = list(&app, &token, "?limit=2").await;
    assert_eq!(
        listed_ids(&page),
        vec![notes[4].id.to_string(), notes[3].id.to_string()],
        "the two newest first"
    );
    assert_eq!(
        page.header("link"),
        Some(expected_link(notes[4].id, notes[3].id).as_str()),
        "and the cursors into the elders"
    );

    let older = list(&app, &token, &format!("?limit=2&max_id={}", notes[3].id)).await;
    assert_eq!(
        listed_ids(&older),
        vec![notes[2].id.to_string(), notes[1].id.to_string()],
        "max_id keeps strictly older rows"
    );

    let gap = list(&app, &token, &format!("?since_id={}", notes[3].id)).await;
    assert_eq!(
        listed_ids(&gap),
        vec![notes[4].id.to_string()],
        "since_id keeps the newer row, newest first"
    );

    let forward = list(&app, &token, &format!("?min_id={}", notes[2].id)).await;
    assert_eq!(
        listed_ids(&forward),
        vec![notes[4].id.to_string(), notes[3].id.to_string()],
        "a min_id page reads forward and still answers newest-first"
    );

    let exhausted = list(&app, &token, &format!("?max_id={}", notes[0].id)).await;
    assert_eq!(
        exhausted.json_value(),
        json!([]),
        "older than everything answers nothing"
    );
    assert!(
        exhausted.header("link").is_none(),
        "and an empty page carries no Link header"
    );

    let all = list(&app, &token, "?limit=0").await;
    assert_eq!(
        listed_ids(&all).len(),
        notes.len(),
        "a limit of none asks the default page"
    );

    let garbage = list(&app, &token, "?max_id=not-an-id").await;
    common::assert_answer(&garbage, StatusCode::BAD_REQUEST, "a cursor that is no id");
    common::assert_error(&garbage, "Invalid id", "a cursor that is no id");
}

/// `limit` defaults to twenty and is capped at forty.
#[tokio::test]
async fn the_listing_default_limit_is_twenty_and_the_cap_forty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    for _ in 0..41 {
        let _note = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    }
    let app = common::test_app(pool);

    assert_eq!(
        listed_ids(&list(&app, &token, "").await).len(),
        20,
        "the default page"
    );
    assert_eq!(
        listed_ids(&list(&app, &token, "?limit=100").await).len(),
        40,
        "and the cap an over-large request falls to"
    );
}

/// `types[]` restricts the page and `exclude_types[]` removes from it, the
/// `include_types[]` alias feeds `types[]` when that is absent, and an
/// invalid type string is no error — it simply matches nothing.
#[tokio::test]
async fn the_listing_filters_the_types_through_every_spelling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_listing(&pool).await;
    let app = common::test_app(pool);

    let kept = list(&app, &seed.token, "?types[]=favourite&types[]=mention").await;
    assert_listed(
        &kept,
        &[&seed.mention, &seed.favourite],
        "types[] keeps the named types",
    );
    let lone = list(&app, &seed.token, "?types=favourite").await;
    assert_listed(&lone, &[&seed.favourite], "and the lone `types=` spelling");

    let dropped = list(&app, &seed.token, "?exclude_types[]=favourite").await;
    assert_listed(
        &dropped,
        &[&seed.follow, &seed.mention],
        "exclude_types[] removes the named type",
    );

    let aliased = list(&app, &seed.token, "?include_types[]=follow").await;
    assert_listed(
        &aliased,
        &[&seed.follow],
        "include_types[] is the alias of types[]",
    );
    let both = list(
        &app,
        &seed.token,
        "?types[]=follow&include_types[]=favourite",
    )
    .await;
    assert_listed(
        &both,
        &[&seed.follow],
        "and `types[]` wins when both arrive, Pleroma's put_new rule",
    );

    let nonsense = list(&app, &seed.token, "?types[]=not-a-type").await;
    common::assert_answer(
        &nonsense,
        StatusCode::OK,
        "a type string that matches nothing",
    );
    assert_eq!(
        nonsense.json_value(),
        json!([]),
        "an invalid type is ignored: it names no row"
    );
}

/// `account_id` keeps one actor's rows; an id naming no readable account
/// is the 404, and a `with_muted` that is no boolean the 422.
#[tokio::test]
async fn the_listing_narrows_to_one_account_and_refuses_stranger_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_listing(&pool).await;
    let app = common::test_app(pool);

    let only_carol = list(&app, &seed.token, &format!("?account_id={}", seed.carol_id)).await;
    assert_eq!(
        listed_ids(&only_carol),
        vec![seed.mention.id.to_string()],
        "account_id keeps one actor's rows"
    );
    let only_bob = list(&app, &seed.token, &format!("?account_id={}", seed.bob_id)).await;
    assert_eq!(
        listed_ids(&only_bob),
        vec![seed.favourite.id.to_string()],
        "and asks answer the same way"
    );

    for query in [
        format!("account_id={}", Uuid::now_v7()),
        "account_id=not-an-id".to_owned(),
    ] {
        let missing = common::TestRequest::get(&format!("{V1}?{query}"))
            .bearer(&seed.token)
            .send(&app)
            .await;
        common::assert_answer(&missing, StatusCode::NOT_FOUND, &query);
        common::assert_error(&missing, NOT_FOUND, &query);
    }

    for spelling in ["with_muted=maybe", "with_muted=2"] {
        let asked = list(&app, &seed.token, &format!("?{spelling}")).await;
        common::assert_answer(&asked, StatusCode::UNPROCESSABLE_ENTITY, spelling);
        let message = asked.json_value()["error"]
            .as_str()
            .expect("an error body")
            .to_owned();
        assert!(
            message.starts_with("Invalid parameters"),
            "a `with_muted` that is no boolean is the parameters 422: {message}"
        );
    }
}

/// Left at its default the listing hides notifications whose cause the
/// caller muted — an actor under a mute that hides notifications, and a
/// status in a muted thread — while a plain timeline-only mute hides
/// nothing; blocked actors and blocked domains stay hidden `with_muted` or
/// not, Pleroma's unconditional clauses.
#[tokio::test]
async fn the_listing_hides_muted_causes_until_asked_otherwise() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_mutes(&pool).await;
    let app = common::test_app(pool);

    let by_default = list(&app, &seed.token, "").await;
    assert_eq!(
        listed_ids(&by_default),
        vec![seed.plain.id.to_string(), seed.from_timeline.id.to_string()],
        "the muted actor and the muted thread hide; the timeline-only \
         mute, and everything untouched, stay"
    );

    let asked = list(&app, &seed.token, "?with_muted=true").await;
    assert_eq!(
        listed_ids(&asked),
        vec![
            seed.plain.id.to_string(),
            seed.on_muted_thread.id.to_string(),
            seed.from_timeline.id.to_string(),
            seed.from_muted.id.to_string(),
        ],
        "with_muted brings the muted actor and the muted thread back, \
         and Pleroma's blocks stay out"
    );
    let body = asked.json_value();
    let entities = body.as_array().expect("an array");
    let flag = |wanted: &Uuid| {
        entities
            .iter()
            .find(|entity| entity["id"] == wanted.to_string())
            .unwrap_or_else(|| panic!("{wanted} is in the page"))
            .pointer("/pleroma/is_muted")
            .expect("the mute flag")
            .clone()
    };
    assert_eq!(
        flag(&seed.from_muted.id),
        json!(true),
        "the notification-hiding mute marks its entity"
    );
    assert_eq!(
        flag(&seed.from_timeline.id),
        json!(true),
        "and so does the timeline mute, which hid nothing"
    );
    assert_eq!(
        flag(&seed.on_muted_thread.id),
        json!(false),
        "only the thread was muted, not its author"
    );
}

/// Dismissed rows never come back — not even with `with_muted` — and
/// neither do rows of a deactivated actor.
#[tokio::test]
async fn the_listing_hides_dismissed_and_deactivated_rows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ_WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let erin = common::create_local_user(&pool, "erin").await;
    let doomed = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let _ghosted = common::create_notification(&pool, &alice, &carol, "follow", None).await;
    let live = common::create_notification(&pool, &alice, &erin, "follow", None).await;
    let app = common::test_app(pool.clone());

    let dismissed = common::TestRequest::post(&format!("{V1}/{}/dismiss", doomed.id))
        .bearer(&token)
        .send(&app)
        .await;
    assert_empty_object(&dismissed, "the fixture dismissal");
    let _deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(carol.id)
        .execute(&pool)
        .await
        .expect("the deactivation lands");

    assert_eq!(
        listed_ids(&list(&app, &token, "").await),
        vec![live.id.to_string()],
        "the dismissed row and the deactivated actor's row are out"
    );
    assert_eq!(
        listed_ids(&list(&app, &token, "?with_muted=true").await),
        vec![live.id.to_string()],
        "and `with_muted` does not uncover them"
    );
}

/// The ten documented types, and whether each fixture row names the status.
const TYPE_SPREAD: [(&str, bool); 10] = [
    ("mention", true),
    ("follow", false),
    ("follow_request", false),
    ("reblog", true),
    ("favourite", true),
    ("move", false),
    ("update", false),
    ("poll", true),
    ("status", true),
    ("pleroma:emoji_reaction", true),
];

/// Every notification type renders its entity in the listing: the stored
/// type, the expanded actor, the fresh `pleroma` flags, the Mastodon time
/// — and the reaction its emoji beside its status.
#[tokio::test]
async fn every_type_renders_its_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let post = common::create_status(&pool, &bob, "hello", Visibility::Public).await;
    let mut seeded = Vec::new();
    for (notification_type, names_status) in TYPE_SPREAD {
        let note = if notification_type == "pleroma:emoji_reaction" {
            common::create_reaction(&pool, &alice, &bob, &post, "🎉").await
        } else {
            let status = names_status.then_some(&post);
            common::create_notification(&pool, &alice, &bob, notification_type, status).await
        };
        seeded.push(note);
    }
    let app = common::test_app(pool);

    let body = list(&app, &token, "").await.json_value();
    let entities = body.as_array().expect("an array");
    assert_eq!(entities.len(), seeded.len(), "every type is in the answer");
    for ((notification_type, names_status), note) in TYPE_SPREAD.iter().zip(&seeded) {
        let entity = entities
            .iter()
            .find(|candidate| candidate["id"] == note.id.to_string())
            .unwrap_or_else(|| panic!("{} is in the page", note.id));
        assert_eq!(entity["type"], *notification_type, "the stored type");
        assert_eq!(
            entity["account"]["id"],
            bob.id.to_string(),
            "the actor expanded for {notification_type}"
        );
        assert_eq!(
            entity["pleroma"]["is_seen"],
            json!(false),
            "a fresh row reads unseen in {notification_type}"
        );
        assert_eq!(
            entity["created_at"],
            mastodon_time(&note.inserted_at),
            "the Mastodon time of {notification_type}"
        );
        let rendered_status = entity.get("status").is_some();
        assert_eq!(
            rendered_status, *names_status,
            "the status key rides on the status-naming types only, in {notification_type}"
        );
    }
    let reaction = entities
        .iter()
        .find(|entity| entity["type"] == "pleroma:emoji_reaction")
        .expect("the reaction");
    assert_eq!(reaction["emoji"], "🎉", "the reaction carries its emoji");
    assert_eq!(
        reaction["group_key"],
        format!("pleroma:emoji_reaction-{}-🎉", post.id),
        "and the stored key that groups it"
    );
    let favourite = entities
        .iter()
        .find(|entity| entity["type"] == "favourite")
        .expect("the favourite");
    assert_eq!(
        favourite["group_key"],
        format!("favourite-{}", post.id),
        "the favourite's stored key"
    );
    let follow = entities
        .iter()
        .find(|entity| entity["type"] == "follow")
        .expect("the follow");
    assert_eq!(
        follow["group_key"],
        format!("ungrouped-{}", follow["id"].as_str().expect("an id")),
        "an ungroupable row derives its key"
    );
}

/// `GET /api/v1/notifications/{id}` answers the caller's live row as the
/// listing renders it, and Pleroma's `is_seen` flag follows the row's
/// `seen` flag.
#[tokio::test]
async fn show_answers_the_owned_live_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_listing(&pool).await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::get(&format!("{V1}/{}", seed.favourite.id))
        .bearer(&seed.token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the read of the owned row");
    let body = response.json_value();
    assert_eq!(body["id"], seed.favourite.id.to_string(), "the one entity");
    assert_eq!(body["type"], "favourite");
    assert_eq!(
        body["group_key"],
        format!("favourite-{}", seed.post_id),
        "the stored key"
    );
    assert_eq!(body["account"]["id"], seed.bob_id.to_string(), "the actor");
    assert_eq!(
        body["status"]["id"],
        seed.post_id.to_string(),
        "the named status rendered"
    );
    assert_eq!(body["pleroma"]["is_seen"], false, "unseen");

    let _seen = sqlx::query("UPDATE notifications SET seen = true WHERE id = $1")
        .bind(seed.favourite.id)
        .execute(&pool)
        .await
        .expect("the seen flag lands");
    let read = common::TestRequest::get(&format!("{V1}/{}", seed.favourite.id))
        .bearer(&seed.token)
        .send(&app)
        .await;
    common::assert_answer(&read, StatusCode::OK, "the read of the seen row");
    assert_eq!(
        read.json_value()["pleroma"]["is_seen"],
        true,
        "the Pleroma flag follows the row"
    );
}

/// The read 404s, one indistinguishable answer, on a dismissed, foreign,
/// unknown, deactivated-actor's or malformed id.
#[tokio::test]
async fn show_refuses_everything_the_caller_may_not_see() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let mallory = common::create_local_user(&pool, "mallory").await;
    let doomed = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let ghosted = common::create_notification(&pool, &alice, &carol, "follow", None).await;
    let foreign = common::create_notification(&pool, &dave, &mallory, "follow", None).await;
    let app = common::test_app(pool.clone());

    let writer = common::create_app_and_token(&pool, &alice, WRITE).await;
    let dismissed = common::TestRequest::post(&format!("{V1}/{}/dismiss", doomed.id))
        .bearer(&writer)
        .send(&app)
        .await;
    assert_empty_object(&dismissed, "the fixture dismissal");
    let _deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(carol.id)
        .execute(&pool)
        .await
        .expect("the deactivation lands");

    for id_text in [
        doomed.id.to_string(),
        ghosted.id.to_string(),
        foreign.id.to_string(),
        Uuid::now_v7().to_string(),
        "not-an-id".to_owned(),
    ] {
        let response = common::TestRequest::get(&format!("{V1}/{id_text}"))
            .bearer(&token)
            .send(&app)
            .await;
        common::assert_answer(
            &response,
            StatusCode::NOT_FOUND,
            &format!("the read of `{id_text}`"),
        );
        common::assert_error(&response, NOT_FOUND, &format!("the read of `{id_text}`"));
    }
}

/// Alice's five plain follows of `bob`, oldest first — the cursor walk's
/// ladder.
async fn ladder(pool: &PgPool, alice: &User, bob: &User) -> Vec<Notification> {
    let mut notes = Vec::new();
    for _ in 0..5 {
        notes.push(common::create_notification(pool, alice, bob, "follow", None).await);
    }
    notes
}

/// The shared listing seed: alice's unseen three — a favourite of
/// `erin`'s post by bob, a mention by carol and a follow by erin, oldest
/// first — plus a foreign row nobody may see through.
struct ListingSeed {
    /// Alice's `read:notifications` token.
    token: String,
    /// Erin's post, the status the favourites and mentions name.
    post_id: Uuid,
    /// Erin's id.
    erin_id: Uuid,
    /// Bob's id.
    bob_id: Uuid,
    /// Carol's id.
    carol_id: Uuid,
    /// Bob's favourite (oldest).
    favourite: Notification,
    /// Carol's mention.
    mention: Notification,
    /// Erin's follow (newest).
    follow: Notification,
}

async fn seed_listing(pool: &PgPool) -> ListingSeed {
    let (alice, token) = fixture(pool, "alice", READ).await;
    let erin = common::create_local_user(pool, "erin").await;
    let bob = common::create_local_user(pool, "bob").await;
    let carol = common::create_local_user(pool, "carol").await;
    let dave = common::create_local_user(pool, "dave").await;
    let mallory = common::create_local_user(pool, "mallory").await;
    let post = common::create_status(pool, &erin, "hello", Visibility::Public).await;
    let favourite = common::create_notification(pool, &alice, &bob, "favourite", Some(&post)).await;
    let mention = common::create_notification(pool, &alice, &carol, "mention", Some(&post)).await;
    let follow = common::create_notification(pool, &alice, &erin, "follow", None).await;
    let _foreign = common::create_notification(pool, &dave, &mallory, "follow", None).await;
    ListingSeed {
        token,
        post_id: post.id,
        erin_id: erin.id,
        bob_id: bob.id,
        carol_id: carol.id,
        favourite,
        mention,
        follow,
    }
}

/// The `with_muted` seed: alice's rows by a notification-muted bob, a
/// blocked carol, a domain-blocked remote mallory, a timeline-only-muted
/// dave, and erin's two — a favourite in a thread alice muted and a plain
/// follow — oldest first.
struct MuteSeed {
    /// Alice's `read:notifications` token.
    token: String,
    /// Bob's row, hidden behind a mute that hides notifications.
    from_muted: Notification,
    /// Dave's row, under a mute that hides no notifications.
    from_timeline: Notification,
    /// Erin's favourite of the muted thread.
    on_muted_thread: Notification,
    /// Erin's plain follow.
    plain: Notification,
}

async fn seed_mutes(pool: &PgPool) -> MuteSeed {
    let (alice, token) = fixture(pool, "alice", READ).await;
    let bob = common::create_local_user(pool, "bob").await;
    let carol = common::create_local_user(pool, "carol").await;
    let dave = common::create_local_user(pool, "dave").await;
    let mallory = common::create_remote_user(pool, "mallory", "evil.example").await;
    let erin = common::create_local_user(pool, "erin").await;
    let post = common::create_status(pool, &erin, "a thread", Visibility::Public).await;
    let from_muted = common::create_notification(pool, &alice, &bob, "follow", None).await;
    let _from_blocked = common::create_notification(pool, &alice, &carol, "follow", None).await;
    let _from_domain = common::create_notification(pool, &alice, &mallory, "follow", None).await;
    let from_timeline = common::create_notification(pool, &alice, &dave, "mention", None).await;
    let on_muted_thread =
        common::create_notification(pool, &alice, &erin, "favourite", Some(&post)).await;
    let plain = common::create_notification(pool, &alice, &erin, "follow", None).await;
    let _muted = mute::mute(pool, &alice, &bob, MuteOptions::default())
        .await
        .expect("the mute fixture lands");
    let _timeline_only = mute::mute(
        pool,
        &alice,
        &dave,
        MuteOptions {
            notifications: false,
            expires_in: None,
        },
    )
    .await
    .expect("the timeline-only mute lands");
    let _blocked = block::block(pool, BASE, &alice, &carol)
        .await
        .expect("the block fixture lands");
    domain_block::block_domain(pool, alice.id, "evil.example")
        .await
        .expect("the domain block lands");
    conversation_mute::mute(pool, alice.id, post.conversation_id)
        .await
        .expect("the thread mute lands");
    MuteSeed {
        token,
        from_muted,
        from_timeline,
        on_muted_thread,
        plain,
    }
}
