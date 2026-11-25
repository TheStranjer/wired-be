//! A local custom emoji named by its bare shortcode, as Pleroma does: accepted
//! on the reaction routes next to `:shortcode:`, and listed without colons.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    CAT_URL, INVALID, PARTY, THUMB, disabled_emoji, emoji, field, listed, public_post, put,
    react_ok, reacted, reader_and_writer, refused, remove, stored, summary, withdraw_ok,
};
use crate::common;

#[tokio::test]
async fn a_bare_shortcode_reacts_and_is_the_same_reaction_as_the_colon_spelling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    emoji(&pool, "wiredcat").await;

    let body = reacted(&app, &token, &status_id, "wiredcat").await;
    assert_eq!(
        body["pleroma"]["emoji_reactions"],
        json!([summary("wiredcat", 1, true, &json!(CAT_URL), &[&alice.id])]),
        "listed bare, with the emoji's image"
    );
    react_ok(&app, &token, &status_id, ":wiredcat:").await;
    assert_eq!(
        stored(&pool).await,
        vec![(
            alice.id.to_string(),
            status_id.to_string(),
            ":wiredcat:".to_owned()
        )],
        "the colon spelling is the same reaction, stored with colons"
    );
    let entries = field(&app, Some(&token), &status_id).await;
    assert_eq!(
        entries,
        json!([summary("wiredcat", 1, true, &json!(CAT_URL), &[&alice.id])]),
        "count 1 after both spellings"
    );
}

#[tokio::test]
async fn a_bare_shortcode_withdraws_a_reaction_made_with_colons() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    emoji(&pool, "wiredcat").await;

    react_ok(&app, &token, &status_id, ":wiredcat:").await;
    withdraw_ok(&app, &token, &status_id.to_string(), "wiredcat").await;
    assert!(
        stored(&pool).await.is_empty(),
        "the bare spelling removed it"
    );
}

#[tokio::test]
async fn the_single_emoji_read_matches_the_bare_and_colon_spellings() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    emoji(&pool, "wiredcat").await;
    react_ok(&app, &token, &status_id, "wiredcat").await;
    react_ok(&app, &token, &status_id, PARTY).await;

    let all = listed(&app, Some(&token), &status_id, None).await;
    assert_eq!(all[0]["name"], "wiredcat", "Pleroma's name for a local one");
    let bare = listed(&app, Some(&token), &status_id, Some("wiredcat")).await;
    let colons = listed(&app, Some(&token), &status_id, Some(":wiredcat:")).await;
    assert_eq!(bare.as_array().map(Vec::len), Some(1), "only that emoji");
    assert_eq!(bare[0]["name"], "wiredcat", "named bare");
    assert_eq!(bare[0]["url"], json!(CAT_URL), "with its image");
    assert_eq!(bare, colons, "both spellings read the same reaction");
    let unicode = listed(&app, Some(&token), &status_id, Some(THUMB)).await;
    assert_eq!(unicode, json!([]), "an emoji nobody used stays empty");
}

#[tokio::test]
async fn an_unknown_or_retired_bare_word_is_still_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    emoji(&pool, "wiredcat").await;
    disabled_emoji(&pool, "gone").await;

    let status_text = status_id.to_string();
    for unusable in ["nosuchemoji", "gone", "wired cat", "wiredcat@"] {
        refused(
            &put(&app, &token, &status_text, unusable).await,
            StatusCode::BAD_REQUEST,
            INVALID,
            unusable,
        );
        refused(
            &remove(&app, &token, &status_text, unusable).await,
            StatusCode::BAD_REQUEST,
            INVALID,
            unusable,
        );
    }
    assert_eq!(
        field(&app, Some(&token), &status_id).await,
        Value::Array(Vec::new()),
        "no refusal left a reaction behind"
    );
}
