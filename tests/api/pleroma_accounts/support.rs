//! The shared fixtures of the Pleroma account route tests: the expected
//! error bodies and the helpers that seed follows, likes and requests.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::cmp::Reverse;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        favourite,
        follow::{self, Follow, FollowState},
        status::{Status, Visibility},
        user::{Flag, User},
    },
};

use crate::common;

/// The instance origin the fixtures live on (`Config::for_tests`).
pub(super) const BASE: &str = "https://test.example";
/// The outgoing listing's path, as the `Link` header links back to it.
pub(super) const OUTGOING: &str = "/api/v1/pleroma/outgoing_follow_requests";
/// The body every miss on the subscription routes carries (decision 8).
pub(super) const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 401 body `AuthUser` renders when no token was presented at all.
pub(super) const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
pub(super) const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
pub(super) const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body a subscription token without either accepted scope gets;
/// `require_any` joins the alternatives with the `|` operator, in the order
/// Pleroma's plug lists them.
pub(super) const NO_WRITE_SCOPE: &str =
    r#"{"error":"Insufficient permissions: follow | write:follows."}"#;
/// The 403 body a listing token without `read:follows` gets.
pub(super) const NO_READ_SCOPE: &str = r#"{"error":"Insufficient permissions: read:follows."}"#;
/// The 422 body both subscription routes answer without an accepted follow.
pub(super) const NOT_FOLLOWING: &str =
    r#"{"error":"Could not subscribe: You are not following this user"}"#;
/// The 403 body the favourites route answers when the target hides their
/// favourites from the viewer — Pleroma's exact (American-spelled) wording.
pub(super) const HIDDEN_FAVOURITES: &str = r#"{"error":"Can't get favorites"}"#;
/// The 403 body a presented token without `read:favourites` gets from the
/// optional gate.
pub(super) const NO_FAV_SCOPE: &str = r#"{"error":"Insufficient permissions: read:favourites."}"#;
/// The 400 body of a malformed pagination cursor.
pub(super) const BAD_ID: &str = r#"{"error":"Invalid id"}"#;

/// One fixture request: the requested account's id as the entity shows it,
/// next to the id of the `follows` row the page keys on.
pub(super) type Ask = (String, Uuid);

/// The subscribe route for `target`.
pub(super) fn subscribe_path(target: &Uuid) -> String {
    format!("/api/v1/pleroma/accounts/{target}/subscribe")
}

/// The unsubscribe route for `target`.
pub(super) fn unsubscribe_path(target: &Uuid) -> String {
    format!("/api/v1/pleroma/accounts/{target}/unsubscribe")
}

/// The subscription route segment `unsubscribe` picks — also the word the
/// assertions name the write by.
#[must_use]
pub(super) fn subscription_verb(unsubscribe: bool) -> &'static str {
    if unsubscribe {
        "unsubscribe"
    } else {
        "subscribe"
    }
}

/// `POST` one subscription route as `token` (the two actions differ only by
/// their path segment, so both run through here).
pub(super) async fn subscription(
    app: &Router,
    token: &str,
    target: &Uuid,
    unsubscribe: bool,
) -> common::TestResponse {
    let path = if unsubscribe {
        unsubscribe_path(target)
    } else {
        subscribe_path(target)
    };
    common::TestRequest::post(&path)
        .bearer(token)
        .send(app)
        .await
}

/// [`subscription`] with the account segment left exactly as `target` spells
/// it (the miss tests feed garbage there).
pub(super) async fn raw_subscription(
    app: &Router,
    token: &str,
    target: &str,
    action: &str,
) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/pleroma/accounts/{target}/{action}"))
        .bearer(token)
        .send(app)
        .await
}

/// `GET /api/v1/pleroma/outgoing_follow_requests{query}` as `token`
/// (`None` anonymous).
pub(super) async fn outgoing(
    app: &Router,
    token: Option<&str>,
    query: &str,
) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("{OUTGOING}{query}"));
    let request = match token {
        Some(found) => request.bearer(found),
        None => request,
    };
    request.send(app).await
}

/// `GET /api/v1/accounts/relationships?id[]={target}` as `token`: the read
/// that proves which column the writes touched.
pub(super) async fn relationship_batch(app: &Router, token: &str, target: &Uuid) -> Value {
    let response =
        common::TestRequest::get(&format!("/api/v1/accounts/relationships?id%5B%5D={target}"))
            .bearer(token)
            .send(app)
            .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the batch read answers the caller's relationship"
    );
    response
        .json_value()
        .as_array()
        .expect("the batch is an array")
        .first()
        .expect("one target was requested")
        .clone()
}

/// Inserts a local [`User`] named `nickname` whose account is locked, so
/// every follow of it waits for an answer.
pub(super) async fn locked_user(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user_with(pool, nickname, |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await
}

/// Inserts a local [`User`] named `nickname` that is deactivated: the id
/// every account route treats as absent.
pub(super) async fn deactivated_user(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user_with(pool, nickname, |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await
}

/// Records `follower`'s follow of the unlocked `target` and asserts it was
/// granted — the accepted relationship the subscription routes require.
pub(super) async fn following(pool: &PgPool, follower: &User, target: &User) -> Follow {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        FollowState::Accepted,
        "an open local account answers on the spot"
    );
    row
}

/// Records `follower`'s follow of the locked `target` and asserts it waits —
/// a request, which is not a subscription.
pub(super) async fn asking(pool: &PgPool, follower: &User, target: &User) -> Follow {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture request records");
    assert_eq!(
        row.state,
        FollowState::Pending,
        "a locked account answers its own requests"
    );
    row
}

/// The stored relationship of the pair, for reading the toggle back.
pub(super) async fn stored(pool: &PgPool, follower: &User, target: &User) -> Follow {
    follow::get(pool, follower.id, target.id)
        .await
        .unwrap()
        .expect("the fixture relationship exists")
}

/// The pair of `user` behind the request row `row`.
pub(super) fn ask(user: &User, row: &Follow) -> Ask {
    (user.id.to_string(), row.id)
}

/// `asks` sorted newest follow row first — the order the listing answers in.
/// `UUIDv7` is only millisecond precise, so the expectation is sorted from the
/// stored ids, never from the order the fixtures were written in (decision 4).
pub(super) fn sorted_newest_first(asks: &[Ask]) -> Vec<Ask> {
    let mut sorted = asks.to_vec();
    sorted.sort_by_key(|(_, follow_id)| Reverse(*follow_id));
    sorted
}

/// The requested-account ids of `asks`, newest first.
pub(super) fn wanted_targets(asks: &[Ask]) -> Vec<String> {
    sorted_newest_first(asks)
        .into_iter()
        .map(|(user, _)| user)
        .collect()
}

/// The `follows` row ids of `asks`, newest first — the cursors the `Link`
/// header and the walking requests must carry.
pub(super) fn wanted_follows(asks: &[Ask]) -> Vec<Uuid> {
    sorted_newest_first(asks)
        .into_iter()
        .map(|(_, follow_id)| follow_id)
        .collect()
}

/// The entity of account `id` in a listing body, however deep in the page the
/// id's millisecond landed it.
pub(super) fn listed_entry<'a>(body: &'a Value, id: &Uuid) -> &'a Value {
    body.as_array()
        .expect("the listing is an array")
        .iter()
        .find(|entry| entry["id"] == id.to_string())
        .unwrap_or_else(|| panic!("account {id} is on the page"))
}

/// The full relationship JSON for `target` with every flag at the documented
/// default, overwritten by `computed` — the expectation shape the relationship
/// tests established.
pub(super) fn expected_relationship(target: &Uuid, computed: &Value) -> Value {
    let mut value = json!({
        "id": target.to_string(),
        "following": false,
        "requested": false,
        "endorsed": false,
        "followed_by": false,
        "blocking": false,
        "blocked_by": false,
        "muting": false,
        "muting_notifications": false,
        "notifying": false,
        "showing_reblogs": true,
        "domain_blocking": false,
        "note": "",
        "subscribing": false,
    });
    let base = value.as_object_mut().expect("the default is an object");
    let overrides = computed.as_object().expect("the flags are a JSON object");
    for (key, flag) in overrides {
        let replaced = base.insert(key.clone(), flag.clone());
        assert!(
            replaced.is_some(),
            "`{key}` overrides a documented default, not a typo"
        );
    }
    value
}

/// The favourites route for account `{target}` — spelled exactly as handed,
/// so the miss tests can feed garbage in the id segment.
pub(super) fn favourites_path(target: &str) -> String {
    format!("/api/v1/pleroma/accounts/{target}/favourites")
}

/// `GET …/favourites{query}` as `token` (`None`: an anonymous request).
pub(super) async fn favourites(
    app: &Router,
    token: Option<&str>,
    target: &str,
    query: &str,
) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("{}{query}", favourites_path(target)));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// A local account that shares its favourites — `hide_favorites` off, the
/// one difference from the default — with a token granting `scopes`.
pub(super) async fn open_account(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user_with(pool, nickname, |mut new| {
        new.hide_favorites = Flag::new(false);
        new
    })
    .await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// A fresh local account named `nickname` with a bearer token granting
/// `scopes`, for the viewers the favourites tests read the lists through.
pub(super) async fn viewer(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// A `visibility` post by `author`.
pub(super) async fn post(
    pool: &PgPool,
    author: &User,
    visibility: Visibility,
    text: &str,
) -> Status {
    common::create_status(pool, author, text, visibility).await
}

/// A public post by a fresh local author named `nickname`.
pub(super) async fn public_post_by(pool: &PgPool, nickname: &str) -> Status {
    let author = common::create_local_user(pool, nickname).await;
    post(pool, &author, Visibility::Public, "a public post").await
}

/// `who` favourites `status_id`, straight through the model. The write
/// answers nothing: the like rows are read back newest-first through
/// [`liked_pairs`], so no expectation depends on the write order.
pub(super) async fn like(pool: &PgPool, who: &User, status_id: Uuid) {
    let created = favourite::favourite(pool, TEST_BASE_URL, who.id, status_id)
        .await
        .expect("the fixture favourite stores");
    assert!(created.is_some(), "the fixture pair had not favourited yet");
}

/// `who`'s likes as `(like id, status id)` pairs, newest like first — the
/// order the list answers in, read back from the table rather than from the
/// order the fixtures were written in (decision 4).
pub(super) async fn liked_pairs(pool: &PgPool, who: &User) -> Vec<(Uuid, Uuid)> {
    sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT f.id, f.status_id FROM favourites f \
          WHERE f.user_id = $1 ORDER BY f.id DESC",
    )
    .bind(who.id)
    .fetch_all(pool)
    .await
    .expect("the like rows read")
}

/// The ids of every entity of a `Status` array answer, in response order.
pub(super) fn listed_status_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the listing is an array")
        .iter()
        .map(|status| status["id"].as_str().expect("a status id").to_owned())
        .collect()
}

/// The status ids of `pairs`, newest first, keeping only those whose status
/// is one of `visible` — the expectation a narrowed favourites page must
/// answer.
pub(super) fn wanted_status_ids(pairs: &[(Uuid, Uuid)], visible: &[Uuid]) -> Vec<String> {
    pairs
        .iter()
        .filter(|(_, status_id)| visible.contains(status_id))
        .map(|(_, status_id)| status_id.to_string())
        .collect()
}

/// The `Link` header of a favourites page over the route path `path`,
/// keyed on like row ids: `next` walks to the oldest visible like, `prev`
/// back to the newest.
pub(super) fn expected_like_link(path: &str, newest: &Uuid, oldest: &Uuid) -> String {
    format!(
        "<{BASE}{path}?max_id={oldest}>; rel=\"next\", \
         <{BASE}{path}?min_id={newest}>; rel=\"prev\""
    )
}
