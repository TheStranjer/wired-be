//! The shared fixtures of the inbound-federation journeys: the mock peer's
//! accounts, the client-API readers and the delivery builders.

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{models::user::User, state::AppState};

use crate::{common, remote};

/// The mock's author account, `bob@127.0.0.1:{port}`.
pub(super) const BOB: &str = "bob";
/// A second mock account, used for a stranger's delivery.
pub(super) const DAVE: &str = "dave";
/// The scopes every local caller carries: the status and conversation reads,
/// the notification read, the follow and poll-vote writes.
pub(super) const SCOPES: &str = "read write follow";
/// The `ActivityStreams` public address, as every delivered note names it.
pub(super) const AS_PUBLIC: &str = remote::PUBLIC;
/// The status write.
pub(super) const STATUSES: &str = "/api/v1/statuses";
/// The account routes the scenarios read.
pub(super) const ACCOUNTS: &str = "/api/v1/accounts";
/// The legacy follow write keyed by handle or actor URL.
pub(super) const FOLLOWS: &str = "/api/v1/follows";
/// The Pleroma emoji-reaction reads' route root.
pub(super) const REACTIONS: &str = "/api/v1/pleroma/statuses";
/// The Pleroma chat routes' root.
pub(super) const CHATS: &str = "/api/v1/pleroma/chats";
/// The fixed `published` every delivered note carries: a past instant, so no
/// clock-skew clamp ever moves it and a later `updated` postdates it.
pub(super) const PUBLISHED: &str = "2024-05-01T10:00:00.000Z";

// --- the fixtures ----------------------------------------------------------------------

/// A local account named `nickname` that can sign its own deliveries, with a
/// [`SCOPES`] bearer token.
pub(super) async fn local(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = remote::keyed_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, SCOPES).await;
    (created, token)
}

/// `GET`s `path` as `token` (anonymous when `None`), asserted 200, answered
/// as a JSON array.
pub(super) async fn list(app: &Router, token: Option<&str>, path: &str) -> Vec<Value> {
    let request = common::TestRequest::get(path);
    let response = match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    };
    common::assert_answer(&response, StatusCode::OK, &format!("the read of `{path}`"));
    response
        .json_value()
        .as_array()
        .unwrap_or_else(|| panic!("`{path}` should answer a JSON array"))
        .clone()
}

/// `GET`s `path` as `token` (anonymous when `None`), returned for the caller
/// to assert on.
pub(super) async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `POST /api/v1/statuses` with `body`, asserted 200, answered as the created
/// entity.
pub(super) async fn post_status(app: &Router, token: &str, body: Value) -> Value {
    let response = common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the status write");
    response.json_value()
}

/// `GET /api/v1/accounts/lookup` for `acct`, asserted 200, answered as the
/// `Account` entity.
pub(super) async fn lookup(app: &Router, acct: &str) -> Value {
    get(app, None, &format!("{ACCOUNTS}/lookup?acct={acct}"))
        .await
        .json_value()
}

// --- reading the timelines, context and notifications back ----------------------------

/// `GET /api/v1/timelines/public`, anonymous.
pub(super) async fn public_timeline(app: &Router) -> Vec<Value> {
    list(app, None, "/api/v1/timelines/public").await
}

/// `GET /api/v1/timelines/home` as `token`.
pub(super) async fn home_timeline(app: &Router, token: &str) -> Vec<Value> {
    list(app, Some(token), "/api/v1/timelines/home").await
}

/// `GET /api/v1/timelines/tag/{name}`, anonymous.
pub(super) async fn tag_timeline(app: &Router, name: &str) -> Vec<Value> {
    list(app, None, &format!("/api/v1/timelines/tag/{name}")).await
}

/// `GET /api/v1/accounts/{id}/statuses` as `token` (anonymous when `None`).
pub(super) async fn account_statuses(
    app: &Router,
    token: Option<&str>,
    account_id: &str,
) -> Vec<Value> {
    list(app, token, &format!("{ACCOUNTS}/{account_id}/statuses")).await
}

/// `GET /api/v1/notifications` as `token`.
pub(super) async fn notifications(app: &Router, token: &str) -> Vec<Value> {
    list(app, Some(token), "/api/v1/notifications").await
}

/// `token`'s notifications of `kind`, in delivery order.
pub(super) async fn told_of(app: &Router, token: &str, kind: &str) -> Vec<Value> {
    notifications(app, token)
        .await
        .into_iter()
        .filter(|notice| notice["type"] == json!(kind))
        .collect()
}

/// `token`'s conversation whose `last_status` is the status with uri `uri`,
/// when the inbox carries one.
pub(super) async fn conversation_for(app: &Router, token: &str, uri: &str) -> Option<Value> {
    list(app, Some(token), "/api/v1/conversations")
        .await
        .into_iter()
        .find(|entry| entry["last_status"]["uri"] == json!(uri))
}

/// The `uri` text of an entity.
pub(super) fn uri_of(entity: &Value) -> String {
    entity["uri"]
        .as_str()
        .expect("the entity carries a uri string")
        .to_owned()
}

/// Whether `entries` carry a status with object uri `uri`.
pub(super) fn carries_uri(entries: &[Value], uri: &str) -> bool {
    entries.iter().any(|entry| entry["uri"] == json!(uri))
}

/// The entity in `entries` with object uri `uri`, refusing to be absent.
pub(super) fn entry_for<'a>(entries: &'a [Value], uri: &str) -> &'a Value {
    entries
        .iter()
        .find(|entry| entry["uri"] == json!(uri))
        .unwrap_or_else(|| panic!("a listing should carry {uri}"))
}

// --- building and delivering the peer's activities -------------------------------------

/// Merges `extra`'s fields over `target`, dropping what they displace.
pub(super) fn merge(target: &mut Value, extra: Value) {
    let (Some(map), Value::Object(additions)) = (target.as_object_mut(), extra) else {
        panic!("both documents are JSON objects");
    };
    for (key, value) in additions {
        let _previous = map.insert(key, value);
    }
}

/// A `Note`-family object on the peer's origin named `name`, authored by
/// `actor`, whose `content` is `content`, public by default; every field of
/// `extra` — the `to`/`cc` addressing, `tag`, `attachment`, `inReplyTo`,
/// `summary`, `sensitive`, `updated`, or a `Question`'s `oneOf` — merges over
/// those defaults.
pub(super) fn note_object(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    content: &str,
    extra: Value,
) -> Value {
    let mut object = json!({
        "id": peer.note(name),
        "type": "Note",
        "attributedTo": actor,
        "to": [AS_PUBLIC],
        "cc": [],
        "content": content,
        "published": PUBLISHED,
    });
    merge(&mut object, extra);
    object
}

/// A signed-`POST`-ready activity of `kind` by the peer's actor `actor`,
/// named `name` on the peer's host, whose `object` is `object` (decision 6's
/// ids are what the dispatcher's containment asks of a peer's own activity).
pub(super) fn activity(
    peer: &remote::MockPeer,
    kind: &str,
    name: &str,
    actor: &str,
    object: Value,
) -> Value {
    let mut envelope = json!({
        "id": peer.activity_id(name),
        "type": kind,
        "actor": actor,
        "object": Value::Null,
    });
    envelope["object"] = object;
    envelope
}

/// A `Create` of a `Note`-family object on the peer named `name`: the same
/// builder [`note_object`] and [`activity`] compose, in the call the visibility
/// and content scenarios deliver.
pub(super) fn create_note(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    content: &str,
    extra: Value,
) -> Value {
    let object = note_object(peer, actor, name, content, extra);
    activity(peer, "Create", name, actor, object)
}

/// Delivers `activity` to this instance's shared inbox as the peer's actor
/// `nick`, asserts the `202 Accepted` the inbox answers, and drains the fold
/// plus everything it queued.
pub(super) async fn deliver(
    app: &Router,
    state: &AppState,
    peer: &remote::MockPeer,
    nick: &str,
    activity: &Value,
) {
    let kind = activity["type"].as_str().unwrap_or("activity");
    let response = peer
        .deliver(app, &peer.actor(nick), remote::INBOX, activity)
        .await;
    assert_eq!(
        response.status,
        StatusCode::ACCEPTED,
        "the inbox folds the mock's {kind}"
    );
    let _drained = common::run_all_jobs(state).await;
}

/// Makes the local `alice_token` an accepted follower of the mock's `bob`: the
/// follow resolves and delivers over the wire, and bob's own `Accept` back
/// through the inbox grants it.
pub(super) async fn follow_bob(
    app: &Router,
    state: &AppState,
    peer: &remote::MockPeer,
    alice_token: &str,
    accept_name: &str,
) {
    let actor = peer.actor(BOB);
    let _target = common::TestRequest::post(FOLLOWS)
        .bearer(alice_token)
        .json(&json!({"uri": actor}))
        .send(app)
        .await;
    let _sent = common::run_all_jobs(state).await;
    let follows = peer.delivered("Follow");
    let follow_id = follows
        .last()
        .expect("alice's Follow reached the peer")
        .activity()["id"]
        .as_str()
        .expect("the Follow carries its id")
        .to_owned();
    let accept = activity(peer, "Accept", accept_name, &actor, json!(follow_id));
    deliver(app, state, peer, BOB, &accept).await;
}
