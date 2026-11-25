//! The `quote` and `quoted_update` notifications over HTTP (WBE-804): a
//! post quoting a local user's post tells its author, observed through
//! `GET /api/v1/notifications`.
//!
//! What this pins:
//!
//! - an inbound `Create` that quotes a local post tells its author `quote`,
//!   naming the quoting status with its author as the actor, once;
//! - a local quote tells the local author, and a self-quote tells nobody;
//! - a quote from a blocked account tells nobody;
//! - an inbound `Update` of a quoting status tells `quoted_update`;
//! - `exclude_types[]=quote` filters the notification out of the listing.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::{
    status::{self, Status, Visibility},
    user::User,
};

use crate::common::{self, deliver, delivering_state};

/// The remote quoter's actor id.
const BOB: &str = "https://remote.example/users/bob";
/// The addressing of a public post.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The scopes the recipients' tokens carry.
const SCOPES: &str = "read write follow";

/// A local user named `nickname` beside a bearer token.
async fn fixture(pool: &PgPool, nickname: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, SCOPES).await;
    (user, token)
}

/// The caller's listing under `query`, as JSON entities.
async fn listed(app: &Router, token: &str, query: &str) -> Vec<Value> {
    let response = common::TestRequest::get(&format!("/api/v1/notifications{query}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the notification listing");
    response
        .json_value()
        .as_array()
        .cloned()
        .expect("the listing answers an array")
}

/// The listing narrowed to `(type, actor id, status id)` triples.
async fn facts(app: &Router, token: &str) -> Vec<(String, String, String)> {
    listed(app, token, "")
        .await
        .iter()
        .map(|entry| {
            (
                entry["type"].as_str().expect("a type").to_owned(),
                entry["account"]["id"]
                    .as_str()
                    .expect("an actor")
                    .to_owned(),
                entry["status"]["id"].as_str().expect("a status").to_owned(),
            )
        })
        .collect()
}

/// A `Create` or `Update` of bob's note `uri`, quoting `quoted_uri`.
fn quoting_activity(kind: &str, uri: &str, quoted_uri: &str, content: &str) -> Value {
    let mut object = json!({
        "id": uri,
        "type": "Note",
        "attributedTo": BOB,
        "to": [PUBLIC],
        "content": content,
        "published": "2024-05-01T10:00:00.000Z",
        "quoteUrl": quoted_uri,
    });
    if kind == "Update" {
        object["updated"] = json!("2024-05-02T10:00:00.000Z");
    }
    json!({
        "id": format!("https://remote.example/activities/{kind}-{}", uuid::Uuid::now_v7()),
        "type": kind,
        "actor": BOB,
        "object": object,
    })
}

/// The stored status under object uri `uri`.
async fn stored(pool: &PgPool, uri: &str) -> Status {
    status::get_by_uri(pool, uri)
        .await
        .expect("the uri read")
        .expect("the status is stored")
}

/// An inbound `Create` that quotes a local post tells its author `quote`,
/// naming the quoting status; a redelivered object adds nothing.
#[tokio::test]
async fn an_inbound_quote_notifies_the_quoted_author() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, token) = fixture(&pool, "alice").await;
    let original = common::create_status(&pool, &alice, "quote me", Visibility::Public).await;
    let uri = "https://remote.example/users/bob/statuses/quote-1";

    let create = quoting_activity("Create", uri, &original.uri, "<p>my take</p>");
    deliver(&state, &create, &bob).await;
    deliver(&state, &create, &bob).await;

    let quoting = stored(&pool, uri).await;
    assert_eq!(
        quoting.quote_of_id,
        Some(original.id),
        "the quote is linked"
    );
    assert_eq!(
        facts(&app, &token).await,
        vec![(
            "quote".to_owned(),
            bob.id.to_string(),
            quoting.id.to_string()
        )],
        "one quote notification, from the quoter, naming the quoting status"
    );
}

/// `exclude_types[]=quote` removes the notification from the listing.
#[tokio::test]
async fn exclude_types_filters_the_quote_out() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, token) = fixture(&pool, "alice").await;
    let original = common::create_status(&pool, &alice, "quote me", Visibility::Public).await;
    let uri = "https://remote.example/users/bob/statuses/quote-2";
    deliver(
        &state,
        &quoting_activity("Create", uri, &original.uri, "<p>take</p>"),
        &bob,
    )
    .await;

    assert_eq!(listed(&app, &token, "").await.len(), 1, "it is listed");
    assert!(
        listed(&app, &token, "?exclude_types[]=quote")
            .await
            .is_empty(),
        "exclude_types[]=quote hides it"
    );
    assert_eq!(
        listed(&app, &token, "?types[]=quote").await.len(),
        1,
        "types[]=quote keeps it"
    );
}

/// A quote from an account the quoted author blocked tells nobody.
#[tokio::test]
async fn a_quote_from_a_blocked_account_is_silent() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, token) = fixture(&pool, "alice").await;
    let original = common::create_status(&pool, &alice, "quote me", Visibility::Public).await;
    let blocked = common::TestRequest::post(&format!("/api/v1/accounts/{}/block", bob.id))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&blocked, StatusCode::OK, "the block write");

    let uri = "https://remote.example/users/bob/statuses/quote-3";
    deliver(
        &state,
        &quoting_activity("Create", uri, &original.uri, "<p>take</p>"),
        &bob,
    )
    .await;

    assert!(
        listed(&app, &token, "?with_muted=true").await.is_empty(),
        "a blocked quoter's quote is not told"
    );
}

/// An inbound `Update` of a quoting status tells `quoted_update`.
#[tokio::test]
async fn an_inbound_edit_of_a_quote_notifies_quoted_update() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, token) = fixture(&pool, "alice").await;
    let original = common::create_status(&pool, &alice, "quote me", Visibility::Public).await;
    let uri = "https://remote.example/users/bob/statuses/quote-4";
    deliver(
        &state,
        &quoting_activity("Create", uri, &original.uri, "<p>first</p>"),
        &bob,
    )
    .await;

    deliver(
        &state,
        &quoting_activity("Update", uri, &original.uri, "<p>second</p>"),
        &bob,
    )
    .await;

    let quoting = stored(&pool, uri).await;
    let mut types: Vec<(String, String, String)> = facts(&app, &token).await;
    types.sort();
    assert_eq!(
        types,
        vec![
            (
                "quote".to_owned(),
                bob.id.to_string(),
                quoting.id.to_string()
            ),
            (
                "quoted_update".to_owned(),
                bob.id.to_string(),
                quoting.id.to_string()
            ),
        ],
        "the create told `quote`, the edit told `quoted_update`"
    );
}

/// A local quote tells the local author; quoting oneself tells nobody.
#[tokio::test]
async fn a_local_quote_notifies_and_a_self_quote_does_not() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "alice").await;
    let (bob, bob_token) = fixture(&pool, "bob").await;
    let original = common::create_status(&pool, &alice, "quote me", Visibility::Public).await;

    let own = common::TestRequest::post("/api/v1/statuses")
        .bearer(&alice_token)
        .json(&json!({"status": "quoting myself", "quote_id": original.id.to_string()}))
        .send(&app)
        .await;
    common::assert_answer(&own, StatusCode::OK, "the self-quote post");
    assert!(
        facts(&app, &alice_token).await.is_empty(),
        "a self-quote tells nobody"
    );

    let theirs = common::TestRequest::post("/api/v1/statuses")
        .bearer(&bob_token)
        .json(&json!({"status": "quoting alice", "quote_id": original.id.to_string()}))
        .send(&app)
        .await;
    common::assert_answer(&theirs, StatusCode::OK, "bob's quote post");
    let quoting_id = theirs.json_value()["id"]
        .as_str()
        .expect("the entity names its id")
        .to_owned();

    assert_eq!(
        facts(&app, &alice_token).await,
        vec![("quote".to_owned(), bob.id.to_string(), quoting_id)],
        "the local author is told of the local quote"
    );
    assert!(
        facts(&app, &bob_token).await.is_empty(),
        "the quoter hears nothing"
    );
}
