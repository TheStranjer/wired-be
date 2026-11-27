//! `POST /api/v1/pleroma/notifications/read` serves the same handler as
//! the legacy `POST /api/pleroma/notifications/read`: `id` and `max_id`
//! mark the same rows and answer the same bodies on identical fixtures,
//! and the refusals match.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::notification::Notification;

use super::support::{
    NOT_FOUND, READ_ROUTE, READ_ROUTE_V1, READ_WRITE, fixture, is_seen, post_json, unread,
};
use crate::common;

/// One user's three notifications and token, oldest first.
struct Seed {
    /// The user's `read:notifications write:notifications` token.
    token: String,
    /// The rows, oldest first.
    notes: Vec<Notification>,
}

/// Seed `owner` with a follow, a mention and a favourite, named after `tag`.
async fn seed_rows(pool: &PgPool, tag: &str) -> Seed {
    let (owner, token) = fixture(pool, &format!("owner{tag}"), READ_WRITE).await;
    let actor = common::create_local_user(pool, &format!("actor{tag}")).await;
    let post = common::create_status(
        pool,
        &actor,
        "hello",
        wired_be::models::status::Visibility::Public,
    )
    .await;
    let oldest = common::create_notification(pool, &owner, &actor, "follow", None).await;
    let middle = common::create_notification(pool, &owner, &actor, "mention", None).await;
    let newest = common::create_notification(pool, &owner, &actor, "favourite", Some(&post)).await;
    Seed {
        token,
        notes: vec![oldest, middle, newest],
    }
}

/// The `is_seen` flag of every stored row of `seed`, oldest first.
async fn seen_flags(pool: &PgPool, seed: &Seed) -> Vec<bool> {
    let mut flags = Vec::new();
    for note in &seed.notes {
        flags.push(is_seen(pool, note.id).await);
    }
    flags
}

/// The answer reduced to what is independent of the row ids: the status
/// code and, per entity, its type and `is_seen` flag.
fn shape(response: &common::TestResponse) -> Shape {
    let body = response.json_value();
    let entities: Vec<Value> = body
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![body.clone()]);
    let entities = entities
        .iter()
        .map(|entity| {
            (
                entity["type"].as_str().unwrap_or_default().to_owned(),
                entity["pleroma"]["is_seen"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    Shape {
        status: response.status,
        entities,
    }
}

/// An answer with its row ids removed.
#[derive(Debug, PartialEq, Eq)]
struct Shape {
    /// The HTTP status.
    status: StatusCode,
    /// Per entity: its `type` and `pleroma.is_seen`.
    entities: Vec<(String, bool)>,
}

/// The `body` for `clause` over `seed`'s row at `index`.
fn clause_body(seed: &Seed, clause: &str, index: usize) -> Value {
    let id = seed.notes.get(index).expect("a seeded row").id;
    json!({ clause: id })
}

/// Run `clause` at `index` against both routes on identical fixtures and
/// assert the same answer shape and the same stored state.
async fn assert_parity(clause: &str, index: usize) {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let legacy = seed_rows(&pool, "a").await;
    let v1 = seed_rows(&pool, "b").await;
    let app = common::test_app(pool.clone());

    let old = post_json(
        &app,
        READ_ROUTE,
        &legacy.token,
        clause_body(&legacy, clause, index),
    )
    .await;
    let new = post_json(
        &app,
        READ_ROUTE_V1,
        &v1.token,
        clause_body(&v1, clause, index),
    )
    .await;
    common::assert_answer(&old, StatusCode::OK, "the legacy read");
    common::assert_answer(&new, StatusCode::OK, "the v1 read");

    assert_eq!(shape(&new), shape(&old), "the same answer on both paths");
    assert_eq!(
        seen_flags(&pool, &v1).await,
        seen_flags(&pool, &legacy).await,
        "the same rows marked on both paths"
    );
    assert_eq!(
        unread(&app, &v1.token).await,
        unread(&app, &legacy.token).await,
        "the same unread count afterwards"
    );
}

/// `id` marks that row and its elders on the v1 path exactly as on the
/// legacy one, and leaves the newer row unseen.
#[tokio::test]
async fn id_clause_matches_the_legacy_path() {
    assert_parity("id", 1).await;

    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_rows(&pool, "c").await;
    let app = common::test_app(pool.clone());
    let response = post_json(
        &app,
        READ_ROUTE_V1,
        &seed.token,
        clause_body(&seed, "id", 1),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the v1 read by id");
    assert_eq!(
        response.json_value()["id"],
        seed.notes[1].id.to_string(),
        "the named notification is answered"
    );
    assert_eq!(
        seen_flags(&pool, &seed).await,
        vec![true, true, false],
        "the row and its elder are seen, the newer one is not"
    );
}

/// `max_id` marks up to the cursor on the v1 path exactly as on the
/// legacy one and answers the marked array.
#[tokio::test]
async fn max_id_clause_matches_the_legacy_path() {
    assert_parity("max_id", 1).await;

    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_rows(&pool, "c").await;
    let app = common::test_app(pool.clone());
    let response = post_json(
        &app,
        READ_ROUTE_V1,
        &seed.token,
        clause_body(&seed, "max_id", 1),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the v1 read by max_id");
    assert_eq!(
        response.json_value().as_array().map(Vec::len),
        Some(2),
        "the cursor row and its elder are answered"
    );
    assert_eq!(
        seen_flags(&pool, &seed).await,
        vec![true, true, false],
        "everything up to the cursor is seen"
    );
}

/// An unknown id and a body naming neither parameter get the legacy
/// path's refusals on the v1 path.
#[tokio::test]
async fn errors_match_the_legacy_path() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_rows(&pool, "a").await;
    let app = common::test_app(pool.clone());

    let missing = json!({"id": uuid::Uuid::now_v7()});
    for payload in [missing, json!({})] {
        let old = post_json(&app, READ_ROUTE, &seed.token, payload.clone()).await;
        let new = post_json(&app, READ_ROUTE_V1, &seed.token, payload).await;
        assert_ne!(old.status, StatusCode::OK, "the legacy path refuses");
        assert_eq!(new.status, old.status, "the same status");
        assert_eq!(new.json_value(), old.json_value(), "the same error body");
    }
    let unknown = post_json(
        &app,
        READ_ROUTE_V1,
        &seed.token,
        json!({"id": uuid::Uuid::now_v7()}),
    )
    .await;
    common::assert_error(&unknown, NOT_FOUND, "an unknown id on the v1 path");
}

/// Without a token the v1 path refuses like the legacy one.
#[tokio::test]
async fn anonymous_refusal_matches_the_legacy_path() {
    let db = common::test_db().await;
    let app: Router = common::test_app(db.pool().clone());

    let old = post_json(&app, READ_ROUTE, "", json!({"max_id": "1"})).await;
    let new = post_json(&app, READ_ROUTE_V1, "", json!({"max_id": "1"})).await;
    common::assert_answer(&old, StatusCode::UNAUTHORIZED, "the legacy path");
    common::assert_answer(&new, StatusCode::UNAUTHORIZED, "the v1 path");
    assert_eq!(new.json_value(), old.json_value(), "the same refusal body");
}
