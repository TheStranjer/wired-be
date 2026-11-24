//! Federated note edits and deletions over HTTP (WBE-173): an inbound
//! `Update` or `Delete` folded by [`handle_incoming`] and what the client
//! API then answers.
//!
//! Every test delivers fixture activities through the real dispatcher — the
//! same job the inbox routes enqueue, run in-process — and reads the result
//! the way a client does. What this pins:
//!
//! - an `Edit` of a mirrored note rewrites its content, warning, tags,
//!   mentions and attachments, stamps `edited_at`, and the superseded
//!   version shows up as the first element of
//!   `GET /api/v1/statuses/{id}/history` — the ticket's proof that the
//!   federated edit records history exactly like the edit endpoint's;
//! - an `Update` no newer than the stored version changes nothing;
//! - an `Update` of a `Question` restates the poll's counts, which
//!   `GET /api/v1/polls/{id}` then renders;
//! - an `Update` for a post this instance never stored lands it (the
//!   dispatcher's own [`create`](wired_be::federation::handlers::create)
//!   fold — the same ingest answer), while a `Delete` from somebody other
//!   than the author leaves the post standing; and
//! - a `Delete` soft-deletes the author's own post — gone from
//!   `GET /api/v1/statuses/{id}` and off its parent's reply counter — and a
//!   `Delete` of an object never stored leaves a tombstone behind that
//!   swallows the post's later `Create`.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::{status, user::User};

use crate::common::{self, deliver, delivering_state};

/// The author every fixture here posts as, and delivers as.
const BOB: &str = "https://remote.example/users/bob";
/// The public addressing recipient the visibility parse reads.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// An edit of a mirrored note rewrites the post and its history: the new
/// content, warning and attachments on the status, the superseded version
/// first in `GET /statuses/{id}/history`.
#[tokio::test]
async fn an_edit_rewrites_the_note_and_its_history_shows_both_versions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;
    let mentioned = common::create_remote_user(&pool, "carol", "remote.example").await;
    let uri = format!("{BOB}/statuses/01EDIT");
    deliver(
        &state,
        &create_of(note(&uri, "<p>first words</p>", json!({}))),
        &signer,
    )
    .await;

    let id = public_entry(&app, &uri).await["id"]
        .as_str()
        .expect("the id is a string")
        .to_owned();
    let edited = note(
        &uri,
        "<p>second words</p>",
        json!({
            "updated": "2024-05-01T12:00:00.000Z",
            "summary": "careful now",
            "sensitive": true,
            "tag": [
                { "type": "Hashtag", "href": "https://remote.example/tags/rust", "name": "#Rust" },
                { "type": "Mention", "href": mentioned.ap_id, "name": "@carol@remote.example" },
            ],
            "attachment": [{
                "type": "Document",
                "mediaType": "image/png",
                "url": "https://remote.example/media/01EDIT.png",
            }],
        }),
    );
    deliver(&state, &update_of(edited), &signer).await;

    let shown = status_entity(&app, &id).await;
    assert_eq!(shown["content"], "<p>second words</p>", "the new text");
    assert_eq!(shown["spoiler_text"], "careful now", "the new warning");
    assert_eq!(shown["sensitive"], true, "folded behind it");
    assert!(
        shown["edited_at"] != Value::Null,
        "and the edit stamped on the post"
    );
    let tags: Vec<&str> = shown["tags"]
        .as_array()
        .expect("tags is an array")
        .iter()
        .map(|tag| tag["name"].as_str().expect("a named tag"))
        .collect();
    assert_eq!(tags, ["rust"], "the update's hashtag is the post's tag now");
    let mentions: Vec<&str> = shown["mentions"]
        .as_array()
        .expect("mentions is an array")
        .iter()
        .map(|m| m["acct"].as_str().expect("an acounted mention"))
        .collect();
    assert_eq!(mentions, ["carol@remote.example"], "and its mention");
    assert_eq!(
        shown["media_attachments"]
            .as_array()
            .expect("media is an array")
            .len(),
        1,
        "and the attachment the update named"
    );

    let history = history(&app, &id).await;
    let versions = history.as_array().expect("the history is an array");
    assert_eq!(versions.len(), 2, "the old version beside the current one");
    assert_eq!(versions[0]["content"], "<p>first words</p>");
    assert_eq!(versions[1]["content"], "<p>second words</p>");
    assert_eq!(
        versions[0]["media_attachments"]
            .as_array()
            .expect("the revision's media list")
            .len(),
        0,
        "the superseded version wore no media"
    );
}

/// Two `Update`s, the second older than the first: the post keeps the
/// newer version — the stale update is ignored, history and all.
#[tokio::test]
async fn an_update_older_than_the_stored_version_changes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;
    let uri = format!("{BOB}/statuses/01STALE");
    deliver(
        &state,
        &create_of(note(&uri, "<p>v1</p>", json!({}))),
        &signer,
    )
    .await;

    deliver(
        &state,
        &update_of(note(
            &uri,
            "<p>v2</p>",
            json!({"updated": "2024-05-01T12:00:00.000Z"}),
        )),
        &signer,
    )
    .await;
    deliver(
        &state,
        &update_of(note(
            &uri,
            "<p>v1 again</p>",
            json!({"updated": "2024-05-01T11:00:00.000Z"}),
        )),
        &signer,
    )
    .await;

    let id = public_entry(&app, &uri).await["id"]
        .as_str()
        .expect("the id is a string")
        .to_owned();
    assert_eq!(
        status_entity(&app, &id).await["content"],
        "<p>v2</p>",
        "the stale update rewrote nothing"
    );
    let versions = history(&app, &id).await;
    assert_eq!(
        versions.as_array().expect("the history").len(),
        2,
        "and recorded no third version: one superseded, one current"
    );
}

/// The `Update` of a mirrored `Question` restates the poll's totals — the
/// remote-poll-results path — through `GET /api/v1/polls/{id}`.
#[tokio::test]
async fn a_question_update_moves_the_poll_counts_the_api_renders() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;
    let uri = format!("{BOB}/statuses/01POLL");
    deliver(
        &state,
        &create_of(question(&uri, [10, 8], 20, "2024-06-01T00:00:00Z", None)),
        &signer,
    )
    .await;

    let entry = public_entry(&app, &uri).await;
    let poll_id = entry["poll"]["id"]
        .as_str()
        .expect("the question landed with its poll")
        .to_owned();

    deliver(
        &state,
        &update_of(question(
            &uri,
            [30, 15],
            42,
            "2024-06-02T00:00:00Z",
            Some("2024-05-02T10:00:00.000Z"),
        )),
        &signer,
    )
    .await;

    let response = common::TestRequest::get(&format!("/api/v1/polls/{poll_id}"))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the poll reads");
    let rendered = response.json_value();
    assert_eq!(rendered["votes_count"], 45, "the remote's total wins");
    assert_eq!(rendered["voters_count"], 42, "and its voter count");
    let counts: Vec<i64> = rendered["options"]
        .as_array()
        .expect("options is an array")
        .iter()
        .map(|option| option["votes_count"].as_i64().expect("a counted option"))
        .collect();
    assert_eq!(counts, vec![30, 15], "option by option");
}

/// An `Update` for a post this instance never stored is that post's first
/// landing: the note arrives, exactly as its document describes it.
#[tokio::test]
async fn an_update_for_an_unknown_note_lands_the_post() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;
    let uri = format!("{BOB}/statuses/01UPDATEFIRST");

    deliver(
        &state,
        &update_of(note(&uri, "<p>arrived by update</p>", json!({}))),
        &signer,
    )
    .await;

    let entry = public_entry(&app, &uri).await;
    assert_eq!(
        entry["content"], "<p>arrived by update</p>",
        "the unknown object was ingested, not edited against nothing"
    );
    assert_eq!(entry["account"]["acct"], "bob@remote.example");
}

/// A `Delete` of the author's own post takes it down: `GET /statuses/{id}`
/// answers 404 and its parent's reply counter steps back down.
#[tokio::test]
async fn a_delete_takes_the_post_down_and_fixes_its_parents_counter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;
    let parent_uri = format!("{BOB}/statuses/01PARENT");
    let reply_uri = format!("{BOB}/statuses/01REPLY");
    deliver(
        &state,
        &create_of(note(&parent_uri, "<p>parent</p>", json!({}))),
        &signer,
    )
    .await;
    deliver(
        &state,
        &create_of(note(
            &reply_uri,
            "<p>answer</p>",
            json!({"inReplyTo": parent_uri}),
        )),
        &signer,
    )
    .await;

    let parent = public_entry(&app, &parent_uri).await;
    let parent_id = parent["id"].as_str().expect("a parent id").to_owned();
    assert_eq!(
        parent["replies_count"], 1,
        "the reply counted on the parent"
    );
    let reply_id = public_entry(&app, &reply_uri).await["id"]
        .as_str()
        .expect("the reply has a local id")
        .to_owned();

    deliver(&state, &delete_of(&reply_uri), &signer).await;

    let gone = common::TestRequest::get(&format!("/api/v1/statuses/{reply_id}"))
        .send(&app)
        .await;
    assert_eq!(
        gone.status,
        StatusCode::NOT_FOUND,
        "the deleted post is gone from the reads"
    );
    let after = status_entity(&app, &parent_id).await;
    assert_eq!(
        after["replies_count"], 0,
        "and its counter stepped back down"
    );

    let tombstoned = status::get_by_uri(&pool, &reply_uri)
        .await
        .expect("the uri read")
        .expect("the row stays");
    assert!(
        tombstoned.deleted_at.is_some(),
        "as a tombstone keeping the uri reserved"
    );
}

/// A `Delete` for an object this instance never stored leaves a tombstone
/// behind — and the object's late `Create` is swallowed by it.
#[tokio::test]
async fn a_delete_before_arrival_tombstones_the_uri_and_swallows_the_create() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;
    let uri = format!("{BOB}/statuses/01LATE");

    deliver(&state, &delete_of(&uri), &signer).await;

    let tombstone = status::get_by_uri(&pool, &uri)
        .await
        .expect("the uri read")
        .expect("the tombstone was reserved");
    assert!(tombstone.deleted_at.is_some(), "born deleted");
    assert_eq!(tombstone.user_id, signer.id, "under its author");

    deliver(
        &state,
        &create_of(note(&uri, "<p>too late</p>", json!({}))),
        &signer,
    )
    .await;

    let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(&uri)
        .fetch_one(&pool)
        .await
        .expect("the row count reads");
    assert_eq!(rows, 1, "still exactly the one tombstone");
    let shown = public_timeline(&app)
        .await
        .into_iter()
        .any(|entry| entry["uri"] == json!(uri));
    assert!(
        !shown,
        "and the post that arrived after its delete never lands"
    );
}

/// A `Delete` signed by somebody other than the post's author is ignored:
/// one account does not withdraw another's post.
#[tokio::test]
async fn a_delete_by_somebody_other_than_the_author_is_ignored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;
    let erin = common::create_remote_user(&pool, "erin", "remote.example").await;
    let uri = format!("{BOB}/statuses/01NOTYOURS");
    deliver(
        &state,
        &create_of(note(&uri, "<p>still here</p>", json!({}))),
        &signer,
    )
    .await;

    let mut attempted = delete_of(&uri);
    attempted["actor"] = json!(erin.ap_id);
    deliver(&state, &attempted, &erin).await;

    let entry = public_entry(&app, &uri).await;
    assert_eq!(
        entry["content"], "<p>still here</p>",
        "the foreign delete moved nothing"
    );
}

/// The object the fixtures edit: a `Note` under `uri` of `content`, with
/// every extra field `extra` merged over it.
fn note(uri: &str, content: &str, extra: Value) -> Value {
    let mut object = json!({
        "id": uri,
        "type": "Note",
        "attributedTo": BOB,
        "to": [PUBLIC],
        "content": content,
        "published": "2024-05-01T10:00:00.000Z",
    });
    let Value::Object(additions) = extra else {
        panic!("the extras are a JSON object");
    };
    for (key, value) in additions {
        object[key] = value;
    }
    object
}

/// A `Question` the fixtures poll: options with the `counts` votes
/// (`tea`/`coffee`), the voters reported, voting closed at `until`, stamped
/// `updated` when a caller supplies one (the create passes `None`, the
/// poll-restating update a time after its own `published`).
fn question(uri: &str, counts: [u32; 2], voters: u32, until: &str, updated: Option<&str>) -> Value {
    let mut extra = json!({
        "type": "Question",
        "endTime": until,
        "votersCount": voters,
        "oneOf": [
            { "type": "Note", "name": "Tea",
              "replies": { "type": "Collection", "totalItems": counts[0] } },
            { "type": "Note", "name": "Coffee",
              "replies": { "type": "Collection", "totalItems": counts[1] } },
        ],
    });
    if let Some(stamp) = updated {
        extra["updated"] = json!(stamp);
    }
    note(uri, "<p>tea or coffee?</p>", extra)
}

/// A `Create` activity wrapping `object`.
fn create_of(object: Value) -> Value {
    activity("Create", object)
}

/// An `Update` activity wrapping `object`.
fn update_of(object: Value) -> Value {
    activity("Update", object)
}

/// The activity itself: type, actor, a fresh unique id, `object`.
fn activity(kind: &str, object: Value) -> Value {
    let mut envelope = json!({
        "id": format!("https://remote.example/activities/{kind}-{}", uuid::Uuid::now_v7()),
        "type": kind,
        "actor": BOB,
    });
    envelope["object"] = object;
    envelope
}

/// A `Delete` whose object is the bare id `uri` — the shape Mastodon sends.
fn delete_of(uri: &str) -> Value {
    json!({
        "id": format!("https://remote.example/activities/Delete-{}", uuid::Uuid::now_v7()),
        "type": "Delete",
        "actor": BOB,
        "object": uri,
    })
}

/// The remote author: `bob@remote.example`, host of every fixture id.
async fn bob(pool: &PgPool) -> User {
    common::create_remote_user(pool, "bob", "remote.example").await
}

/// `GET /api/v1/statuses/{id}`, anonymous: the stored post's entity.
async fn status_entity(app: &Router, id: &str) -> Value {
    let response = common::TestRequest::get(&format!("/api/v1/statuses/{id}"))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the status read");
    response.json_value()
}

/// `GET /api/v1/statuses/{id}/history`, anonymous — a public post shows
/// its versions to whoever may read the post.
async fn history(app: &Router, id: &str) -> Value {
    let response = common::TestRequest::get(&format!("/api/v1/statuses/{id}/history"))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the history read");
    response.json_value()
}

/// `GET /api/v1/timelines/public`, anonymous.
async fn public_timeline(app: &Router) -> Vec<Value> {
    let response = common::TestRequest::get("/api/v1/timelines/public")
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the anonymous public timeline");
    response
        .json_value()
        .as_array()
        .expect("the timeline is an array")
        .clone()
}

/// The entity of the status with object uri `uri` from the public
/// timeline, refusing to be absent.
async fn public_entry(app: &Router, uri: &str) -> Value {
    public_timeline(app)
        .await
        .into_iter()
        .find(|entry| entry["uri"] == json!(uri))
        .unwrap_or_else(|| panic!("the public timeline should carry {uri}"))
}
