//! The edit itself: fields, form bodies, no-ops, successive edits, tags,
//! mentions and the content type.

use super::*;

/// Editing the text rewrites the post, records the superseded version and
/// stamps `edited_at` on the answer and the row alike.
#[tokio::test]
async fn an_edit_records_the_previous_version_and_stamps_the_answer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "editor").await;
    let row = post(&pool, &alice, "original words").await;

    let response = edit_row(&app, &token, &row, json!({"status": "corrected words"})).await;
    common::assert_answer(&response, StatusCode::OK, "the author edits");
    let body = response.json_value();
    assert_eq!(body["id"], row.id.to_string(), "the same post");
    assert_eq!(
        body["content"], "<p>corrected words</p>",
        "answered as the new version"
    );
    assert!(!body["edited_at"].is_null(), "and stamped as edited");

    let stored = status::get_by_id(&pool, row.id)
        .await
        .unwrap()
        .expect("the edited status is stored live");
    assert_eq!(stored.raw_text, "corrected words", "the row carries it too");

    let versions = history(&app, Some(&token), &row.id.to_string()).await;
    common::assert_answer(&versions, StatusCode::OK, "the history reads");
    let array = versions.json_value();
    let items = array.as_array().expect("the history is an array");
    assert_eq!(
        items.len(),
        2,
        "the superseded version plus the current one"
    );
    assert_eq!(items[0]["content"], "<p>original words</p>");
    assert_eq!(
        items[0]["created_at"],
        json!(mastodon_time(&row.inserted_at)),
        "the first version was authored when the post was"
    );
    assert_eq!(items[1]["content"], "<p>corrected words</p>");
    assert_eq!(
        items[1]["created_at"], body["edited_at"],
        "and the current element carries the edit's stamp"
    );
    assert_eq!(items[0]["account"]["id"], alice.id.to_string());
    assert_eq!(items[1]["account"]["id"], alice.id.to_string());
    assert_eq!(items[1]["media_attachments"], json!([]));
    assert_eq!(items[1]["emojis"], json!([]), "the standing placeholders");
    assert_eq!(items[1]["poll"], Value::Null);
}

/// The edit route's `after_status_edited` hook: a successful edit pushes
/// Mastodon's `status.update` onto the hub, riding the post's own target
/// streams with the render hint and no entity of its own.
#[tokio::test]
async fn an_edit_pushes_status_update_onto_the_hub() {
    let db = common::test_db().await;
    let state = common::test_app_state(db.pool().clone(), common::test_config());
    let app = wired_be::app(state.clone());
    let (alice, token) = editor(&state.db, "edit-pushing").await;
    let row = post(&state.db, &alice, "before the push").await;
    let mut rx = state.hub.subscribe();

    let response = edit_row(&app, &token, &row, json!({"status": "after the push"})).await;
    common::assert_answer(&response, StatusCode::OK, "the author edits their own post");

    let event = rx.recv().await.expect("the edit hook published an event");
    assert_eq!(
        event.event, "status.update",
        "the edit pushes the redraw event"
    );
    assert!(
        event.streams.contains(&"public".to_owned()),
        "a public post's streams carry it: {:?}",
        event.streams
    );
    assert!(
        event.streams.contains(&format!("user:{}", alice.id)),
        "and the author's home stream: {:?}",
        event.streams
    );
    assert_eq!(
        event.render.as_ref(),
        Some(&RenderSpec::Status(row.id)),
        "the event carries the status render hint"
    );
    assert!(
        event.payload.is_none(),
        "the entity is rendered per socket, not shipped"
    );
    assert_eq!(
        event.audience,
        Audience::Public,
        "a public post gates nobody"
    );
}

/// The body's fields merge over the stored row: what it presents wins,
/// what it leaves out stays, and the source read sees the merged result.
#[tokio::test]
async fn only_the_presented_parameters_change() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = editor(&pool, "merger").await;
    let created = posted(
        &app,
        &token,
        &json!({
            "status": "the stored text",
            "spoiler_text": "kept warning",
            "sensitive": true,
            "language": "eo",
            "visibility": "unlisted",
        }),
    )
    .await;
    let id = created["id"].as_str().expect("the create answers its id");

    let response = edit(&app, &token, id, json!({"status": "new text"})).await;
    common::assert_answer(&response, StatusCode::OK, "the edit passes");
    let body = response.json_value();
    assert_eq!(
        body["content"], "<p>new text</p>",
        "the submitted field won"
    );
    assert_eq!(
        body["spoiler_text"], "kept warning",
        "the rest carried over"
    );
    assert_eq!(body["sensitive"], true);
    assert_eq!(body["language"], "eo");
    assert_eq!(
        body["visibility"], "unlisted",
        "and an edit never moves scope"
    );

    let source_read = source(&app, &token, id).await;
    common::assert_answer(&source_read, StatusCode::OK, "the author reads the source");
    assert_eq!(
        source_read.json_value()["text"],
        "new text",
        "the source is the newest raw text"
    );
}

/// Warning, flag and tag edit every one of their own fields and record a
/// version for the changed warning.
#[tokio::test]
async fn spoiler_sensitive_and_language_edit_their_fields() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = editor(&pool, "tagger").await;
    let created = posted(&app, &token, &json!({"status": "same text"})).await;
    let id = created["id"].as_str().expect("the create answers its id");

    let response = edit(
        &app,
        &token,
        id,
        json!({"spoiler_text": "  cw  ", "sensitive": true, "language": "de"}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the edit passes");
    let body = response.json_value();
    assert_eq!(
        body["spoiler_text"], "  cw  ",
        "a warning is stored verbatim, spaces and all"
    );
    assert_eq!(body["sensitive"], true);
    assert_eq!(body["language"], "de");
    assert_eq!(body["content"], "<p>same text</p>", "the text did not move");
    assert!(
        !body["edited_at"].is_null(),
        "the warning changed a versioned field, so the post is marked edited"
    );

    let source_read = source(&app, &token, id).await.json_value();
    assert_eq!(
        source_read["spoiler_text"], "  cw  ",
        "the source carries the warning beside the text"
    );
    assert_eq!(
        source_read["content_type"], "text/plain",
        "and the type the text was written in"
    );
    assert_eq!(
        json_keys(&source_read),
        vec!["content_type", "id", "spoiler_text", "text"],
        "and exactly those four keys (decision 1: Pleroma's `content_type` \
         rides Mastodon's shape)"
    );
}

/// A form body edits the same way a JSON one does (decision 10), flex
/// boolean spellings included.
#[tokio::test]
async fn a_form_body_edits_like_a_json_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "form-editor").await;
    let row = post(&pool, &alice, "before the form").await;

    let response = common::TestRequest::put(&format!("/api/v1/statuses/{}", row.id))
        .bearer(&token)
        .form(&[("status", "after the form"), ("sensitive", "on")])
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the form edit passes");
    let body = response.json_value();
    assert_eq!(body["content"], "<p>after the form</p>");
    assert_eq!(body["sensitive"], true, "`\"on\"` spells true");
}

/// An edit that changes none of the versioned fields records no revision,
/// stamps no `edited_at` and leaves the history a one-element array —
/// including the empty body that changes nothing at all.
#[tokio::test]
async fn an_edit_of_nothing_records_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "restater").await;
    let row = post(&pool, &alice, "same words").await;

    let response = edit_row(&app, &token, &row, json!({"status": "same words"})).await;
    common::assert_answer(&response, StatusCode::OK, "the restatement answers");
    let body = response.json_value();
    assert_eq!(body["content"], row.content, "the post is exactly itself");
    assert_eq!(body["edited_at"], Value::Null, "and unmarked");
    assert!(
        revisions(&pool, row.id).await.is_empty(),
        "no revision was recorded"
    );

    let empty = edit_row(&app, &token, &row, json!({})).await;
    common::assert_answer(&empty, StatusCode::OK, "an empty body is accepted");
    assert_eq!(empty.json_value()["edited_at"], Value::Null);
    let history_read = history(&app, Some(&token), &row.id.to_string())
        .await
        .json_value();
    assert_eq!(
        contents_of(&history_read),
        vec!["<p>same words</p>"],
        "and the history stays the one current version"
    );

    let trimmed = edit_row(&app, &token, &row, json!({"status": "  same words  "})).await;
    common::assert_answer(&trimmed, StatusCode::OK, "a padded restatement is accepted");
    assert_eq!(trimmed.json_value()["edited_at"], Value::Null);
    assert!(
        revisions(&pool, row.id).await.is_empty(),
        "the trim makes it the same text, so still no revision"
    );
}

/// Two successive edits order their revisions oldest-authored-first with
/// the live version last, each entry carrying the state of its time.
#[tokio::test]
async fn successive_edits_walk_the_history_forward() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "twice-edited").await;
    let row = post(&pool, &alice, "v1").await;

    let first = edit_row(&app, &token, &row, json!({"status": "v2"})).await;
    common::assert_answer(&first, StatusCode::OK, "the first edit passes");
    let second = edit_row(
        &app,
        &token,
        &row,
        json!({"status": "v3", "spoiler_text": "late warning"}),
    )
    .await;
    common::assert_answer(&second, StatusCode::OK, "the second edit passes");

    let history_read = history(&app, Some(&token), &row.id.to_string())
        .await
        .json_value();
    let items = history_read.as_array().expect("the history is an array");
    assert_eq!(
        items.len(),
        3,
        "two superseded versions and the current one"
    );
    assert_eq!(
        contents_of(&history_read),
        vec!["<p>v1</p>", "<p>v2</p>", "<p>v3</p>"]
    );
    assert_eq!(
        items[0]["spoiler_text"], "",
        "each entry carries its own state"
    );
    assert_eq!(items[1]["spoiler_text"], "");
    assert_eq!(items[2]["spoiler_text"], "late warning");
    let times: Vec<&str> = items
        .iter()
        .map(|entry| entry["created_at"].as_str().expect("a timestamp"))
        .collect();
    let ordered = times
        .iter()
        .zip(times.iter().skip(1))
        .all(|(older, newer)| older <= newer);
    assert!(ordered, "authored times walk forward: {times:?}");
    assert_eq!(
        items[2]["created_at"],
        second.json_value()["edited_at"],
        "the live row's stamp is the current version's time"
    );
}

/// The rewritten text re-links the post: mentions and hashtags the new
/// words carry replace the old ones wholesale, and the render is the
/// creation's own (the shared `render_status_fields`).
#[tokio::test]
async fn an_edit_replaces_the_posts_hashtags_and_mentions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "relabeler").await;
    let bob = common::create_local_user(&pool, "bobby").await;
    let row = post(&pool, &alice, "first words").await;

    let response = edit_row(
        &app,
        &token,
        &row,
        json!({"status": "hi @bobby and @nobody, see #Rust"}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the edit passes");
    let body = response.json_value();
    assert_eq!(
        body["mentions"],
        json!([{ "id": bob.id.to_string(), "username": "bobby",
                 "url": bob.ap_id, "acct": "bobby" }]),
        "the new text's known mention is linked, the unknown one stays text"
    );
    assert_eq!(
        body["tags"],
        json!([{ "name": "rust", "url": format!("{TEST_BASE_URL}/tags/rust") }]),
        "and its hashtag folded and linked"
    );

    let back = edit_row(&app, &token, &row, json!({"status": "plain again"})).await;
    common::assert_answer(&back, StatusCode::OK, "the second edit passes");
    let emptied = back.json_value();
    assert_eq!(
        emptied["mentions"],
        json!([]),
        "the replaced set is the whole set: the dropped mention is unlinked"
    );
    assert_eq!(emptied["tags"], json!([]), "and so is the dropped tag");

    let versions = revisions(&pool, row.id).await;
    assert_eq!(versions.len(), 2, "both edits superseded a version");
    assert_eq!(
        versions[1].raw_text, "hi @bobby and @nobody, see #Rust",
        "the second snapshot is the first edit's version, source and all"
    );
}

/// The markdown a creation stored answers its type back through the source
/// read, and an edit of the text re-renders under the kept type.
#[tokio::test]
async fn the_content_type_survives_and_renders() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = editor(&pool, "markdowner").await;
    let created = posted(
        &app,
        &token,
        &json!({"status": "a **bold** claim", "content_type": "text/markdown"}),
    )
    .await;
    let id = created["id"].as_str().expect("the create answers its id");
    assert_eq!(created["content"], "<p>a <strong>bold</strong> claim</p>");

    let source_read = source(&app, &token, id).await.json_value();
    assert_eq!(source_read["text"], "a **bold** claim");
    assert_eq!(source_read["content_type"], "text/markdown");

    let response = edit(&app, &token, id, json!({"status": "a **quieter** claim"})).await;
    common::assert_answer(&response, StatusCode::OK, "the edit passes");
    assert_eq!(
        response.json_value()["content"],
        "<p>a <strong>quieter</strong> claim</p>",
        "the kept type renders the new text the way the create would"
    );
}
