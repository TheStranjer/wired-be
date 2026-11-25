//! The relay actor's own endpoints: `GET /relay` answers the instance
//! actor document, `GET /relay/following` and `GET /relay/followers`
//! answer empty `OrderedCollection`s, and `POST /relay/inbox` is the
//! shared inbox handler — all under the usual `Accept` negotiation.

use axum::http::StatusCode;
use chrono::Utc;
use serde_json::{Value, json};
use wired_be::{
    federation::{
        dispatch::KIND_INCOMING,
        signature::{SigningKey, sign_request},
    },
    services::instance_actor::{INSTANCE_ACTOR_PATH, ensure_instance_actor},
};

use super::support::{ACTIVITY_JSON, BASE};
use crate::common;

// --- the endpoints --------------------------------------------------------------------

/// `GET /relay` answers the instance actor document — the very JSON
/// `GET /internal/fetch` serves — under the usual negotiation: an
/// `Accept` naming an `ActivityPub` media type gets it, anyone else the
/// plain 404, and every answer says `Vary: Accept`.
#[tokio::test]
async fn the_relay_route_serves_the_instance_actor_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);

    let internal = common::TestRequest::get(INSTANCE_ACTOR_PATH)
        .header("accept", "application/activity+json")
        .send(&app)
        .await;
    let relay = common::TestRequest::get("/relay")
        .header("accept", "application/activity+json")
        .send(&app)
        .await;
    assert_eq!(relay.status, StatusCode::OK, "GET /relay answers a peer");
    assert_eq!(
        relay.header("content-type"),
        Some(ACTIVITY_JSON),
        "as an ActivityPub document"
    );
    assert_eq!(
        relay.header("vary"),
        Some("Accept"),
        "and the negotiation is cache-visible"
    );
    let document = relay.json_value();
    assert_eq!(
        document,
        internal.json_value(),
        "the very document /internal/fetch serves"
    );
    assert_eq!(
        document["id"],
        format!("{BASE}{INSTANCE_ACTOR_PATH}"),
        "under the instance actor's id"
    );

    let silent = common::TestRequest::get("/relay").send(&app).await;
    common::assert_answer(
        &silent,
        StatusCode::NOT_FOUND,
        "a client that states no Accept",
    );
    let browser = common::TestRequest::get("/relay")
        .header("accept", "text/html")
        .send(&app)
        .await;
    common::assert_error(&browser, "Not found", "a browser on the relay actor");
}

/// The two relay subscription collections answer empty
/// `OrderedCollection`s at their own addresses, under the same
/// negotiation: a peer gets the root, a silent client the plain 404.
#[tokio::test]
async fn the_relay_collections_are_empty_ordered_collections() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);

    for side in ["following", "followers"] {
        let path = format!("/relay/{side}");
        let asked = common::TestRequest::get(&path)
            .header("accept", "application/ld+json")
            .send(&app)
            .await;
        assert_eq!(asked.status, StatusCode::OK, "GET {path} answers a peer");
        assert_eq!(
            asked.header("content-type"),
            Some(ACTIVITY_JSON),
            "{path} is an ActivityPub document"
        );
        assert_eq!(
            asked.header("vary"),
            Some("Accept"),
            "and the answer varies on Accept"
        );
        let collection = asked.json_value();
        assert_eq!(
            collection["id"],
            format!("{BASE}/relay/{side}"),
            "the collection names its own address"
        );
        assert_eq!(
            collection["type"], "OrderedCollection",
            "the ticket's OrderedCollection"
        );
        assert_eq!(collection["totalItems"], 0, "empty");
        assert_eq!(
            collection["first"],
            format!("{BASE}/relay/{side}?page=true"),
            "with the paged-first link the other roots carry"
        );
        assert!(
            collection.get("@context").is_some(),
            "and the JSON-LD context every document carries"
        );

        let quiet = common::TestRequest::get(&path).send(&app).await;
        common::assert_answer(
            &quiet,
            StatusCode::NOT_FOUND,
            &format!("a silent client on {path}"),
        );
    }
}

/// `POST /relay/inbox` is the shared inbox: a signed delivery is queued as
/// one `federator.incoming` job with no `inbox_owner` and answered 202,
/// and an unsigned one is the extractor's 401.
#[tokio::test]
async fn the_relay_inbox_is_the_shared_inbox() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let config = common::test_config();
    let actor = ensure_instance_actor(&pool, &config)
        .await
        .expect("the instance actor is provisioned");
    let private_pem = common::actor_of(&pool, &actor)
        .await
        .private_key
        .expect("the provisioned actor holds its private key");
    let key_id = format!("{}#main-key", actor.ap_id);
    let app = common::test_app(pool.clone());

    let unsigned = common::TestRequest::post("/relay/inbox")
        .json(&json!({
            "id": "https://peer.example/activities/un-signed",
            "type": "Follow",
            "actor": "https://peer.example/users/alice",
            "object": format!("{BASE}/users/bob"),
        }))
        .send(&app)
        .await;
    assert_eq!(
        unsigned.status,
        StatusCode::UNAUTHORIZED,
        "an unsigned delivery is refused"
    );

    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://peer.example/activities/relay-signed",
        "type": "Follow",
        "actor": actor.ap_id,
        "object": format!("{BASE}/users/bob"),
    });
    let wire = sign_request(
        "POST",
        &format!("{BASE}/relay/inbox"),
        Some(&serde_json::to_vec(&activity).expect("the test activity serializes")),
        &SigningKey {
            key_id: &key_id,
            private_pem: &private_pem,
        },
        Utc::now(),
    )
    .expect("signing the test delivery");
    let accepted = common::TestRequest::post("/relay/inbox")
        .json(&activity)
        .header("host", &wire.host)
        .header("date", &wire.date)
        .header("digest", wire.digest.as_deref().expect("a body digest"))
        .header("signature", &wire.signature)
        .send(&app)
        .await;
    assert_eq!(
        accepted.status,
        StatusCode::ACCEPTED,
        "the relay inbox accepts a signed delivery"
    );
    assert_eq!(accepted.text(), "", "with an empty body");

    let rows = sqlx::query_as::<_, (Value,)>("SELECT args FROM jobs WHERE kind = $1")
        .bind(KIND_INCOMING)
        .fetch_all(&pool)
        .await
        .expect("the incoming jobs read");
    assert_eq!(rows.len(), 1, "exactly one job was queued");
    assert_eq!(
        rows[0].0["inbox_owner"],
        Value::Null,
        "the shared handler claims no owner"
    );
    assert_eq!(
        rows[0].0["signer_id"],
        actor.id.to_string(),
        "the signer is recorded"
    );
    assert_eq!(rows[0].0["activity"], activity, "and the activity rides");
}
