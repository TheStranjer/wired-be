//! WBE-87's `preview` parameter: every check and the full render of the
//! entity a create would answer, with no row, no side table, no counter, no
//! hook and no key — read from the query or either body encoding, refusing
//! exactly what the create refuses, and ignoring an `Idempotency-Key`.

use axum::http::StatusCode;
use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    models::status,
    util::{
        entities::{hashtag_html, mention_html},
        time::parse_time,
    },
};

use super::support::{
    BAD_CONTENT_TYPE, BAD_VISIBILITY, EMPTY_STATUS, STATUSES, WRITE, author_now, id_of, key_rows,
    publish, publish_keyed, stored_count, user_with_token,
};
use crate::common;

// ─────────────  POST /api/v1/statuses: preview  ─────────────

/// The three side tables a create fills: every preview test requires them
/// all empty, because the point of a preview is that it renders without
/// writing.
async fn side_rows(pool: &PgPool) -> (i64, i64, i64) {
    let mentions = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM status_mentions")
        .fetch_one(pool)
        .await
        .expect("the mention count reads");
    let tags = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM hashtags")
        .fetch_one(pool)
        .await
        .expect("the tag count reads");
    let links = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM status_hashtags")
        .fetch_one(pool)
        .await
        .expect("the tag link count reads");
    (mentions, tags, links)
}

/// A preview answers the [`Status`] entity a create of the same body would
/// have: fresh id with the urls it derives, the render with the mention and
/// the tag resolved into it, zero counters, `created_at` now, the author's
/// view — and no row anywhere behind it.
#[tokio::test]
async fn a_preview_answers_the_entity_and_writes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let response = publish(
        &app,
        Some(token.as_str()),
        json!({"status": "hello @bob and @nobody, see #Rust", "preview": true}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "a preview");
    let value = response.json_value();
    let id = id_of(&value);

    assert_eq!(
        value["uri"],
        format!("{TEST_BASE_URL}/objects/{id}"),
        "the entity carries the id a create would mint, with its urls"
    );
    assert_eq!(value["url"], json!(format!("{TEST_BASE_URL}/notice/{id}")));
    assert_eq!(
        value["content"],
        format!(
            "<p>hello {} and @nobody, see {}</p>",
            mention_html(&bob.ap_id, "bob"),
            hashtag_html(TEST_BASE_URL, "rust"),
        ),
        "the same render a create stores, the unknown name still plain text"
    );
    assert_eq!(
        value["mentions"],
        json!([{ "id": bob.id.to_string(), "username": "bob",
                 "url": bob.ap_id, "acct": "bob" }]),
        "the mention is resolved into the entity"
    );
    assert_eq!(
        value["tags"],
        json!([{ "name": "rust", "url": format!("{TEST_BASE_URL}/tags/rust") }]),
        "and so is the tag, folded"
    );
    assert_eq!(value["reblogs_count"], 0, "every counter zero");
    assert_eq!(value["favourites_count"], 0, "all four");
    assert_eq!(value["replies_count"], 0);
    assert_eq!(value["pleroma"]["quotes_count"], 0);
    assert_eq!(value["visibility"], "public", "the author's defaults apply");
    assert_eq!(value["account"]["acct"], "alice", "as the author sees it");
    assert_eq!(
        value["pleroma"]["content"],
        json!({ "text/plain": "hello @bob and @nobody, see #Rust" }),
        "the source text beside the render"
    );
    let created_at = parse_time(value["created_at"].as_str().expect("a timestamp"))
        .expect("the Mastodon-format stamp parses");
    assert!(
        (Utc::now() - created_at).num_seconds().abs() < 120,
        "created_at is now: {created_at}"
    );

    assert_eq!(stored_count(&pool).await, 0, "no status row");
    assert_eq!(side_rows(&pool).await, (0, 0, 0), "no mention or tag rows");
    assert_eq!(key_rows(&pool).await, 0, "and no claim");
    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        0,
        "and the author's count stands"
    );
    assert!(
        status::get_by_id(&pool, id).await.unwrap().is_none(),
        "the previewed id names nothing in the database"
    );
}

/// The flag is a [`flex`] boolean read through the query string as well as
/// either body encoding (decision 10), and every false spelling — the
/// absence of the key included — stays a real post.
///
/// [`flex`]: wired_be::extract::flex
#[tokio::test]
async fn the_preview_flag_reads_every_parameter_spelling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let from_form = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "previewed from a form"), ("preview", "true")])
        .send(&app)
        .await;
    common::assert_answer(&from_form, StatusCode::OK, "a form preview");
    assert_eq!(
        stored_count(&pool).await,
        0,
        "the form preview wrote nothing"
    );

    let from_query = common::TestRequest::post(&format!("{STATUSES}?preview=1"))
        .bearer(&token)
        .json(&json!({"status": "previewed via the query"}))
        .send(&app)
        .await;
    common::assert_answer(&from_query, StatusCode::OK, "a query preview");
    assert_eq!(stored_count(&pool).await, 0, "and the query wrote nothing");

    let spelled_off = publish(
        &app,
        Some(token.as_str()),
        json!({"status": "a real post", "preview": "off"}),
    )
    .await;
    common::assert_answer(&spelled_off, StatusCode::OK, "the `off` spelling posts");
    assert_eq!(stored_count(&pool).await, 1, "and it stored its row");

    let bare = publish(&app, Some(token.as_str()), json!({"status": "no flag"})).await;
    common::assert_answer(&bare, StatusCode::OK, "the absent flag posts");
    assert_eq!(stored_count(&pool).await, 2, "as it always did");
}

/// Validation runs on previews exactly as on posts: the same 422s in the
/// same wordings, nothing written, and no key stored even when the request
/// carried one.
#[tokio::test]
async fn a_preview_owes_the_creation_refusals() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for (body, message) in [
        (json!({"status": "   ", "preview": true}), EMPTY_STATUS),
        (
            json!({"status": "hi", "visibility": "sideways", "preview": true}),
            BAD_VISIBILITY,
        ),
        (
            json!({"status": "hi", "content_type": "text/rtf", "preview": true}),
            BAD_CONTENT_TYPE,
        ),
        (
            json!({"status": "hi", "spoiler_text": "x".repeat(201), "preview": true}),
            "Validation failed: Spoiler text is too long (maximum is 200 characters)",
        ),
    ] {
        let response = publish(&app, Some(token.as_str()), body.clone()).await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("the refused preview `{body}`"),
        );
        common::assert_error(&response, message, &format!("the wording for `{body}`"));
    }

    let keyed_refusal =
        publish_keyed(&app, &token, "K-1", json!({"status": "", "preview": true})).await;
    common::assert_answer(
        &keyed_refusal,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a refused preview that carried a key",
    );

    assert_eq!(stored_count(&pool).await, 0, "not one refusal wrote a row");
    assert_eq!(key_rows(&pool).await, 0, "or stored a key");
}

/// A preview is not a create, so there is nothing for a key to remember:
/// previews under one key mint fresh ids and claim nothing, and the real
/// post that follows the previews claims the key as if it had never been
/// spoken.
#[tokio::test]
async fn preview_ignores_the_idempotency_key() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let first = publish_keyed(
        &app,
        &token,
        "K-1",
        json!({"status": "first look", "preview": true}),
    )
    .await;
    let second = publish_keyed(
        &app,
        &token,
        "K-1",
        json!({"status": "second look", "preview": true}),
    )
    .await;
    common::assert_answer(&first, StatusCode::OK, "the first preview");
    common::assert_answer(&second, StatusCode::OK, "the second preview");
    assert_ne!(
        id_of(&first.json_value()),
        id_of(&second.json_value()),
        "each preview mints its own id"
    );
    assert_eq!(stored_count(&pool).await, 0);
    assert_eq!(key_rows(&pool).await, 0, "and the key stayed unclaimed");

    let real = publish_keyed(&app, &token, "K-1", json!({"status": "the real post"})).await;
    common::assert_answer(&real, StatusCode::OK, "the post after the previews");
    assert_eq!(key_rows(&pool).await, 1, "now the key is claimed");
    let replay = publish_keyed(&app, &token, "K-1", json!({"status": "ignored"})).await;
    assert_eq!(
        replay.json_value()["id"],
        real.json_value()["id"],
        "and replays the post it finally created"
    );
    assert_eq!(stored_count(&pool).await, 1);
}
