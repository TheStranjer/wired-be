//! The custom emojis of the history: each version lists the emojis its own
//! text used.

use wired_be::models::custom_emoji::{self, NewCustomEmoji};

use super::*;

/// Stores a local emoji named `shortcode` served from the `wired` pack.
async fn local_emoji(pool: &PgPool, shortcode: &str) {
    let stored = custom_emoji::upsert(
        pool,
        &NewCustomEmoji {
            category: Some("wired".to_owned()),
            ..NewCustomEmoji::local(
                shortcode,
                &format!("{TEST_BASE_URL}/emoji/wired/{shortcode}.png"),
            )
        },
    )
    .await
    .expect("the fixture emoji stores");
    assert_eq!(
        stored.shortcode, shortcode,
        "the fixture row is the one asked"
    );
}

/// The `shortcode`s of an entry's `emojis` array, in the order served.
fn shortcodes_of(entry: &Value) -> Vec<&str> {
    entry["emojis"]
        .as_array()
        .expect("emojis is an array")
        .iter()
        .map(|emoji| emoji["shortcode"].as_str().expect("a shortcode string"))
        .collect()
}

/// An edit that swaps one emoji for another leaves each version with its
/// own: the superseded one lists what it used, the current one what it
/// uses now, and a third emoji nobody used appears nowhere.
#[tokio::test]
async fn each_version_of_the_history_lists_the_emojis_its_text_used() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "emoji-editor").await;
    local_emoji(&pool, "wiredcat").await;
    local_emoji(&pool, "blob").await;
    local_emoji(&pool, "unused").await;
    let row = post(&pool, &alice, "first :wiredcat:").await;

    let edited = edit_row(&app, &token, &row, json!({"status": "second :blob:"})).await;
    common::assert_answer(&edited, StatusCode::OK, "the edit is accepted");

    let response = history(&app, None, &row.id.to_string()).await;
    common::assert_answer(&response, StatusCode::OK, "the history reads");
    let body = response.json_value();
    let versions = body.as_array().expect("the history is an array");
    assert_eq!(versions.len(), 2, "one superseded version and the current");
    let superseded = versions.first().expect("the superseded version");
    let current = versions.get(1).expect("the current version");
    assert_eq!(
        shortcodes_of(superseded),
        vec!["wiredcat"],
        "the first version used its own emoji"
    );
    assert_eq!(
        shortcodes_of(current),
        vec!["blob"],
        "the current version uses the one it was edited to"
    );
    let first = &superseded["emojis"][0];
    assert_eq!(
        first["url"],
        format!("{TEST_BASE_URL}/emoji/wired/wiredcat.png"),
        "the entity carries the stored image url"
    );
    assert_eq!(first["category"], "wired", "and the pack it came from");
    assert_eq!(first["visible_in_picker"], true, "and its picker flag");
}

/// An emoji a text names that no row backs renders nothing, and a version
/// naming none answers the empty array.
#[tokio::test]
async fn an_unknown_shortcode_and_a_plain_text_render_no_emojis() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "emoji-unknown").await;
    let unknown = post(&pool, &alice, "nobody stored :ghost:").await;
    let plain = post(&pool, &alice, "no shortcodes at all").await;

    for row in [&unknown, &plain] {
        let response = history(&app, None, &row.id.to_string()).await;
        common::assert_answer(&response, StatusCode::OK, "the history reads");
        let body = response.json_value();
        let versions = body.as_array().expect("the history is an array");
        assert_eq!(
            versions.first().map(|entry| &entry["emojis"]),
            Some(&json!([])),
            "no stored emoji, no entry"
        );
    }
}
