//! Flat notification dismissal and reading over HTTP: `POST
//! /api/v1/notifications/clear`, the two dismissals, `DELETE
//! /api/v1/notifications/destroy_multiple` and `POST
//! /api/pleroma/notifications/read` (`src/api/notifications.rs`).
//!
//! Every test seeds the `notifications` rows directly through the
//! `create_notification` fixture and observes dismissal through the grouped
//! v2 listing (`src/api/notifications_v2.rs` shares the rows and the soft
//! stamp): dismissed notifications vanish from every read while the rows
//! stay, the `read` clauses mark seen by id order (`id` reads that row and
//! its elders, `max_id` reads up to the cursor and answers the marked
//! array), deleting a status takes its notifications with it, and every
//! route demands a user credential with `write:notifications`.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    models::{
        mute::{self, MuteOptions},
        notification::Notification,
        status::Visibility,
        user::User,
    },
    util::time::mastodon_time,
};

use crate::common;

/// The v1 dismissal routes' mount point.
const V1: &str = "/api/v1/notifications";
/// Pleroma's read route.
const READ_ROUTE: &str = "/api/pleroma/notifications/read";
/// The scope every route in this file demands.
const WRITE: &str = "write:notifications";
/// Both halves, for fixtures that also observe through the v2 listing.
const READ_WRITE: &str = "read:notifications write:notifications";
/// The message of the shared `AppError::NotFound` body.
const NOT_FOUND: &str = "Record not found";

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
    /// The verb, `"POST"` or `"DELETE"`.
    verb: &'static str,
    /// The request target, query string included.
    path: String,
    /// Whether to send an `id` body with the request.
    with_id_body: bool,
}

/// The five routes of this file, all mounted on `id` where they name one.
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

/// Send `route` with the bearer `token`; empty `token` sends no
/// credential.
async fn call(app: &Router, route: &GatedRoute, token: &str, id: Uuid) -> common::TestResponse {
    let mut request = match route.verb {
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

/// Call every gated route with `token` (empty: unauthenticated).
async fn call_every_route(app: &Router, token: &str, id: Uuid) -> Vec<common::TestResponse> {
    let mut responses = Vec::new();
    for route in gated_routes(id) {
        responses.push(call(app, &route, token, id).await);
    }
    responses
}

/// Every route demands `write:notifications`: a `read:notifications`
/// token is refused with Pleroma's wording, and the root `write` grant
/// passes the hierarchy.
#[tokio::test]
async fn every_route_demands_write_notifications() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let narrow = common::create_app_and_token(&pool, &alice, "read:notifications").await;
    let write_root = common::create_app_and_token(&pool, &alice, "write").await;
    let app = common::test_app(pool);

    let refused = call_every_route(&app, &narrow, note.id).await;
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
