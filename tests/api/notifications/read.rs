//! `POST /api/pleroma/notifications/read`: the `id` clause marks a row and
//! its elders seen, `max_id` marks up to the cursor and answers the marked
//! array, foreign rows are refused, a muted actor still reads, and
//! deleting a status takes its notifications with it.

use axum::http::StatusCode;
use serde_json::json;
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

use super::support::{
    NOT_FOUND, READ_ROUTE, READ_WRITE, fixture, is_seen, listed_keys, post_json, stored_row,
    ungrouped, unread,
};
use crate::common;

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
