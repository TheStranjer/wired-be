//! Unit tests for the custom-emoji part of a page render: which
//! `custom_emojis` rows a status renders, keyed by its *author's* domain, and
//! the texts — content, content warning, poll options — they are read from.

use serde_json::{Value, json};
use sqlx::PgPool;

use super::{
    ViewerMode, render_statuses,
    test_support::{BASE, post, test_state, user},
};
use crate::{
    models::{
        custom_emoji::{self, NewCustomEmoji},
        poll::{self, PollParams},
        status::{NewStatus, Status as StatusRow, insert},
        user::User,
    },
    test_fixtures::remote_user,
};

/// Stores the emoji `shortcode` of `domain` (`None` = local), its image at
/// `https://{host}/emoji/{shortcode}.png`.
async fn store_emoji(pool: &PgPool, shortcode: &str, domain: Option<&str>) {
    let host = domain.unwrap_or("test.example");
    let stored = custom_emoji::upsert(
        pool,
        &NewCustomEmoji {
            domain: domain.map(str::to_owned),
            static_url: Some(format!("https://{host}/emoji/{shortcode}-still.png")),
            category: Some("wired".to_owned()),
            ..NewCustomEmoji::local(shortcode, &format!("https://{host}/emoji/{shortcode}.png"))
        },
    )
    .await
    .expect("the fixture emoji stores");
    assert_eq!(
        stored.shortcode, shortcode,
        "the fixture row is the one asked"
    );
}

/// Marks the stored emoji `shortcode` of the local pack as disabled.
async fn disable_local(pool: &PgPool, shortcode: &str) {
    let moved = sqlx::query("UPDATE custom_emojis SET disabled = true WHERE shortcode = $1")
        .bind(shortcode)
        .execute(pool)
        .await
        .expect("the emoji disables");
    assert_eq!(moved.rows_affected(), 1, "the emoji was there to disable");
}

/// A public mirror of a post by the remote account `author`, with `content`
/// as its HTML.
async fn remote_status(pool: &PgPool, author: &User, content: &str) -> StatusRow {
    insert(
        pool,
        BASE,
        &NewStatus {
            user_id: author.id,
            uri: Some(format!(
                "https://remote.example/posts/{}",
                uuid::Uuid::now_v7()
            )),
            content: content.to_owned(),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the remote fixture post inserts")
}

/// Renders `rows` anonymously and returns every entity's JSON, in order.
async fn render_all(pool: &PgPool, rows: &[StatusRow]) -> Vec<Value> {
    let state = test_state(pool);
    render_statuses(&state, ViewerMode::ANONYMOUS, rows)
        .await
        .expect("the page renders")
        .iter()
        .map(|entity| serde_json::to_value(entity).expect("the entity serializes"))
        .collect()
}

/// The shortcodes of `entity`'s `emojis`, in the order rendered.
fn shortcodes(entity: &Value) -> Vec<&str> {
    entity["emojis"]
        .as_array()
        .expect("emojis is an array")
        .iter()
        .map(|emoji| emoji["shortcode"].as_str().expect("a shortcode string"))
        .collect()
}

#[tokio::test]
async fn a_local_status_renders_its_local_emoji_entity() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    store_emoji(&pool, "wiredcat", None).await;
    let row = post(&pool, &alice, "Hello fediverse :wiredcat: and more").await;

    let rendered = render_all(&pool, &[row]).await;

    assert_eq!(
        rendered[0]["emojis"],
        json!([{
            "shortcode": "wiredcat",
            "url": "https://test.example/emoji/wiredcat.png",
            "static_url": "https://test.example/emoji/wiredcat-still.png",
            "visible_in_picker": true,
            "category": "wired",
            "tags": [],
        }]),
        "the local row renders as the CustomEmoji entity"
    );
}

#[tokio::test]
async fn a_remote_status_uses_only_its_authors_domain() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let bob = remote_user(&pool, "bob").await;
    store_emoji(&pool, "blob", Some("remote.example")).await;
    store_emoji(&pool, "blob", Some("other.example")).await;
    store_emoji(&pool, "blob", None).await;
    let row = remote_status(&pool, &bob, "<p>look :blob:</p>").await;

    let rendered = render_all(&pool, &[row]).await;

    assert_eq!(
        rendered[0]["emojis"][0]["url"], "https://remote.example/emoji/blob.png",
        "the author's domain row, not the local or another domain's"
    );
    assert_eq!(
        shortcodes(&rendered[0]),
        vec!["blob"],
        "one emoji, however many domains stored the name"
    );
}

#[tokio::test]
async fn an_emoji_of_another_domain_is_not_used() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = remote_user(&pool, "bob").await;
    store_emoji(&pool, "blob", Some("other.example")).await;
    store_emoji(&pool, "wiredcat", None).await;
    let remote = remote_status(&pool, &bob, "<p>:blob: :wiredcat:</p>").await;
    let local = post(&pool, &alice, ":blob: :wiredcat:").await;

    let rendered = render_all(&pool, &[remote, local]).await;

    assert!(
        shortcodes(&rendered[0]).is_empty(),
        "the remote author has neither: one is another domain's, one is local"
    );
    assert_eq!(
        shortcodes(&rendered[1]),
        vec!["wiredcat"],
        "the local author has the local one only"
    );
}

#[tokio::test]
async fn an_emoji_used_only_in_the_content_warning_is_included() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    store_emoji(&pool, "warn", None).await;
    let row = insert(
        &pool,
        BASE,
        &NewStatus {
            spoiler_text: "careful :warn:".to_owned(),
            ..NewStatus::local(&alice, "nothing special here")
        },
    )
    .await
    .expect("the warned post inserts");

    let rendered = render_all(&pool, &[row]).await;

    assert_eq!(shortcodes(&rendered[0]), vec!["warn"], "from the warning");
}

#[tokio::test]
async fn an_emoji_used_only_in_a_poll_option_is_included() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    store_emoji(&pool, "tea", None).await;
    let row = post(&pool, &alice, "which one").await;
    let mut conn = pool.acquire().await.expect("slot connection");
    let made = poll::create(
        &mut conn,
        row.id,
        &PollParams {
            options: vec![":tea: please".to_owned(), "coffee".to_owned()],
            ..PollParams::default()
        },
    )
    .await
    .expect("the fixture poll creates");
    assert_eq!(made.status_id, row.id, "the poll belongs to the post");
    drop(conn);

    let rendered = render_all(&pool, &[row]).await;

    assert_eq!(shortcodes(&rendered[0]), vec!["tea"], "from the option");
}

#[tokio::test]
async fn unknown_shortcodes_and_plain_text_render_no_emojis() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    store_emoji(&pool, "wiredcat", None).await;
    let unknown = post(&pool, &alice, "nobody stored :ghost:").await;
    let plain = post(&pool, &alice, "no shortcodes at 12:30").await;

    let rendered = render_all(&pool, &[unknown, plain]).await;

    assert_eq!(rendered[0]["emojis"], json!([]), "unknown shortcode");
    assert_eq!(rendered[1]["emojis"], json!([]), "plain text");
}

#[tokio::test]
async fn a_disabled_emoji_is_not_rendered() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    store_emoji(&pool, "on", None).await;
    store_emoji(&pool, "off", None).await;
    disable_local(&pool, "off").await;
    let row = post(&pool, &alice, ":on: :off:").await;

    let rendered = render_all(&pool, &[row]).await;

    assert_eq!(
        shortcodes(&rendered[0]),
        vec!["on"],
        "the moderation switch hides the disabled one"
    );
}

#[tokio::test]
async fn emojis_list_once_each_in_order_of_first_use() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    store_emoji(&pool, "aaa", None).await;
    store_emoji(&pool, "bbb", None).await;
    let row = post(&pool, &alice, ":bbb: then :aaa: then :bbb: again").await;

    let rendered = render_all(&pool, &[row]).await;

    assert_eq!(
        shortcodes(&rendered[0]),
        vec!["bbb", "aaa"],
        "first appearance order, repeats dropped"
    );
}

#[tokio::test]
async fn a_reblog_nests_the_originals_emojis_and_has_none_of_its_own() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    store_emoji(&pool, "wiredcat", None).await;
    let original = post(&pool, &alice, "hi :wiredcat:").await;
    let boost = insert(
        &pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(original.id),
            ..NewStatus::local(&bob, "")
        },
    )
    .await
    .expect("the boost inserts");

    let rendered = render_all(&pool, &[boost]).await;

    assert_eq!(rendered[0]["emojis"], json!([]), "the boost has no text");
    assert_eq!(
        shortcodes(&rendered[0]["reblog"]),
        vec!["wiredcat"],
        "the nested original carries its own"
    );
}

#[tokio::test]
async fn a_quoted_status_nests_its_own_emojis() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    store_emoji(&pool, "wiredcat", None).await;
    let quoted = post(&pool, &alice, "the original :wiredcat:").await;
    let quoting = insert(
        &pool,
        BASE,
        &NewStatus {
            quote_of_id: Some(quoted.id),
            ..NewStatus::local(&bob, "my take")
        },
    )
    .await
    .expect("the quote inserts");

    let rendered = render_all(&pool, &[quoting]).await;

    assert_eq!(
        rendered[0]["emojis"],
        json!([]),
        "the quoting text has none"
    );
    assert_eq!(
        shortcodes(&rendered[0]["quote"]["quoted_status"]),
        vec!["wiredcat"],
        "the quoted status renders its emoji inside the quote"
    );
}
