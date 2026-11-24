//! Integration tests for quote consent (FEP-044f): a remote `QuoteRequest`
//! for a local post, the `Accept` it queues, and the `QuoteAuthorization`
//! document served at `GET /quote_authorizations/{uuid}` — the address that
//! `Accept` names as its `result` and Mastodon fetches before it shows the
//! quote as accepted.
//!
//! The handler runs through [`handle_quote_request`] over the same pool the
//! router reads, so the `result` the test follows is exactly the one a peer
//! would be sent. The document builders and the handler's refusals are
//! covered where they live, in `src/federation/quotes.rs` and
//! `src/federation/handlers/quotes`; what these tests own is the route and
//! the loop from the delivery to the document.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    federation::{
        delivery::KIND_DELIVER,
        dispatch::{ActivityView, ObjectRef, Outcome},
        handlers::quotes::handle_quote_request,
    },
    models::{
        quote_authorization,
        status::{self, Status, Visibility},
        user::User,
    },
};

use crate::common;

/// The media type of an `ActivityPub` document, charset included.
const ACTIVITY_JSON: &str = "application/activity+json; charset=utf-8";
/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";

/// `GET`s `path` as an `ActivityPub` client.
async fn fetch_ap(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", "application/activity+json")
        .send(app)
        .await
}

/// The quoted post's author, the quoter and the quoted post.
async fn setup(pool: &PgPool) -> (User, User, Status) {
    let alice = common::create_local_user(pool, "alice").await;
    let bob = common::create_remote_user(pool, "bob", "remote.example").await;
    let note = common::create_status(pool, &alice, "quote me", Visibility::Public).await;
    (alice, bob, note)
}

/// The object id of bob's quoting post number `n`.
fn quoting_uri(n: u8) -> String {
    format!("https://remote.example/objects/bob-quote-{n}")
}

/// Delivers bob's request number `n` to quote `note`, with the quoting note
/// embedded, and answers the `Accept` the handler queued for him.
async fn request_quote(pool: &PgPool, bob: &User, note: &Status, n: u8) -> Value {
    let request_id = format!("https://remote.example/activities/quote-request-{n}");
    let raw = json!({
        "id": request_id,
        "type": "QuoteRequest",
        "actor": bob.ap_id,
        "object": note.uri,
        "instrument": {
            "id": quoting_uri(n),
            "type": "Note",
            "attributedTo": bob.ap_id,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": "<p>bob quoting</p>",
            "quote": note.uri,
        },
    });
    let view = ActivityView {
        id: request_id.clone(),
        kind: "QuoteRequest".to_owned(),
        actor: bob.ap_id.clone(),
        object: ObjectRef::Id(note.uri.clone()),
        target: None,
        to: Vec::new(),
        cc: Vec::new(),
        raw,
        inbox_owner: None,
    };
    let state = common::test_app_state(pool.clone(), common::test_config());

    let outcome = handle_quote_request(state, view, bob.clone())
        .await
        .expect("the request is handled");

    assert_eq!(outcome, Outcome::Handled, "the request is answered");
    sqlx::query_scalar::<_, Value>(
        "SELECT args -> 'body' FROM jobs
          WHERE kind = $1 AND args -> 'body' -> 'object' ->> 'id' = $2",
    )
    .bind(KIND_DELIVER)
    .bind(&request_id)
    .fetch_one(pool)
    .await
    .expect("one answer is queued for the request")
}

/// The path of the document an `Accept` names as its `result`.
fn result_path(accept: &Value) -> String {
    let result = accept["result"]
        .as_str()
        .expect("the Accept names a result");
    result
        .strip_prefix(BASE)
        .expect("the result is on this instance")
        .to_owned()
}

/// The accepted request's document is served at the `result` of the
/// `Accept`, with exactly the keys Mastodon's verifier reads: the quoting
/// post as `interactingObject` and the quoted one as `interactionTarget`.
#[tokio::test]
async fn the_accept_result_serves_the_authorization_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, bob, note) = setup(&pool).await;
    let accept = request_quote(&pool, &bob, &note, 1).await;
    let app = common::test_app(pool);
    let path = result_path(&accept);

    let response = fetch_ap(&app, &path).await;

    assert_eq!(response.status, StatusCode::OK, "the grant is served");
    assert_eq!(
        response.header("content-type"),
        Some(ACTIVITY_JSON),
        "as an ActivityPub document"
    );
    assert_eq!(response.header("vary"), Some("Accept"), "varying on Accept");
    assert_eq!(accept["type"], "Accept", "the answer is an Accept");
    assert_eq!(
        response.json_value(),
        json!({
            "@context": [
                "https://www.w3.org/ns/activitystreams",
                {
                    "gts": "https://gotosocial.org/ns#",
                    "QuoteAuthorization": "https://w3id.org/fep/044f#QuoteAuthorization",
                    "interactingObject": {"@id": "gts:interactingObject", "@type": "@id"},
                    "interactionTarget": {"@id": "gts:interactionTarget", "@type": "@id"},
                },
            ],
            "id": format!("{BASE}{path}"),
            "type": "QuoteAuthorization",
            "attributedTo": alice.ap_id,
            "interactingObject": quoting_uri(1),
            "interactionTarget": note.uri,
        }),
        "the document, field for field"
    );
}

/// An id that names no grant, one that is no id, and a client that does not
/// speak `ActivityPub` all get the plain 404.
#[tokio::test]
async fn unknown_ids_and_other_clients_get_the_plain_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, bob, note) = setup(&pool).await;
    let accept = request_quote(&pool, &bob, &note, 1).await;
    let app = common::test_app(pool);
    let known = result_path(&accept);

    for path in [
        format!("/quote_authorizations/{}", Uuid::now_v7()),
        "/quote_authorizations/not-an-id".to_owned(),
    ] {
        let response = fetch_ap(&app, &path).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "GET {path}");
        assert_eq!(
            response.json_value(),
            json!({"error": "Not found"}),
            "the plain 404 body for {path}"
        );
    }
    let bare = common::TestRequest::get(&known).send(&app).await;
    assert_eq!(
        bare.status,
        StatusCode::NOT_FOUND,
        "a known grant is no document without an Accept"
    );
    for other in ["text/html", "application/json"] {
        let response = common::TestRequest::get(&known)
            .header("accept", other)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "a known grant is no document for Accept {other}"
        );
    }
}

/// A grant that was withdrawn, and one whose quoted post was deleted, are
/// 410: the quote is no longer authorized.
#[tokio::test]
async fn a_revoked_grant_or_a_deleted_post_is_gone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, bob, note) = setup(&pool).await;
    let other = common::create_status(&pool, &alice, "another post", Visibility::Public).await;
    let first = result_path(&request_quote(&pool, &bob, &note, 1).await);
    let second = result_path(&request_quote(&pool, &bob, &other, 2).await);
    let app = common::test_app(pool.clone());
    assert_eq!(
        fetch_ap(&app, &first).await.status,
        StatusCode::OK,
        "the first grant stands"
    );
    assert_eq!(
        fetch_ap(&app, &second).await.status,
        StatusCode::OK,
        "so does the second"
    );
    let first_id: Uuid = first
        .rsplit('/')
        .next()
        .expect("the path ends in an id")
        .parse()
        .expect("the id is a uuid");

    let revoked = quote_authorization::revoke(&pool, first_id)
        .await
        .expect("the revoke runs");
    let tombstone = status::soft_delete(&pool, other.id)
        .await
        .expect("the delete runs");

    assert!(revoked, "the first grant stood");
    assert!(tombstone.is_some(), "the second post was live");
    assert_eq!(
        fetch_ap(&app, &first).await.status,
        StatusCode::GONE,
        "a revoked grant is gone"
    );
    assert_eq!(
        fetch_ap(&app, &second).await.status,
        StatusCode::GONE,
        "a grant of a deleted post is gone"
    );
}
