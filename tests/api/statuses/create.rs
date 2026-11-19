//! `POST /api/v1/statuses`: the minimal create and everything it stores and
//! answers — the counters it steps, all five visibilities, the author's
//! default settings, the three content types, the posting application, and
//! the mentions and hashtags it links and renders.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        oauth_app::{self, NewOauthApp},
        status::Visibility,
        user::{Flag, NewUser},
    },
    util::entities::{hashtag_html, mention_html},
};

use super::support::{
    WRITE, author_now, created_row, id_of, mention_links, posted, publish, stored_count, tag_links,
    user_with_token,
};
use crate::common;

// ────────────────────────────  POST /api/v1/statuses  ────────────────────────

/// A minimal post is created, answered as the [`Status`] entity and stored
/// as the row that entity describes — local, public, its urls derived from
/// its own minted id.
#[tokio::test]
async fn a_minimal_post_creates_the_status_it_answers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let value = posted(&app, &token, json!({"status": "hello fediverse"})).await;
    let row = created_row(&pool, &value).await;

    assert_eq!(value["id"], row.id.to_string(), "the entity is the row");
    assert_eq!(
        value["uri"],
        format!("{TEST_BASE_URL}/objects/{}", row.id),
        "a local status owns this instance's object id (decision 6)"
    );
    assert_eq!(
        value["url"],
        json!(format!("{TEST_BASE_URL}/notice/{}", row.id)),
        "and the human URL derived from the same id"
    );
    assert_eq!(value["content"], "<p>hello fediverse</p>", "rendered HTML");
    assert_eq!(
        value["pleroma"]["content"],
        json!({ "text/plain": "hello fediverse" }),
        "the source text the author wrote"
    );
    assert_eq!(value["visibility"], "public", "the default scope");
    assert_eq!(value["sensitive"], false, "the default flag");
    assert_eq!(value["spoiler_text"], "", "no content warning");
    assert_eq!(value["language"], Value::Null, "no stated language");
    assert_eq!(value["in_reply_to_id"], Value::Null, "a root post");
    assert_eq!(value["reblog"], Value::Null, "and not a reblog");
    assert_eq!(value["mentions"], json!([]), "nothing mentioned");
    assert_eq!(value["tags"], json!([]), "nothing tagged");
    assert_eq!(value["media_attachments"], json!([]), "nothing attached");
    assert_eq!(value["reblogs_count"], 0, "every counter starts at zero");
    assert_eq!(value["favourites_count"], 0, "all four of them");
    assert_eq!(value["replies_count"], 0, "including replies");
    assert_eq!(value["pleroma"]["quotes_count"], 0, "and quotes");
    assert_eq!(value["account"]["acct"], "alice", "rendered as its author");
    assert_eq!(value["pleroma"]["local"], true, "a posted status is local");
    assert_eq!(
        value["pleroma"]["conversation_id"],
        row.conversation_id.to_string(),
        "a root status is its own conversation"
    );
    assert_eq!(
        (
            row.user_id,
            row.raw_text.as_str(),
            row.content_type.as_str(),
            row.visibility,
        ),
        (
            alice.id,
            "hello fediverse",
            "text/plain",
            Visibility::Public
        ),
        "and the row holds what the body said"
    );
    assert!(
        row.quote_of_id.is_none(),
        "a body that names no quote stores no quote"
    );
}

/// The author's `statuses_count` moves with every post that passes, and not
/// with the ones that do not.
#[tokio::test]
async fn a_post_counts_for_its_author_and_a_refusal_does_not() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        0,
        "a fresh account has not posted"
    );
    let first = posted(&app, &token, json!({"status": "one"})).await;
    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        1,
        "the first post is counted"
    );
    let second = posted(&app, &token, json!({"status": "two"})).await;
    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        2,
        "and the second is counted on top of it"
    );
    assert_ne!(
        id_of(&first),
        id_of(&second),
        "two posts are two rows, each its own id"
    );

    let refused = publish(&app, Some(token.as_str()), json!({"status": "   "})).await;
    common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, "a blank post");
    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        2,
        "a refusal counts nothing"
    );
    assert_eq!(
        stored_count(&pool).await,
        2,
        "and stores no row, not even a tombstone"
    );
    assert!(
        author_now(&pool, alice.id).await.last_status_at.is_some(),
        "and the author knows when they last posted"
    );
}

/// All five visibilities round trip, `local` answering its own name exactly
/// as Pleroma's renderer emits it.
#[tokio::test]
async fn every_visibility_is_stored_and_answered() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for name in ["public", "unlisted", "private", "direct", "local"] {
        let value = posted(
            &app,
            &token,
            json!({"status": "said it", "visibility": name}),
        )
        .await;
        assert_eq!(value["visibility"], name, "the entity answers {name}");
        assert_eq!(
            value["content"], "<p>said it</p>",
            "the same text in every scope, {name}"
        );
        assert_eq!(
            created_row(&pool, &value).await.visibility.as_str(),
            name,
            "and the row stores {name}"
        );
    }
}

/// The account's own settings are the defaults: its `default_scope` decides
/// the visibility and its `sensitive_default` the flag, until the body names
/// either.
#[tokio::test]
async fn the_author_s_settings_are_the_default_visibility_and_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let shy = common::create_local_user_with(&pool, "shy", |new| NewUser {
        default_scope: "private".to_owned(),
        sensitive_default: Flag::new(true),
        ..new
    })
    .await;
    let shy_token = common::create_app_and_token(&pool, &shy, WRITE).await;
    let (_open, open_token) = user_with_token(&pool, "open", WRITE).await;
    let app = common::test_app(pool.clone());

    let own = posted(&app, &shy_token, json!({"status": "as I always post"})).await;
    assert_eq!(
        own["visibility"], "private",
        "the account's default_scope decides a bare post"
    );
    assert_eq!(own["sensitive"], true, "and its sensitive_default flags it");

    let chosen = posted(
        &app,
        &shy_token,
        json!({"status": "otherwise", "visibility": "unlisted", "sensitive": false}),
    )
    .await;
    assert_eq!(chosen["visibility"], "unlisted", "and the body wins");
    assert_eq!(
        chosen["sensitive"], false,
        "including against a `true` default"
    );

    let plain = posted(&app, &open_token, json!({"status": "stock account"})).await;
    assert_eq!(
        plain["visibility"], "public",
        "an account on the stock default posts public"
    );
    assert_eq!(plain["sensitive"], false, "and never sensitive");
}

/// The three advertised content types render their own way: markdown
/// becomes the markup it means, submitted HTML is reduced to the allowlist,
/// and plain text is escaped so it can say those characters at all.
#[tokio::test]
async fn each_content_type_renders_in_its_own_format() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let markdown = posted(
        &app,
        &token,
        json!({"status": "a **bold** claim", "content_type": "text/markdown"}),
    )
    .await;
    assert_eq!(
        markdown["content"], "<p>a <strong>bold</strong> claim</p>",
        "markdown bold becomes <strong>"
    );
    assert_eq!(
        markdown["pleroma"]["content"],
        json!({ "text/plain": "a **bold** claim" }),
        "while the source stays what was typed"
    );

    let html = posted(
        &app,
        &token,
        json!({"status": "<b>rich</b><script>steal()</script>", "content_type": "text/html"}),
    )
    .await;
    assert_eq!(
        html["content"], "<b>rich</b>",
        "the allowed tag survives, the script element goes with its text"
    );

    let plain = posted(&app, &token, json!({"status": "<b>rich</b>"})).await;
    assert_eq!(
        plain["content"], "<p>&lt;b&gt;rich&lt;/b&gt;</p>",
        "no content_type is plain text, and plain text is escaped"
    );
    assert_eq!(
        created_row(&pool, &plain).await.content_type,
        "text/plain",
        "and is stored as such"
    );
}

/// The name and website of the app behind the token are stored as the
/// status's application and answer in the entity's `application` block;
/// an app that has no website answers that key as null.
#[tokio::test]
async fn the_application_behind_the_token_is_recorded() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, stock_token) = user_with_token(&pool, "alice", WRITE).await;
    let client = common::create_app_named(&pool, "Wired Client", Some(&alice)).await;
    let named_token = common::create_token(&pool, &alice, &client, WRITE).await;
    // The same registration the fixture does, plus the one column it leaves
    // out: an app that names its homepage.
    let sited = oauth_app::create(
        &pool,
        &NewOauthApp {
            client_name: "Sited Client".to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: WRITE.to_owned(),
            website: Some("https://client.example".to_owned()),
            user_id: Some(alice.id),
        },
    )
    .await
    .expect("the website-carrying app registers");
    let sited_token = common::create_token(&pool, &alice, &sited, WRITE).await;
    let app = common::test_app(pool.clone());

    let stock = posted(
        &app,
        &stock_token,
        json!({"status": "from the fixture app"}),
    )
    .await;
    assert_eq!(
        stock["application"],
        json!({ "name": "Test Client", "website": Value::Null }),
        "the token's app is named, and a website it does not have is null"
    );

    let value = posted(
        &app,
        &named_token.token,
        json!({"status": "from a named client"}),
    )
    .await;
    assert_eq!(
        value["application"],
        json!({ "name": "Wired Client", "website": Value::Null }),
        "the entity answers the client that posted"
    );
    let row = created_row(&pool, &value).await;
    assert_eq!(
        (
            row.application_name.as_deref(),
            row.application_website.as_deref(),
        ),
        (Some("Wired Client"), None),
        "and the row stores its name, and no website"
    );

    let with_site = posted(
        &app,
        &sited_token.token,
        json!({"status": "from a client with a homepage"}),
    )
    .await;
    assert_eq!(
        with_site["application"],
        json!({ "name": "Sited Client", "website": "https://client.example" }),
        "a client that names a homepage answers both"
    );
    let sited_row = created_row(&pool, &with_site).await;
    assert_eq!(
        (
            sited_row.application_name.as_deref(),
            sited_row.application_website.as_deref(),
        ),
        (Some("Sited Client"), Some("https://client.example")),
        "and the row stores the pair"
    );
}

/// A mention of a local account becomes its `h-card` anchor in the content,
/// a row in `status_mentions`, and an entry in the entity's `mentions`; an
/// unknown name is left as the text it was written as.
#[tokio::test]
async fn a_local_mention_is_linked_stored_and_answered() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let value = posted(&app, &token, json!({"status": "hi @bob, and @nobody"})).await;
    let row = created_row(&pool, &value).await;

    assert_eq!(
        value["content"],
        format!("<p>hi {}, and @nobody</p>", mention_html(&bob.ap_id, "bob")),
        "the known account becomes its h-card, the unknown one stays text"
    );
    assert_eq!(
        mention_links(&pool, row.id).await,
        vec![bob.id],
        "exactly the account this instance knows is linked in status_mentions"
    );
    assert_eq!(
        value["mentions"],
        json!([{ "id": bob.id.to_string(), "username": "bob",
                 "url": bob.ap_id, "acct": "bob" }]),
        "and it answers in the entity's `mentions`"
    );
}

/// One post naming an account and a tag links both in the same content, in
/// mention order and whatever case the text spelled the account in: a
/// case-different `@BOB` still resolves to bob's stored row and profile URL,
/// and the tag answers folded.
#[tokio::test]
async fn a_mention_and_a_hashtag_link_one_post_with_the_acct_in_any_case() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let value = posted(&app, &token, json!({"status": "hi @BOB #tag"})).await;
    let row = created_row(&pool, &value).await;

    assert_eq!(
        value["content"],
        format!(
            "<p>hi {} {}</p>",
            mention_html(&bob.ap_id, "BOB"),
            hashtag_html(TEST_BASE_URL, "tag"),
        ),
        "the case-different mention links to bob's profile URL (the `ap_id` \
         fallback the fixture's row leaves it on) displayed as typed, and the \
         tag links beside it"
    );
    assert_eq!(
        mention_links(&pool, row.id).await,
        vec![bob.id],
        "the account the folded name finds is linked in `status_mentions`"
    );
    assert_eq!(
        tag_links(&pool, row.id).await,
        vec!["tag"],
        "and the tag in `status_hashtags`"
    );
    assert_eq!(
        value["mentions"],
        json!([{ "id": bob.id.to_string(), "username": "bob",
                 "url": bob.ap_id, "acct": "bob" }]),
        "bob answers in the entity's `mentions`, by the entity's own spelling"
    );
    assert_eq!(
        value["tags"],
        json!([{ "name": "tag", "url": format!("{TEST_BASE_URL}/tags/tag") }]),
        "and the tag in its `tags`"
    );
}

/// Hashtags are extracted and deduplicated into one stored link each, linked
/// at every spelling in the content, and answered in the entity's `tags`.
#[tokio::test]
async fn hashtags_are_stored_linked_and_answered() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let value = posted(
        &app,
        &token,
        json!({"status": "hello #Rust and #rust, also #123"}),
    )
    .await;
    let row = created_row(&pool, &value).await;

    assert_eq!(
        tag_links(&pool, row.id).await,
        vec!["rust"],
        "one stored tag for both spellings, folded to lowercase, and #123 is \
         no tag at all"
    );
    assert_eq!(
        value["tags"],
        json!([{ "name": "rust", "url": format!("{TEST_BASE_URL}/tags/rust") }]),
        "the tag answers in the entity's `tags`"
    );
    assert_eq!(
        value["content"],
        format!(
            "<p>hello {tag} and {tag}, also #123</p>",
            tag = hashtag_html(TEST_BASE_URL, "rust")
        ),
        "and every spelling of it is linked in the content"
    );
}
