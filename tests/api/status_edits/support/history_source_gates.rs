//! The history and source reads, the ownership gates and the create
//! refusals the edit shares.

use super::*;

/// A never-edited status is the one-element history — the current version —
/// with its media, warning and flag, rendered for the anonymous caller the
/// show route admits.
#[tokio::test]
async fn the_history_of_an_unedited_status_is_its_current_version() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "unedited-author").await;
    let row = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            spoiler_text: "careful".to_owned(),
            sensitive: true,
            ..NewStatus::local(&alice, "as written")
        },
    )
    .await
    .expect("the fixture post inserts");
    let picture = attach(&pool, &alice, &row, 0).await;

    let response = history(&app, None, &row.id.to_string()).await;
    common::assert_answer(&response, StatusCode::OK, "history needs no token");
    let array = response.json_value();
    let items = array.as_array().expect("the history is an array");
    assert_eq!(items.len(), 1, "the current version is the whole history");
    let item = &items[0];
    assert_eq!(
        json_keys(item),
        vec![
            "account",
            "content",
            "created_at",
            "emojis",
            "media_attachments",
            "poll",
            "sensitive",
            "spoiler_text",
        ],
        "the fixed `StatusEdit` key set"
    );
    assert_eq!(item["content"], "<p>as written</p>");
    assert_eq!(item["spoiler_text"], "careful");
    assert_eq!(item["sensitive"], true);
    assert_eq!(item["created_at"], json!(mastodon_time(&row.inserted_at)));
    assert_eq!(item["account"]["id"], alice.id.to_string());
    let media = item["media_attachments"]
        .as_array()
        .expect("media is an array");
    assert_eq!(media[0]["id"], picture.to_string());
    assert_eq!(item["emojis"], json!([]));
    assert_eq!(item["poll"], Value::Null);
}

/// The history runs the show route's gate: a presented user token needs
/// `read:statuses`, and the malformed, unknown, invisible and deleted are
/// the shared 404.
#[tokio::test]
async fn the_history_reads_behind_the_show_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "history-owner").await;
    let row = post(&pool, &alice, "seen by the admitted").await;
    let id = row.id.to_string();
    let (quiet, quiet_token) = scoped(&pool, "quiet-reader", "write:statuses").await;
    let hidden = post_with(&pool, &quiet, "hidden", Visibility::Private).await;

    common::assert_answer(
        &history(&app, Some("nonsense"), &id).await,
        StatusCode::UNAUTHORIZED,
        "a token that does not resolve is the extractor's 401",
    );
    common::assert_all_answered(
        &[
            history(&app, None, "not-an-uuid").await,
            history(&app, None, UNKNOWN_ID).await,
            history(&app, Some(&token), &hidden.id.to_string()).await,
        ],
        StatusCode::NOT_FOUND,
        "malformed, unknown and somebody else's private are one 404",
    );
    let refused = history(&app, Some(&quiet_token), &id).await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "the scope gate");
    common::assert_error(
        &refused,
        NO_READ,
        "a user token without read:statuses is refused",
    );
}

/// A deleted status answers the history with the shared 404 even to its
/// own author.
#[tokio::test]
async fn a_deleted_status_has_no_history_to_read() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "gone-history").await;
    let row = post(&pool, &alice, "doomed").await;
    let tombstone = status::soft_delete(&pool, row.id)
        .await
        .expect("the fixture delete runs");
    assert!(tombstone.is_some(), "the fixture status was live");

    common::assert_answer(
        &history(&app, Some(&token), &row.id.to_string()).await,
        StatusCode::NOT_FOUND,
        "the author may not read a tombstone's history either",
    );
}

/// A post with no history at all still names its source to its author, and
/// nobody else — not as a stranger, and not on a token without the read
/// scope.
#[tokio::test]
async fn the_source_belongs_to_the_author_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "source-owner").await;
    let (_bob, bob_token) = editor(&pool, "source-stranger").await;
    let (quiet, quiet_token) = scoped(&pool, "quiet-source", "write:statuses").await;
    let row = post(&pool, &alice, "raw & source **text**").await;
    let id = row.id.to_string();
    let their_hidden = post_with(&pool, &quiet, "hidden from bob", Visibility::Private).await;

    let response = source(&app, &token, &id).await;
    common::assert_answer(&response, StatusCode::OK, "the author reads");
    let body = response.json_value();
    assert_eq!(body["id"], id);
    assert_eq!(
        body["text"], "raw & source **text**",
        "the raw text, not the render"
    );
    assert_eq!(body["spoiler_text"], "");
    assert_eq!(body["content_type"], "text/plain");

    common::assert_all_answered(
        &[
            source(&app, &token, UNKNOWN_ID).await,
            source(&app, &bob_token, &their_hidden.id.to_string()).await,
        ],
        StatusCode::NOT_FOUND,
        "unknown and invisible are the shared 404",
    );
    let not_yours = source(&app, &bob_token, &id).await;
    common::assert_answer(&not_yours, StatusCode::FORBIDDEN, "a visible stranger");
    common::assert_error(&not_yours, NOT_FOUND, "refused in the shared wording");
    let no_scope = source(&app, &quiet_token, &id).await;
    common::assert_answer(&no_scope, StatusCode::FORBIDDEN, "the scope gate");
    common::assert_error(
        &no_scope,
        NO_READ,
        "a user token without read:statuses is refused",
    );
    common::assert_answer(
        &source(&app, "nonsense", &id).await,
        StatusCode::UNAUTHORIZED,
        "a token that does not resolve is the extractor's 401",
    );
}

/// Only the author's own local post is editable: somebody else's visible
/// post and a mirrored remote one are the 403 twin of the shared
/// `"Record not found"`, the invisible, unknown, malformed and deleted stay
/// the 404, and one's own reblog row is the edit's own 422.
#[tokio::test]
async fn the_edit_admits_only_ones_own_local_post() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "would-be-editor").await;
    let (bob, bob_token) = editor(&pool, "real-owner").await;
    let theirs = post(&pool, &bob, "not yours to edit").await;
    let mirrored = remote_post(&pool, "remote-author", "https://remote.example/objects/1").await;
    let boost = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            reblog_of_id: Some(theirs.id),
            ..NewStatus::local(&alice, "")
        },
    )
    .await
    .expect("the fixture reblog inserts");
    let hidden = post_with(&pool, &bob, "private and invisible", Visibility::Private).await;

    let not_yours = edit_row(&app, &token, &theirs, json!({"status": "mine now"})).await;
    common::assert_answer(&not_yours, StatusCode::FORBIDDEN, "somebody else's post");
    common::assert_error(&not_yours, NOT_FOUND, "refused in the shared wording");

    let remote = edit_row(&app, &token, &mirrored, json!({"status": "edited here"})).await;
    common::assert_answer(&remote, StatusCode::FORBIDDEN, "a mirrored remote post");
    common::assert_error(&remote, NOT_FOUND, "is nobody's to edit here");

    let someone_elses_boost =
        edit_row(&app, &bob_token, &boost, json!({"status": "not yours"})).await;
    common::assert_answer(
        &someone_elses_boost,
        StatusCode::FORBIDDEN,
        "a boost of another's post is not yours before it is a boost",
    );

    let own_boost = edit_row(&app, &token, &boost, json!({"status": "my boost"})).await;
    common::assert_answer(
        &own_boost,
        StatusCode::UNPROCESSABLE_ENTITY,
        "one's own boost",
    );
    common::assert_error(&own_boost, CANNOT_EDIT, "refuses with its own words");

    common::assert_all_answered(
        &[
            edit(&app, &token, UNKNOWN_ID, json!({"status": "anywhere"})).await,
            edit(&app, &token, "not-an-uuid", json!({"status": "anywhere"})).await,
            edit(
                &app,
                &token,
                &hidden.id.to_string(),
                json!({"status": "peek"}),
            )
            .await,
        ],
        StatusCode::NOT_FOUND,
        "unknown, malformed and invisible are the shared 404",
    );
}

/// The edit route runs the strict pipeline: no token is the 401, an
/// app-only token and a scope-less user token are the 403s, and a token
/// that does not resolve is the 401.
#[tokio::test]
async fn the_edit_needs_a_user_token_with_write_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _) = editor(&pool, "gated").await;
    let row = post(&pool, &alice, "gated post").await;
    let (_quiet, quiet_token) = scoped(&pool, "scopeless", "read:statuses").await;
    let app_token = common::create_app_only_token(&pool, "write:statuses").await;

    let path = format!("/api/v1/statuses/{}", row.id);
    let no_token = common::TestRequest::put(&path)
        .json(&json!({"status": "anyone?"}))
        .send(&app)
        .await;
    common::assert_answer(&no_token, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&no_token, NO_TOKEN, "asks for credentials");

    let bad = edit(
        &app,
        "nonsense",
        &row.id.to_string(),
        json!({"status": "anyone?"}),
    )
    .await;
    common::assert_answer(&bad, StatusCode::UNAUTHORIZED, "an unusable token");
    common::assert_error(&bad, BAD_TOKEN, "is the 401");

    let app_only = edit(
        &app,
        &app_token,
        &row.id.to_string(),
        json!({"status": "app edits?"}),
    )
    .await;
    common::assert_answer(&app_only, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(&app_only, USER_REQUIRED, "is no user");

    let no_scope = edit(
        &app,
        &quiet_token,
        &row.id.to_string(),
        json!({"status": "no scope"}),
    )
    .await;
    common::assert_answer(&no_scope, StatusCode::FORBIDDEN, "a scope-less user token");
    common::assert_error(&no_scope, NO_WRITE, "lacks write:statuses");
}

/// The merged body answers the create's refusals — and a refusal writes
/// nothing, records no revision and marks no stamp.
#[tokio::test]
async fn the_edit_refuses_where_the_create_would() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "refused-editor").await;
    let row = post(&pool, &alice, "untouched").await;

    let limit = common::test_config().max_status_chars;
    for (body, message) in [
        (json!({"status": "   "}), EMPTY_STATUS),
        (json!({"content_type": "text/x-markdown"}), BAD_CONTENT_TYPE),
        (json!({"status": "x".repeat(limit + 1)}), OVER_LIMIT),
        (json!({"spoiler_text": "x".repeat(201)}), SPOILER_TOO_LONG),
    ] {
        let response = edit_row(&app, &token, &row, body).await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a refused edit",
        );
        common::assert_error(&response, message, "carries the create's wording");
    }

    let stored = status::get_by_id(&pool, row.id)
        .await
        .unwrap()
        .expect("the fixture post is stored live");
    assert_eq!(stored.content, "<p>untouched</p>", "nothing was written");
    assert!(stored.edited_at.is_none(), "nothing was stamped");
    assert!(
        revisions(&pool, row.id).await.is_empty(),
        "nothing recorded"
    );
}
