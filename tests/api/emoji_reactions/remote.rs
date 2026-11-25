//! Reacting with a custom emoji of another instance, named `shortcode@domain`.

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use wired_be::models::{
    custom_emoji::{self, NewCustomEmoji},
    emoji_reaction::{self, NewReaction},
};

use super::support::{
    INVALID, emoji, field, listed, public_post, put, react_ok, reacted, reader_and_writer, refused,
    stored, summary, withdraw_ok,
};
use crate::common;

/// The instance the fixture emoji belongs to.
const DOMAIN: &str = "misskey.test";
/// The remote emoji's API name.
const NAME: &str = "misskeycat@misskey.test";
/// Where the remote emoji's image lives.
const URL: &str = "https://misskey.test/files/misskeycat.png";

/// Teaches the instance `shortcode` of [`DOMAIN`], as a remote post would.
async fn remote_emoji(pool: &PgPool, shortcode: &str) {
    let stored_row = custom_emoji::upsert(
        pool,
        &NewCustomEmoji {
            shortcode: shortcode.to_owned(),
            domain: Some(DOMAIN.to_owned()),
            image_url: URL.to_owned(),
            ..NewCustomEmoji::default()
        },
    )
    .await
    .expect("the remote emoji upserts");
    assert_eq!(
        stored_row.domain.as_deref(),
        Some(DOMAIN),
        "the fixture emoji is the remote one"
    );
}

#[tokio::test]
async fn a_remote_emoji_is_reacted_with_in_either_spelling_and_named_for_its_domain() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    remote_emoji(&pool, "misskeycat").await;

    let body = reacted(&app, &token, &status_id, NAME).await;
    assert_eq!(
        body["pleroma"]["emoji_reactions"],
        json!([summary(NAME, 1, true, &json!(URL), &[&alice.id])]),
        "named shortcode@domain, with the remote image"
    );
    react_ok(&app, &token, &status_id, ":misskeycat@misskey.test:").await;
    assert_eq!(
        stored(&pool).await,
        vec![(
            alice.id.to_string(),
            status_id.to_string(),
            ":misskeycat:".to_owned()
        )],
        "the coloned spelling is the same reaction, stored as the shortcode"
    );
    let row: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT url, emoji_domain FROM emoji_reactions")
            .fetch_one(&pool)
            .await
            .expect("the row reads");
    assert_eq!(
        row,
        (Some(URL.to_owned()), Some(DOMAIN.to_owned())),
        "the remote image and its origin are stored beside it"
    );

    withdraw_ok(&app, &token, &status_id.to_string(), NAME).await;
    assert!(
        stored(&pool).await.is_empty(),
        "unreacting with the same spelling removes it"
    );
    react_ok(&app, &token, &status_id, NAME).await;
    withdraw_ok(
        &app,
        &token,
        &status_id.to_string(),
        ":misskeycat@misskey.test:",
    )
    .await;
    assert!(
        stored(&pool).await.is_empty(),
        "and so does the coloned one"
    );
}

#[tokio::test]
async fn an_unknown_or_retired_remote_emoji_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    remote_emoji(&pool, "misskeycat").await;
    remote_emoji(&pool, "retired").await;
    let retired =
        sqlx::query("UPDATE custom_emojis SET disabled = true WHERE shortcode = 'retired'")
            .execute(&pool)
            .await
            .expect("the fixture retires the emoji")
            .rows_affected();
    assert_eq!(retired, 1, "the emoji was there to retire");

    let status_text = status_id.to_string();
    for unusable in [
        "nosuch@misskey.test",
        ":nosuch@misskey.test:",
        "misskeycat@other.example",
        "retired@misskey.test",
        "misskeycat",
        "misskeycat@",
        ":misskeycat@misskey.test",
    ] {
        refused(
            &put(&app, &token, &status_text, unusable).await,
            StatusCode::BAD_REQUEST,
            INVALID,
            unusable,
        );
    }
    assert!(stored(&pool).await.is_empty(), "no refusal stored a row");
}

#[tokio::test]
async fn a_local_and_a_remote_emoji_of_one_shortcode_are_two_reactions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    remote_emoji(&pool, "misskeycat").await;
    emoji(&pool, "misskeycat").await;

    react_ok(&app, &token, &status_id, NAME).await;
    react_ok(&app, &token, &status_id, ":misskeycat:").await;
    assert_eq!(stored(&pool).await.len(), 2, "two reactions, not one");

    withdraw_ok(&app, &token, &status_id.to_string(), NAME).await;
    let left = listed(&app, Some(&token), &status_id, None).await;
    assert_eq!(
        left[0]["name"], "misskeycat",
        "the local emoji is still reacted with"
    );
    assert_eq!(left.as_array().map(Vec::len), Some(1), "and it is alone");
}

#[tokio::test]
async fn a_remote_and_a_local_reactor_of_one_remote_emoji_share_an_entry() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    remote_emoji(&pool, "misskeycat").await;
    let dave = common::create_remote_user(&pool, "dave", DOMAIN).await;
    let mirrored = emoji_reaction::react_in_domain(
        &pool,
        &NewReaction {
            user_id: dave.id,
            status_id,
            emoji: ":misskeycat:",
            url: Some(URL),
        },
        "https://misskey.test/activities/react-1",
        Some(DOMAIN),
    )
    .await
    .expect("the mirrored reaction stores");
    assert_eq!(mirrored.emoji_domain.as_deref(), Some(DOMAIN), "its origin");

    react_ok(&app, &token, &status_id, NAME).await;

    let entries = field(&app, Some(&token), &status_id).await;
    assert_eq!(
        entries,
        json!([summary(NAME, 2, true, &json!(URL), &[&dave.id, &alice.id])]),
        "one entry, counted twice, the remote reactor first"
    );
    let all = listed(&app, Some(&token), &status_id, None).await;
    assert_eq!(all[0]["count"], 2, "the reactions read agrees");
    let one = listed(&app, Some(&token), &status_id, Some(NAME)).await;
    assert_eq!(one[0]["name"], NAME, "the single-emoji read takes the name");
    let colons = listed(
        &app,
        Some(&token),
        &status_id,
        Some(":misskeycat@misskey.test:"),
    )
    .await;
    assert_eq!(colons, one, "with or without colons");
    let local = listed(&app, Some(&token), &status_id, Some(":misskeycat:")).await;
    assert_eq!(local, json!([]), "the plain shortcode is the local emoji");
    let bare = listed(&app, Some(&token), &status_id, Some("misskeycat")).await;
    assert_eq!(bare, json!([]), "and so is the bare one");
}
