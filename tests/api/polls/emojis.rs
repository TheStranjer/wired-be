//! `Poll.emojis` over HTTP: the custom emojis whose `:shortcode:` an
//! option title uses render on the poll entity, in the status render, the
//! poll read and the vote answer alike — for a local poll and for a poll
//! whose author is remote — and an emoji used only in the status body
//! never lands there.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::{
    custom_emoji::{self, NewCustomEmoji},
    poll::{self, PollParams},
    status::Visibility,
};

use super::support::{WRITE, accepted, user_with_token};
use super::vote_support::{marks, read_poll};
use crate::common;

/// The host the remote author lives on.
const REMOTE_HOST: &str = "remote.example";

/// Stores the emoji `shortcode` under `domain` (`None`: the local pack).
async fn store_emoji(pool: &PgPool, shortcode: &str, domain: Option<&str>) {
    let host = domain.unwrap_or("local");
    let new = NewCustomEmoji {
        domain: domain.map(str::to_owned),
        static_url: Some(format!("https://{host}/emoji/{shortcode}.png")),
        ..NewCustomEmoji::local(shortcode, &format!("https://{host}/emoji/{shortcode}.gif"))
    };
    let stored = custom_emoji::upsert(pool, &new)
        .await
        .expect("the fixture emoji stores");
    assert_eq!(
        stored.shortcode, shortcode,
        "the row stored is the one asked"
    );
}

/// The `shortcode`s of a poll entity's `emojis`, in the order served.
fn shortcodes(poll: &Value) -> Vec<&str> {
    poll["emojis"]
        .as_array()
        .expect("emojis is an array")
        .iter()
        .map(|emoji| emoji["shortcode"].as_str().expect("a shortcode string"))
        .collect()
}

/// `GET /api/v1/statuses/{id}` anonymously, answered as JSON.
async fn read_status(app: &Router, id: &str) -> Value {
    let response = common::TestRequest::get(&format!("/api/v1/statuses/{id}"))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "a public status read");
    response.json_value()
}

/// A local poll whose option uses `:wiredcat:` carries it on the create's
/// answer, the status read and the poll read, with `shortcode`, `url` and
/// `static_url`; `:blob:`, used only in the body, is not on the poll.
#[tokio::test]
async fn a_local_poll_renders_the_emoji_its_options_use() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    store_emoji(&pool, "wiredcat", None).await;
    store_emoji(&pool, "blob", None).await;

    let created = accepted(
        &app,
        &token,
        json!({
            "status": "body :blob:",
            "poll": {"options": [":wiredcat: yes", "no"], "expires_in": 3600},
        }),
    )
    .await;
    assert_eq!(
        shortcodes(&created["poll"]),
        ["wiredcat"],
        "the create answer"
    );
    let emoji = &created["poll"]["emojis"][0];
    assert_eq!(
        emoji["url"], "https://local/emoji/wiredcat.gif",
        "its image"
    );
    assert_eq!(
        emoji["static_url"], "https://local/emoji/wiredcat.png",
        "and its still"
    );
    let lists_blob = created["emojis"]
        .as_array()
        .expect("the status emojis")
        .iter()
        .any(|each| each["shortcode"] == "blob");
    assert!(lists_blob, "the status itself lists the body emoji");

    let id = created["id"].as_str().expect("the status id");
    let status = read_status(&app, id).await;
    assert_eq!(shortcodes(&status["poll"]), ["wiredcat"], "the status read");

    let poll_id = created["poll"]["id"].as_str().expect("the poll id");
    let read = read_poll(&app, poll_id, None).await;
    common::assert_answer(&read, StatusCode::OK, "the poll read");
    assert_eq!(
        shortcodes(&read.json_value()),
        ["wiredcat"],
        "the poll read"
    );
}

/// The vote answer carries the same emoji as the read.
#[tokio::test]
async fn the_vote_answer_carries_the_option_emoji() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", WRITE).await;
    let app = common::test_app(pool.clone());
    store_emoji(&pool, "wiredcat", None).await;

    let created = accepted(
        &app,
        &token,
        json!({"status": "pick", "poll": {"options": [":wiredcat: yes", "no"], "expires_in": 3600}}),
    )
    .await;
    let poll_id = created["poll"]["id"].as_str().expect("the poll id");

    let voted = marks(&app, &bob_token, poll_id, json!([0])).await;
    common::assert_answer(&voted, StatusCode::OK, "the ballot");
    assert_eq!(
        shortcodes(&voted.json_value()),
        ["wiredcat"],
        "the vote answer"
    );
}

/// A poll with no emoji in its options renders `emojis: []`, even when
/// the body uses one.
#[tokio::test]
async fn an_emoji_only_in_the_body_is_not_on_the_poll() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    store_emoji(&pool, "blob", None).await;

    let created = accepted(
        &app,
        &token,
        json!({"status": "only :blob: here", "poll": {"options": ["yes", "no"], "expires_in": 3600}}),
    )
    .await;
    assert_eq!(
        created["poll"]["emojis"],
        json!([]),
        "no option uses an emoji"
    );
}

/// A remote author's poll renders the emoji stored under their domain,
/// on the status read and the poll read.
#[tokio::test]
async fn a_remote_poll_renders_the_emoji_of_its_authors_domain() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    store_emoji(&pool, "wiredcat", Some(REMOTE_HOST)).await;
    let remote = common::create_remote_user(&pool, "carol", REMOTE_HOST).await;
    let posted = common::create_status(&pool, &remote, "which?", Visibility::Public).await;
    let params = PollParams {
        options: vec![":wiredcat: yes".to_owned(), "no".to_owned()],
        ..PollParams::default()
    };
    let mut conn = pool.acquire().await.expect("slot connection");
    let stored = poll::create(&mut conn, posted.id, &params)
        .await
        .expect("the fixture poll creates");
    drop(conn);

    let status = read_status(&app, &posted.id.to_string()).await;
    assert_eq!(shortcodes(&status["poll"]), ["wiredcat"], "the status read");
    let emoji = &status["poll"]["emojis"][0];
    assert_eq!(
        emoji["url"],
        format!("https://{REMOTE_HOST}/emoji/wiredcat.gif"),
        "the remote emoji's own image"
    );

    let read = read_poll(&app, &stored.id.to_string(), None).await;
    common::assert_answer(&read, StatusCode::OK, "the poll read");
    assert_eq!(
        shortcodes(&read.json_value()),
        ["wiredcat"],
        "the poll read"
    );
}
