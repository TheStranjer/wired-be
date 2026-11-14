//! Status edits over HTTP (WBE-89): the write `PUT /api/v1/statuses/{id}`,
//! the version listing `GET /api/v1/statuses/{id}/history` and the source
//! read `GET /api/v1/statuses/{id}/source`.
//!
//! The edit is Pleroma's `update`: a user token with `write:statuses`, the
//! shared status gate first (the malformed, unknown, deleted and invisible
//! are the module's one 404), and then Pleroma's ownership stack — only the
//! author's own **local** post, answered with the 403 twin of the
//! `"Record not found"` body, and never a reblog row (422 `"Cannot edit a
//! reblog"`). The body's six parameters (`status`, `spoiler_text`,
//! `sensitive`, `language`, `content_type`, `media_ids`) merge over the
//! stored row — only the presented ones change — and run through the
//! creation's own checks and render, so every refusal is the creation's and
//! the rewritten post linkifies, tags and mentions the way a fresh post
//! would. The write is one transaction that records the superseded version
//! (only when the content, warning, sensitive flag or media actually
//! changed: an edit of nothing records no revision and sets no `edited_at`)
//! and replaces the post's mentions, hashtags and attachments; the answer
//! is the [`Status`] entity with `edited_at` set.
//!
//! The history is Pleroma's `show_history` with optional auth
//! (`read:statuses` on a presented user token): the superseded versions
//! oldest-authored-first with the current one last, each entry a
//! `StatusEdit` — content, warning, flag, authored time, author account,
//! media, and the still-empty `emojis` and `poll` placeholders. A never-
//! edited status is the one-element array. The source is Pleroma's
//! `show_source` under this ticket's author-only gate: the stored raw text,
//! the warning and the `MIME` type, as `{"id", "text", "spoiler_text",
//! "content_type"}`.
//!
//! The `update` telling of the post's local boosters already lands at the
//! module's `after_status_edited` hook (WBE-113, tested in
//! `notification_generation.rs`), and so does the federated `Update` of the
//! re-drafted note (WBE-176, tested in `status_federation.rs`), beside the
//! `status.update` stream push the ticket-190 hook test below pins.
//!
//! [`Status`]: wired_be::entities::status::Status

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        media_attachment::{self, NewMediaAttachment},
        status::{self, NewStatus, Status as StatusRow, Visibility},
        status_edit,
        user::User,
    },
    streaming::hub::{Audience, RenderSpec},
    util::time::mastodon_time,
};

use crate::common;

/// The 404 body of the shared status gate, and the 403 body of the edit's
/// and the source's ownership refusals.
const NOT_FOUND: &str = "Record not found";
/// The 401 body `AuthUser` sends when no usable token was presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for an app-only token on a route that needs a user.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 401 body for a token that does not resolve.
const BAD_TOKEN: &str = "The access token is invalid";
/// The 403 wording of the edit gate: it asks `write:statuses`.
const NO_WRITE: &str = "Insufficient permissions: write:statuses.";
/// The 403 wording of the source and history gates: they ask
/// `read:statuses`.
const NO_READ: &str = "Insufficient permissions: read:statuses.";
/// The 422 body of `PUT` for a reblog row.
const CANNOT_EDIT: &str = "Cannot edit a reblog";
/// The 422 bodies the edit shares with the create.
const EMPTY_STATUS: &str = "Cannot post an empty status without attachments";
const BAD_CONTENT_TYPE: &str = "Invalid content_type";
const OVER_LIMIT: &str = "The status is over the character limit";
const SPOILER_TOO_LONG: &str =
    "Validation failed: Spoiler text is too long (maximum is 200 characters)";
/// An id that names no stored status.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

/// A local user named `nickname` with a `write:statuses read:statuses`
/// bearer token: enough to edit a post and read it, its history and its
/// source back through the API.
async fn editor(pool: &PgPool, nickname: &str) -> (User, String) {
    scoped(pool, nickname, "write:statuses read:statuses").await
}

/// A local user named `nickname` with exactly `scopes`, for the gate tests
/// that must *lack* one of the two.
async fn scoped(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A public post by `author`, through the model.
async fn post(pool: &PgPool, author: &User, text: &str) -> StatusRow {
    common::create_status(pool, author, text, Visibility::Public).await
}

/// A public post by `author` at `visibility`.
async fn post_with(pool: &PgPool, author: &User, text: &str, visibility: Visibility) -> StatusRow {
    common::create_status(pool, author, text, visibility).await
}

/// A public post through `POST /api/v1/statuses`, as the create route wrote
/// it — the fixture for the tests that read a creation-built post back
/// through the edit and the source route.
async fn posted(app: &Router, token: &str, body: &Value) -> Value {
    let response = common::TestRequest::post("/api/v1/statuses")
        .bearer(token)
        .json(body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the create accepts the body");
    response.json_value()
}

/// A mirror of a remote post, by a fresh remote account: the shape
/// federation stores, and the row no edit of ours may touch.
async fn remote_post(pool: &PgPool, nickname: &str, uri: &str) -> StatusRow {
    let author = common::create_remote_user(pool, nickname, "remote.example").await;
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            uri: Some(uri.to_owned()),
            user_id: author.id,
            content: "<p>from elsewhere</p>".to_owned(),
            raw_text: "from elsewhere".to_owned(),
            local: false,
            ..NewStatus::default()
        },
    )
    .await
    .expect("the fixture remote post inserts")
}

/// An unposted upload by `author` — a `media_ids` entry an edit may claim.
async fn upload(pool: &PgPool, author: &User) -> Uuid {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            status_id: None,
            url: format!("{TEST_BASE_URL}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("the fixture upload inserts")
    .id
}

/// `attachment` already on `status_row` at `position`.
async fn attach(pool: &PgPool, author: &User, status_row: &StatusRow, position: i32) -> Uuid {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            status_id: Some(status_row.id),
            position,
            url: format!("{TEST_BASE_URL}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("the fixture attachment inserts")
    .id
}

/// The status's attachment ids as stored, in render order.
async fn stored_media(pool: &PgPool, status_id: Uuid) -> Vec<Uuid> {
    let rows = media_attachment::for_statuses(pool, &[status_id])
        .await
        .expect("the attachment rows read");
    rows.get(&status_id).map_or_else(Vec::new, |found| {
        found.iter().map(|attachment| attachment.id).collect()
    })
}

/// The superseded versions of `status_id`, straight from the table.
async fn revisions(pool: &PgPool, status_id: Uuid) -> Vec<status_edit::StatusEdit> {
    status_edit::list_for_status(pool, status_id)
        .await
        .expect("the revisions read")
}

/// `PUT /api/v1/statuses/{id}` with a JSON body, the id as text so the miss
/// probes can feed any spelling as the path segment.
async fn edit(app: &Router, token: &str, id: &str, body: Value) -> common::TestResponse {
    common::TestRequest::put(&format!("/api/v1/statuses/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// [`edit`] keyed by the stored row, the usual call.
async fn edit_row(app: &Router, token: &str, row: &StatusRow, body: Value) -> common::TestResponse {
    edit(app, token, &row.id.to_string(), body).await
}

/// `GET /api/v1/statuses/{id}/history` as `token` (`None`: anonymous).
async fn history(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}/history"));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `GET /api/v1/statuses/{id}/source` as `token`.
async fn source(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/api/v1/statuses/{id}/source"))
        .bearer(token)
        .send(app)
        .await
}

/// The rendered object's keys, sorted, for exact key-set assertions.
fn json_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The `content` of every entry of a history array.
fn contents_of(body: &Value) -> Vec<&str> {
    body.as_array()
        .expect("the history is an array")
        .iter()
        .map(|entry| entry["content"].as_str().expect("a content string"))
        .collect()
}

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

/// `media_ids` replaces the post's whole attachment set: the named upload
/// arrives, the previous one is freed, and the superseded version's history
/// entry still names the media the post wore before.
#[tokio::test]
async fn editing_media_replaces_the_posts_attachments() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "media-editor").await;
    let row = post(&pool, &alice, "pictures at an edit").await;
    let old = attach(&pool, &alice, &row, 0).await;
    let fresh = upload(&pool, &alice).await;

    let response = edit_row(
        &app,
        &token,
        &row,
        json!({"media_ids": [fresh.to_string()]}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the media edit passes");
    let body = response.json_value();
    let media = body["media_attachments"]
        .as_array()
        .expect("media is an array");
    assert_eq!(media.len(), 1, "the new set is the named one");
    assert_eq!(media[0]["id"], fresh.to_string());
    assert_eq!(
        stored_media(&pool, row.id).await,
        vec![fresh],
        "and the store agrees"
    );
    let old_row = media_attachment::get_by_id(&pool, old)
        .await
        .unwrap()
        .expect("the freed attachment stays in the table");
    assert!(
        old_row.status_id.is_none(),
        "the un-named one is freed, not deleted"
    );

    let versions = history(&app, Some(&token), &row.id.to_string())
        .await
        .json_value();
    let items = versions.as_array().expect("the history is an array");
    assert_eq!(items.len(), 2, "a media change superseded a version");
    let before = items[0]["media_attachments"]
        .as_array()
        .expect("media is an array");
    assert_eq!(before.len(), 1, "the snapshot names the old attachment");
    assert_eq!(before[0]["id"], old.to_string());
}

/// Media ids that name nothing the author may attach — unknown, malformed,
/// another account's upload — drop out of the replacement, exactly as
/// Pleroma's `attachments_from_ids` rejects its nils.
#[tokio::test]
async fn media_nobody_may_attach_drops_out_of_the_replacement() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "picker").await;
    let (bob, _) = editor(&pool, "other-picker").await;
    let row = post(&pool, &alice, "mine only").await;
    let mine = upload(&pool, &alice).await;
    let theirs = upload(&pool, &bob).await;

    let response = edit_row(
        &app,
        &token,
        &row,
        json!({"media_ids": [mine.to_string(), UNKNOWN_ID, "not-a-uuid", theirs.to_string()]}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the edit passes");
    assert_eq!(
        stored_media(&pool, row.id).await,
        vec![mine],
        "the usable id survives and the rest name nothing"
    );
}

/// An empty `media_ids` list strips the post of every attachment; an
/// absent one says nothing about media.
#[tokio::test]
async fn media_can_be_cleared_and_an_absent_list_touches_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "unposter").await;
    let row = post(&pool, &alice, "text only after all").await;
    let one = attach(&pool, &alice, &row, 0).await;

    let keep = edit_row(&app, &token, &row, json!({"spoiler_text": "cw"})).await;
    common::assert_answer(&keep, StatusCode::OK, "the non-media edit passes");
    assert_eq!(
        stored_media(&pool, row.id).await,
        vec![one],
        "a body that names no media leaves the post's set exactly as it is"
    );

    let cleared = edit_row(&app, &token, &row, json!({"media_ids": []})).await;
    common::assert_answer(&cleared, StatusCode::OK, "the clearing edit passes");
    assert_eq!(
        cleared.json_value()["media_attachments"],
        json!([]),
        "and a submitted empty list strips the post"
    );
    assert!(
        stored_media(&pool, row.id).await.is_empty(),
        "the post wears nothing after the clearing edit"
    );
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
