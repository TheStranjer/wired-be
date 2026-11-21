//! The shared fixtures of the notification journeys: the route and emoji
//! constants and the client-API helpers every scenario builds its facts with.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::user::User;

use crate::common;

/// The scopes every scenario actor carries: the root `read` and `write`
/// cover every sub-scope the journeys touch (decision 9's scope tree).
pub(super) const SCOPES: &str = "read write";
/// The flat notification listing and its dismissal routes.
pub(super) const V1: &str = "/api/v1/notifications";
/// The grouped notification listing and its `unread_count` companion.
pub(super) const V2: &str = "/api/v2/notifications";
/// Pleroma's mark-seen route.
pub(super) const PLEROMA_READ: &str = "/api/pleroma/notifications/read";
/// The read-marker routes.
pub(super) const MARKERS: &str = "/api/v1/markers";
/// A `GET /api/v1/markers` asking for both timelines, spelled like an
/// array-parameter client.
pub(super) const BOTH_TIMELINES: &str = "?timeline%5B%5D=home&timeline%5B%5D=notifications";
/// The same asking for `notifications` alone.
pub(super) const NOTIFICATIONS_TIMELINE: &str = "?timeline%5B%5D=notifications";
/// The Pleroma notification-settings route.
pub(super) const SETTINGS: &str = "/api/pleroma/notification_settings";
/// The status create route every action seeds from.
pub(super) const STATUSES: &str = "/api/v1/statuses";
/// The account routes: the follow write and `update_credentials`.
pub(super) const ACCOUNTS: &str = "/api/v1/accounts";
/// The Pleroma status route the reaction write lives under.
pub(super) const PLEROMA_STATUSES: &str = "/api/v1/pleroma/statuses";
/// The Mastodon conversation routes.
pub(super) const CONVERSATIONS: &str = "/api/v1/conversations";
/// Pleroma's conversation routes.
pub(super) const PLEROMA_CONVERSATIONS: &str = "/api/v1/pleroma/conversations";
/// The Pleroma chat routes.
pub(super) const CHATS: &str = "/api/v1/pleroma/chats";
/// The paged twin of the chat list.
pub(super) const CHATS_V2: &str = "/api/v2/pleroma/chats";
/// A thumbs-up and its percent-encoded path segment.
pub(super) const THUMB: &str = "👍";
pub(super) const THUMB_SEGMENT: &str = "%F0%9F%91%8D";
/// A party popper and its segment.
pub(super) const PARTY: &str = "🎉";
pub(super) const PARTY_SEGMENT: &str = "%F0%9F%8E%89";
/// A heart and its segment.
pub(super) const HEART: &str = "❤️";
pub(super) const HEART_SEGMENT: &str = "%E2%9D%A4%EF%B8%8F";
/// The shared 404 body of the notification, conversation and chat gates.
pub(super) const NOT_FOUND: &str = "Record not found";
/// A flat-page telling as a `(type, actor id, status id)` triple.
pub(super) type Telling = (String, String, Option<String>);

/// A local user named `nickname` beside a bearer token granting `SCOPES`.
pub(super) async fn user(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, SCOPES).await;
    (created, token)
}

/// The `id` of a rendered entity, as its JSON spells it.
pub(super) fn entity_id(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity carries its id")
        .to_owned()
}

/// The `id` strings of rendered entities, in answer order.
pub(super) fn ids_of(rows: &[Value]) -> Vec<String> {
    rows.iter().map(entity_id).collect()
}

/// The `GET` of `path` as `token`, asserted 200 and answered as JSON.
pub(super) async fn got(app: &Router, token: &str, path: &str) -> Value {
    let response = common::TestRequest::get(path).bearer(token).send(app).await;
    common::assert_answer(&response, StatusCode::OK, &format!("GET {path}"));
    response.json_value()
}

/// The `GET` of `path` as `token`, asserted 200 and answered as a JSON
/// array of entities.
pub(super) async fn got_array(app: &Router, token: &str, path: &str) -> Vec<Value> {
    got(app, token, path)
        .await
        .as_array()
        .expect("the endpoint answers a JSON array")
        .clone()
}

/// The `POST` of the JSON `body` to `path` as `token`, asserted 200 and
/// answered.
pub(super) async fn posted(app: &Router, token: &str, path: &str, body: &Value) -> Value {
    let response = common::TestRequest::post(path)
        .bearer(token)
        .json(body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, &format!("POST {path}"));
    response.json_value()
}

/// The body-less `POST` to `path` as `token`, asserted 200 and answered.
pub(super) async fn acted(app: &Router, token: &str, path: &str) -> Value {
    let response = common::TestRequest::post(path)
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, &format!("POST {path}"));
    response.json_value()
}

/// The `DELETE` of `path` as `token`, asserted 200 and answered.
pub(super) async fn withdrawn(app: &Router, token: &str, path: &str) -> Value {
    let response = common::TestRequest::delete(path)
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, &format!("DELETE {path}"));
    response.json_value()
}

/// A plain public `POST /api/v1/statuses` of `text` as `token`.
pub(super) async fn post(app: &Router, token: &str, text: &str) -> Value {
    posted(app, token, STATUSES, &json!({ "status": text })).await
}

/// A `direct` `POST /api/v1/statuses` of `text` as `token`.
pub(super) async fn direct_post(app: &Router, token: &str, text: &str) -> Value {
    posted(
        app,
        token,
        STATUSES,
        &json!({ "status": text, "visibility": "direct" }),
    )
    .await
}

/// The `POST /api/v1/accounts/{target}/follow`, answered as the
/// relationship entity it wrote.
pub(super) async fn follow(app: &Router, token: &str, target: &str) -> Value {
    acted(app, token, &format!("{ACCOUNTS}/{target}/follow")).await
}

/// The body-less `POST /api/v1/statuses/{id}/{verb}` (`favourite`, `reblog`
/// and their twins), answered as the status entity.
pub(super) async fn status_action(app: &Router, token: &str, id: &str, verb: &str) -> Value {
    acted(app, token, &format!("{STATUSES}/{id}/{verb}")).await
}

/// The `PUT /api/v1/pleroma/statuses/{id}/reactions/{segment}` write,
/// answered as the status entity.
pub(super) async fn react(app: &Router, token: &str, id: &str, segment: &str) -> Value {
    let path = format!("{PLEROMA_STATUSES}/{id}/reactions/{segment}");
    let response = common::TestRequest::put(&path)
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the reaction stores");
    response.json_value()
}

/// Locks `token`'s own account through `update_credentials`, so a later
/// follow of it waits as a request — the HTTP spelling of the ticket's
/// "edit follow into a request".
pub(super) async fn lock_account(app: &Router, token: &str) {
    let response = common::TestRequest::patch(&format!("{ACCOUNTS}/update_credentials"))
        .bearer(token)
        .json(&json!({ "locked": true }))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the self-lock");
    assert_eq!(
        response.json_value()["locked"],
        json!(true),
        "the answer states the flag"
    );
}

/// `PUT /api/pleroma/notification_settings` blocking (or unblocking)
/// strangers, asserted against the module's `{"status":"success"}` answer.
pub(super) async fn gate_strangers(app: &Router, token: &str, blocked: bool) {
    let response = common::TestRequest::put(SETTINGS)
        .bearer(token)
        .json(&json!({ "block_from_strangers": blocked }))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the settings write");
    assert_eq!(
        response.json_value(),
        json!({ "status": "success" }),
        "the settings answer"
    );
}

/// The caller's flat notification page under `query`, as entities.
pub(super) async fn flat(app: &Router, token: &str, query: &str) -> Vec<Value> {
    got_array(app, token, &format!("{V1}{query}")).await
}

/// A page as `(type, actor id, status id)` triples.
pub(super) fn told(rows: &[Value]) -> Vec<Telling> {
    rows.iter()
        .map(|row| {
            (
                row["type"].as_str().expect("a type").to_owned(),
                row["account"]["id"]
                    .as_str()
                    .expect("an actor id")
                    .to_owned(),
                row["status"]["id"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

/// Triples (or ids) sorted for order-insensitive comparison.
pub(super) fn sorted<T: Ord>(mut values: Vec<T>) -> Vec<T> {
    values.sort();
    values
}

/// One expected telling: `kind` by `actor`, about `status` when named.
pub(super) fn telling(kind: &str, actor: &str, status: Option<&str>) -> Telling {
    (kind.to_owned(), actor.to_owned(), status.map(str::to_owned))
}

/// The row of a page carrying `wanted`'s type, for the per-row probes.
pub(super) fn row_of<'a>(rows: &'a [Value], wanted: &str) -> &'a Value {
    rows.iter()
        .find(|row| row["type"].as_str() == Some(wanted))
        .unwrap_or_else(|| panic!("the page has no `{wanted}` row: {rows:?}"))
}

/// The ids are strictly descending — the flat listing's own key
/// (decision 4).
pub(super) fn assert_descending(ids: &[String]) {
    for pair in ids.windows(2) {
        assert_eq!(pair.len(), 2, "a window of two ids is a pair");
        assert!(pair[0] > pair[1], "newest row first, got {ids:?}");
    }
}

/// The grouped listing under `query`, answered raw.
pub(super) async fn grouped(app: &Router, token: &str, query: &str) -> Value {
    got(app, token, &format!("{V2}{query}")).await
}

/// The `notification_groups` of a grouped body, as values.
pub(super) fn groups(body: &Value) -> Vec<Value> {
    body["notification_groups"]
        .as_array()
        .expect("notification_groups is a list")
        .clone()
}

/// The `GET {V2}/unread_count` answer.
pub(super) async fn unread(app: &Router, token: &str) -> i64 {
    got(app, token, &format!("{V2}/unread_count")).await["count"]
        .as_i64()
        .expect("the count is a number")
}

/// The `{}` answer of the caller's `POST {V1}/clear`.
pub(super) async fn clear(app: &Router, token: &str) {
    assert_eq!(
        acted(app, token, &format!("{V1}/clear")).await,
        json!({}),
        "the clear answers the empty object"
    );
}

/// The `{}` answer of the `POST {V1}/{id}/dismiss` for one notification.
pub(super) async fn dismiss(app: &Router, token: &str, id: &str) {
    assert_eq!(
        acted(app, token, &format!("{V1}/{id}/dismiss")).await,
        json!({}),
        "the dismissal answers the empty object"
    );
}

// ─────────────────  Scenario (1): every type, the flat page  ─────────────────

/// The scenario 1 seed: every action that ends in a telling, performed
/// over HTTP by its own actor, and the facts the recipient's flat listing
/// must therefore answer.
#[derive(Debug)]
pub(super) struct Mixed {
    /// Alice's token: the recipient of seven tellings of six types.
    pub(super) alice: String,
    /// Eve's token: the locked account told only of the pending request.
    pub(super) eve: String,
    /// Bob's account id.
    pub(super) bob: String,
    /// Carol's account id.
    pub(super) carol: String,
    /// Dave's account id.
    pub(super) dave: String,
    /// The id of alice's post — favourited twice, boosted, reacted to.
    pub(super) subject: String,
    /// The id of carol's mentioning post.
    pub(super) carol_post: String,
    /// The id of dave's mentioning post.
    pub(super) dave_post: String,
    /// Alice's inbox as sorted tellings.
    pub(super) want: Vec<Telling>,
}

/// Performs every action of scenario 1 through `app`: a follow, two
/// favourites of one post, a boost, a reaction, two mentions from
/// different accounts, alice's own self-like and self-reaction (which
/// must tell nobody), and — after locking eve through the profile edit —
/// a follow of her that waits as a request.
pub(super) async fn mix(app: &Router, pool: &PgPool) -> Mixed {
    let (alice, alice_token) = user(pool, "alice").await;
    let (bob, bob_token) = user(pool, "bob").await;
    let (carol, carol_token) = user(pool, "carol").await;
    let (dave, dave_token) = user(pool, "dave").await;
    let (eve, eve_token) = user(pool, "eve").await;
    let subject = entity_id(&post(app, &alice_token, "tell me everything").await);

    let granted = follow(app, &bob_token, &alice.id.to_string()).await;
    assert_eq!(
        granted["following"],
        json!(true),
        "an unlocked account grants the follow: {granted}"
    );
    let _liked = status_action(app, &bob_token, &subject, "favourite").await;
    let _boosted = status_action(app, &carol_token, &subject, "reblog").await;
    let _reacted = react(app, &bob_token, &subject, THUMB_SEGMENT).await;
    let carol_post = entity_id(&post(app, &carol_token, "hello @alice").await);
    let dave_post = entity_id(&post(app, &dave_token, "@alice, once more").await);
    let _also_liked = status_action(app, &dave_token, &subject, "favourite").await;

    // The self-actions an open inbox must stay silent about.
    let _own_like = status_action(app, &alice_token, &subject, "favourite").await;
    let _own_reaction = react(app, &alice_token, &subject, PARTY_SEGMENT).await;

    // A follow of an account edited into a locked one waits as a request.
    lock_account(app, &eve_token).await;
    let waiting = follow(app, &bob_token, &eve.id.to_string()).await;
    assert_eq!(
        waiting["requested"],
        json!(true),
        "the locked account turns the follow into a request: {waiting}"
    );

    let bob_id = bob.id.to_string();
    let carol_id = carol.id.to_string();
    let dave_id = dave.id.to_string();
    let want = sorted(vec![
        telling("favourite", &bob_id, Some(&subject)),
        telling("favourite", &dave_id, Some(&subject)),
        telling("follow", &bob_id, None),
        telling("mention", &carol_id, Some(&carol_post)),
        telling("mention", &dave_id, Some(&dave_post)),
        telling("pleroma:emoji_reaction", &bob_id, Some(&subject)),
        telling("reblog", &carol_id, Some(&subject)),
    ]);
    Mixed {
        alice: alice_token,
        eve: eve_token,
        bob: bob_id,
        carol: carol_id,
        dave: dave_id,
        subject,
        carol_post,
        dave_post,
        want,
    }
}
