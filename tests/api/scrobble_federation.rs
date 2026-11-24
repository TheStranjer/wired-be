//! Scrobbles federating out (WBE-763): `POST /api/v1/pleroma/scrobble`
//! queues a Pleroma-compatible `Listen` wrapping an `Audio` object, and the
//! object and activity routes serve those documents back.
//!
//! The queued document is [`wired_be::federation::activities::listen_json`]:
//! `id` `{BASE_URL}/activities/{id}`, the `Audio` embedded under
//! `{BASE_URL}/objects/{id}`, `to`/`cc` by the scrobble's visibility. The
//! publisher's follower expansion, run here by the queued job's own handler,
//! turns the addresses into the `federator.deliver` jobs this file counts. A
//! `direct` scrobble addresses nobody and queues nothing. `GET
//! /objects/{id}` serves the `Audio` of a `public` or `unlisted` scrobble and
//! answers 404 for a `private` or `direct` one.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    federation::{delivery, publisher},
    jobs::AttemptInfo,
    models::{
        follow::{self, FollowState},
        user::{self, User},
    },
};

use crate::common;

/// The `ActivityStreams` public address.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The instance origin the fixtures run under.
const BASE: &str = TEST_BASE_URL;
/// The first attempt a background handler runs on.
const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: 5,
};

/// A local account with the endpoints and keys a registered one carries.
async fn listener(pool: &PgPool, nickname: &str) -> User {
    let ap_id = format!("{BASE}/users/{nickname}");
    let stored = ap_id.clone();
    let inbox = user::inbox(&ap_id);
    let followers = user::followers(&ap_id);
    common::create_local_user_with(pool, nickname, move |mut new| {
        new.ap_id = stored;
        new.inbox = Some(inbox);
        new.followers_url = Some(followers);
        new.private_key = Some("PEM".to_owned());
        new.public_key = Some("PUB".to_owned());
        new
    })
    .await
}

/// Records `follower`'s accepted follow of `target`.
async fn followed(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, BASE, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(row.state, FollowState::Accepted, "an open account accepts");
}

/// The activities of the queued `federator.publish` jobs, in order.
async fn published(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args->'activity' FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read")
    .into_iter()
    .map(|(activity,)| activity)
    .collect()
}

/// The `(inbox, body)` pairs of the queued `federator.deliver` jobs.
async fn deliveries(pool: &PgPool) -> Vec<(String, Value)> {
    sqlx::query_as::<_, (String, Value)>(
        "SELECT args->>'inbox', args->'body' FROM jobs WHERE kind = $1 ORDER BY 1",
    )
    .bind(delivery::KIND_DELIVER)
    .fetch_all(pool)
    .await
    .expect("the delivery jobs read")
}

/// Runs every queued publish through the publisher's own handler.
async fn fan_out(pool: &PgPool) {
    let state = common::test_app_state(pool.clone(), common::test_config());
    let rows =
        sqlx::query_as::<_, (Value,)>("SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at")
            .bind(publisher::KIND_PUBLISH)
            .fetch_all(pool)
            .await
            .expect("the publish jobs read");
    for (args,) in rows {
        publisher::handle(state.clone(), FIRST_ATTEMPT, args)
            .await
            .expect("the fan-out job completes");
    }
}

/// `POST /api/v1/pleroma/scrobble` of a track at `visibility`, answered as
/// the created entity.
async fn scrobble(app: &Router, token: &str, visibility: &str) -> Value {
    let response = common::TestRequest::post("/api/v1/pleroma/scrobble")
        .bearer(token)
        .json(&json!({
            "title": "Wired Song",
            "artist": "The Lain",
            "album": "Serial Experiments",
            "length": 183_000,
            "externalLink": "https://example.com/track/1",
            "visibility": visibility,
        }))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the scrobble is accepted");
    response.json_value()
}

/// `GET`s `path` as an `ActivityPub` client.
async fn fetch_ap(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", "application/activity+json")
        .send(app)
        .await
}

/// A public scrobble by an account with a remote follower queues one
/// `Listen` with the `Audio` embedded, and the fan-out delivers exactly it to
/// the follower's shared inbox.
#[tokio::test]
async fn a_public_scrobble_delivers_a_listen_with_an_embedded_audio() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let wanda = listener(&pool, "wanda").await;
    let token = common::create_app_and_token(&pool, &wanda, "write:scrobbles").await;
    let erin = common::create_remote_user(&pool, "erin", "peer.example").await;
    followed(&pool, &erin, &wanda).await;

    let entity = scrobble(&app, &token, "public").await;
    let id = entity["id"].as_str().expect("the entity id is text");

    let queued = published(&pool).await;
    assert_eq!(queued.len(), 1, "the scrobble is announced once");
    fan_out(&pool).await;
    let sent = deliveries(&pool).await;
    assert_eq!(sent.len(), 1, "one delivery to the one remote host");
    let (inbox, body) = &sent[0];
    assert_eq!(inbox, "https://peer.example/inbox", "the shared inbox");

    assert_eq!(body["type"], "Listen");
    assert_eq!(body["id"], format!("{BASE}/activities/{id}"));
    assert_eq!(body["actor"], wanda.ap_id);
    assert_eq!(body["to"], json!([PUBLIC]));
    assert_eq!(body["cc"], json!([format!("{}/followers", wanda.ap_id)]));
    let audio = &body["object"];
    assert_eq!(audio["type"], "Audio", "the object is embedded");
    assert_eq!(audio["id"], format!("{BASE}/objects/{id}"));
    assert_eq!(audio["title"], "Wired Song");
    assert_eq!(audio["artist"], "The Lain");
    assert_eq!(audio["album"], "Serial Experiments");
    assert_eq!(audio["length"], 183_000);
    assert_eq!(audio["externalLink"], "https://example.com/track/1");
    assert_eq!(audio["actor"], wanda.ap_id);
}

/// A `direct` scrobble addresses nobody: no publish, no delivery.
#[tokio::test]
async fn a_direct_scrobble_queues_nothing_for_followers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let wanda = listener(&pool, "wanda").await;
    let token = common::create_app_and_token(&pool, &wanda, "write:scrobbles").await;
    let erin = common::create_remote_user(&pool, "erin", "peer.example").await;
    followed(&pool, &erin, &wanda).await;

    let _entity = scrobble(&app, &token, "direct").await;
    fan_out(&pool).await;

    assert!(published(&pool).await.is_empty(), "nothing is published");
    assert!(deliveries(&pool).await.is_empty(), "nothing is delivered");
}

/// A `private` scrobble still reaches the followers it is addressed to, with
/// the followers collection in `to` and nothing in `cc`.
#[tokio::test]
async fn a_private_scrobble_goes_to_followers_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let wanda = listener(&pool, "wanda").await;
    let token = common::create_app_and_token(&pool, &wanda, "write:scrobbles").await;
    let erin = common::create_remote_user(&pool, "erin", "peer.example").await;
    followed(&pool, &erin, &wanda).await;

    let _entity = scrobble(&app, &token, "private").await;
    fan_out(&pool).await;

    let sent = deliveries(&pool).await;
    assert_eq!(sent.len(), 1, "the follower hears it");
    let body = &sent[0].1;
    assert_eq!(body["to"], json!([format!("{}/followers", wanda.ap_id)]));
    assert_eq!(body["cc"], json!([]));
}

/// With nobody remote following, the write queues a publish that fans out to
/// no inbox.
#[tokio::test]
async fn local_followers_alone_deliver_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let wanda = listener(&pool, "wanda").await;
    let token = common::create_app_and_token(&pool, &wanda, "write:scrobbles").await;
    let local = listener(&pool, "lain").await;
    followed(&pool, &local, &wanda).await;

    let _entity = scrobble(&app, &token, "public").await;
    fan_out(&pool).await;

    assert!(deliveries(&pool).await.is_empty(), "a timeline is delivery");
}

/// `GET /objects/{id}` of a public scrobble is its `Audio`, and the
/// `Listen` is at `GET /activities/{id}`.
#[tokio::test]
async fn a_public_scrobble_is_served_as_audio_and_listen() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let wanda = listener(&pool, "wanda").await;
    let token = common::create_app_and_token(&pool, &wanda, "write:scrobbles").await;
    let entity = scrobble(&app, &token, "public").await;
    let id = entity["id"].as_str().expect("the entity id is text");

    let object = fetch_ap(&app, &format!("/objects/{id}")).await;
    common::assert_answer(&object, StatusCode::OK, "the audio object");
    let audio = object.json_value();
    assert_eq!(audio["type"], "Audio");
    assert_eq!(audio["id"], format!("{BASE}/objects/{id}"));
    assert_eq!(audio["title"], "Wired Song");
    assert_eq!(audio["externalLink"], "https://example.com/track/1");

    let activity = fetch_ap(&app, &format!("/activities/{id}")).await;
    common::assert_answer(&activity, StatusCode::OK, "the listen activity");
    let listen = activity.json_value();
    assert_eq!(listen["type"], "Listen");
    assert_eq!(listen["id"], format!("{BASE}/activities/{id}"));
    assert_eq!(
        listen["object"], audio,
        "the embedded object is the served one"
    );
}

/// An unlisted scrobble is served too.
#[tokio::test]
async fn an_unlisted_scrobble_is_served() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let wanda = listener(&pool, "wanda").await;
    let token = common::create_app_and_token(&pool, &wanda, "write:scrobbles").await;
    let entity = scrobble(&app, &token, "unlisted").await;
    let id = entity["id"].as_str().expect("the entity id is text");

    let object = fetch_ap(&app, &format!("/objects/{id}")).await;

    common::assert_answer(&object, StatusCode::OK, "the unlisted audio object");
    assert_eq!(
        object.json_value()["to"],
        json!([format!("{}/followers", wanda.ap_id)])
    );
}

/// A private or direct scrobble is a 404 for an anonymous caller, on both
/// routes, and so is an id nothing carries.
#[tokio::test]
async fn a_private_scrobble_is_not_served_to_strangers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let wanda = listener(&pool, "wanda").await;
    let token = common::create_app_and_token(&pool, &wanda, "write:scrobbles").await;

    for visibility in ["private", "direct"] {
        let entity = scrobble(&app, &token, visibility).await;
        let id = entity["id"].as_str().expect("the entity id is text");
        for path in [format!("/objects/{id}"), format!("/activities/{id}")] {
            let response = fetch_ap(&app, &path).await;
            common::assert_answer(
                &response,
                StatusCode::NOT_FOUND,
                &format!("{visibility} {path}"),
            );
        }
    }
    let unknown = fetch_ap(&app, "/objects/01934567-89ab-7cde-f012-3456789abcde").await;
    common::assert_answer(&unknown, StatusCode::NOT_FOUND, "an unknown id");
}

/// A request that does not name an `ActivityPub` media type gets no
/// document, scrobble or not.
#[tokio::test]
async fn a_scrobble_object_needs_an_activitypub_accept() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let wanda = listener(&pool, "wanda").await;
    let token = common::create_app_and_token(&pool, &wanda, "write:scrobbles").await;
    let entity = scrobble(&app, &token, "public").await;
    let id = entity["id"].as_str().expect("the entity id is text");

    let response = common::TestRequest::get(&format!("/objects/{id}"))
        .send(&app)
        .await;

    common::assert_answer(&response, StatusCode::NOT_FOUND, "no ActivityPub Accept");
}
