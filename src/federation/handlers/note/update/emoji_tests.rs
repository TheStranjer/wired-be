//! Tests of the custom emoji the edit fold stores from an `Update`'s `Emoji`
//! tags.

use serde_json::{Value, json};
use sqlx::PgPool;

use super::super::test_support::{app_state, author, author_again, mirrored, note, view_of};
use super::super::update_note;
use crate::{
    federation::dispatch::Outcome,
    models::custom_emoji::{self, CustomEmoji},
};

/// The domain of the test author's account.
const DOMAIN: &str = "remote.example";

/// An `Update` of `uri` to `text` stamped `updated`, announcing the emoji
/// `shortcode` with the image at `url` when one is given.
fn edit_with(uri: &str, text: &str, updated: &str, emoji: Option<(&str, &str)>) -> Value {
    let tags: Vec<Value> = emoji
        .into_iter()
        .map(|(shortcode, url)| {
            json!({
                "type": "Emoji",
                "name": format!(":{shortcode}:"),
                "icon": { "type": "Image", "url": url },
            })
        })
        .collect();
    note(
        uri,
        json!({
            "content": format!("<p>{text}</p>"),
            "updated": updated,
            "tag": tags,
        }),
    )
}

/// The stored emojis of the author's domain named `shortcode`.
async fn stored_emoji(pool: &PgPool, shortcode: &str) -> Vec<CustomEmoji> {
    custom_emoji::get_by_shortcodes(pool, &[shortcode.to_owned()], Some(DOMAIN))
        .await
        .expect("the emoji read")
}

/// How many emoji rows the database holds.
async fn emoji_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM custom_emojis")
        .fetch_one(pool)
        .await
        .expect("the emoji count reads")
}

#[tokio::test]
async fn an_edit_stores_an_emoji_first_used_in_it_under_the_authors_domain() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let author = author(&pool).await;
    let uri = "https://remote.example/users/bob/statuses/emoji-edit-1";
    drop(mirrored(&pool, &author, uri, "first words").await);
    assert!(
        stored_emoji(&pool, "mastonew").await.is_empty(),
        "the emoji is unknown before the edit"
    );

    let edit = edit_with(
        uri,
        "first words with :mastonew:",
        "2024-05-01T12:00:00.000Z",
        Some(("mastonew", "https://remote.example/emoji/mastonew.png")),
    );
    let outcome = update_note(
        state,
        view_of("Update", edit, None),
        author_again(&pool).await,
    )
    .await
    .expect("the fold completes");

    assert_eq!(outcome, Outcome::Handled, "the edit is folded");
    let stored = stored_emoji(&pool, "mastonew").await;
    assert_eq!(
        stored.len(),
        1,
        "the emoji is stored under the author's domain"
    );
    assert_eq!(
        stored.first().map(|row| row.image_url.as_str()),
        Some("https://remote.example/emoji/mastonew.png"),
        "with the image the tag names"
    );
}

#[tokio::test]
async fn an_edit_refreshes_a_known_emojis_image_and_keeps_its_moderator_flags() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let author = author(&pool).await;
    let uri = "https://remote.example/users/bob/statuses/emoji-edit-2";
    drop(mirrored(&pool, &author, uri, "first words").await);
    custom_emoji::upsert_remote(
        &pool,
        "blobcat",
        Some(DOMAIN),
        "https://remote.example/emoji/old.png",
    )
    .await
    .expect("the known emoji lands");
    sqlx::query("UPDATE custom_emojis SET disabled = true WHERE shortcode = 'blobcat'")
        .execute(&pool)
        .await
        .map(drop)
        .expect("the moderator flag is set");

    let edit = edit_with(
        uri,
        "first words :blobcat:",
        "2024-05-01T12:00:00.000Z",
        Some(("blobcat", "https://remote.example/emoji/new.png")),
    );
    let outcome = update_note(
        state,
        view_of("Update", edit, None),
        author_again(&pool).await,
    )
    .await
    .expect("the fold completes");

    assert_eq!(outcome, Outcome::Handled, "the edit is folded");
    let stored = stored_emoji(&pool, "blobcat").await;
    assert_eq!(stored.len(), 1, "still one row for the emoji");
    assert_eq!(
        stored.first().map(|row| row.image_url.as_str()),
        Some("https://remote.example/emoji/new.png"),
        "the image url follows the edit"
    );
    assert!(
        stored.first().is_some_and(|row| row.disabled),
        "and the moderator's flag is kept"
    );
}

#[tokio::test]
async fn an_edit_without_emoji_tags_stores_no_emoji() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let author = author(&pool).await;
    let uri = "https://remote.example/users/bob/statuses/emoji-edit-3";
    drop(mirrored(&pool, &author, uri, "first words").await);

    let edit = edit_with(uri, "plain edit", "2024-05-01T12:00:00.000Z", None);
    let outcome = update_note(
        state,
        view_of("Update", edit, None),
        author_again(&pool).await,
    )
    .await
    .expect("the fold completes");

    assert_eq!(outcome, Outcome::Handled, "the edit is folded");
    assert_eq!(emoji_count(&pool).await, 0, "no emoji rows appear");
}

#[tokio::test]
async fn a_stale_edit_does_not_store_its_emoji() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let author = author(&pool).await;
    let uri = "https://remote.example/users/bob/statuses/emoji-edit-4";
    drop(mirrored(&pool, &author, uri, "first words").await);
    let newer = edit_with(uri, "newer words", "2024-05-01T12:00:05.000Z", None);
    let outdated = edit_with(
        uri,
        "older words :ghost:",
        "2024-05-01T12:00:00.000Z",
        Some(("ghost", "https://remote.example/emoji/ghost.png")),
    );

    let first = update_note(
        state.clone(),
        view_of("Update", newer, None),
        author_again(&pool).await,
    )
    .await
    .expect("the newer edit folds");
    assert_eq!(first, Outcome::Handled, "the newer edit applies");
    let late = update_note(state, view_of("Update", outdated, None), author)
        .await
        .expect("the stale edit folds");

    assert_eq!(
        late,
        Outcome::Ignored("an Update no newer than the stored version"),
        "the older edit is ignored"
    );
    assert_eq!(emoji_count(&pool).await, 0, "and its emoji is not stored");
}
