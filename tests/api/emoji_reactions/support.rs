//! The fixtures and request helpers every emoji reaction test shares.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    models::{
        custom_emoji::{self, NewCustomEmoji},
        emoji_reaction::MAX_SUMMARY_REACTORS,
        status::Visibility,
        user::User,
    },
    util::time::parse_time,
};

use crate::common;

/// The path prefix every route in this file lives under.
pub(super) const ROOT: &str = "/api/v1/pleroma/statuses";
/// The 400 body of an unusable `{emoji}` segment.
pub(super) const INVALID: &str = "Invalid emoji";
/// The 404 body of the shared status gate.
pub(super) const NOT_FOUND: &str = "Record not found";
/// The 401 body `AuthUser` sends when no token was presented at all.
pub(super) const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for an app-only token on a route that needs a user.
pub(super) const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 wording of the write gate.
pub(super) const NO_WRITE: &str = "Insufficient permissions: write:statuses.";
/// The 403 wording of the read gate, for a presented token without it.
pub(super) const NO_READ: &str = "Insufficient permissions: read:statuses.";
/// The 401 body for a token that does not resolve.
pub(super) const BAD_TOKEN: &str = "The access token is invalid";
/// The prefix of the 422 a malformed parameter gets.
pub(super) const BAD_PARAM: &str = "Invalid parameters:";
/// One unicode emoji, as it is stored and as the API names it.
pub(super) const THUMB: &str = "👍";
/// A second unicode emoji, for the multi-emoji assertions.
pub(super) const PARTY: &str = "🎉";
/// A custom emoji's shortcode, in its stored `:shortcode:` spelling.
pub(super) const CAT: &str = ":cat:";
/// What the API names [`CAT`]: its bare shortcode, as Pleroma lists a local one.
pub(super) const CAT_NAME: &str = "cat";
/// Where [`CAT`]'s image lives.
pub(super) const CAT_URL: &str = "https://test.example/emoji/cats/cat.png";
/// An id that names no stored status.
pub(super) const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// How many accounts react in the test that outruns the summary cap.
pub(super) const CROWD: usize = MAX_SUMMARY_REACTORS + 2;

/// A local user named `nickname` with a bearer token granting `scopes`.
pub(super) async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` who may read and react.
pub(super) async fn reader_and_writer(pool: &PgPool, nickname: &str) -> (User, String) {
    fixture(pool, nickname, "read:statuses write:statuses").await
}

/// A local user named `nickname` holding only the write half.
pub(super) async fn writer_only(pool: &PgPool, nickname: &str) -> (User, String) {
    fixture(pool, nickname, "write:statuses").await
}

/// A public post by a fresh local author named `nickname`, as its id.
pub(super) async fn public_post(pool: &PgPool, nickname: &str) -> Uuid {
    let author = common::create_local_user(pool, nickname).await;
    common::create_status(pool, &author, "a post", Visibility::Public)
        .await
        .id
}

/// A local, enabled custom emoji named `shortcode`.
pub(super) async fn emoji(pool: &PgPool, shortcode: &str) {
    let stored = custom_emoji::upsert(pool, &NewCustomEmoji::local(shortcode, CAT_URL))
        .await
        .expect("the emoji fixture upserts");
    assert_eq!(
        stored.shortcode, shortcode,
        "the fixture emoji is the one asked for"
    );
}

/// A custom emoji named `shortcode` that moderation has retired.
pub(super) async fn disabled_emoji(pool: &PgPool, shortcode: &str) {
    emoji(pool, shortcode).await;
    let retired = sqlx::query("UPDATE custom_emojis SET disabled = true WHERE shortcode = $1")
        .bind(shortcode)
        .execute(pool)
        .await
        .expect("the fixture disables the emoji")
        .rows_affected();
    assert_eq!(retired, 1, "the emoji row was there to retire");
}

/// The route of one reaction request: the status id as the client wrote it and
/// the optional, already-encoded emoji segment.
pub(super) fn route(status: &str, emoji_segment: Option<&str>) -> String {
    let base = format!("{ROOT}/{status}/reactions");
    emoji_segment.map_or_else(|| base.clone(), |wanted| format!("{base}/{wanted}"))
}

/// `text` as a percent-encoded path segment: every non-ASCII byte as its `%XX`
/// spelling, which is how a client sends an emoji in a path.
pub(super) fn segment(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b':') {
                char::from(byte).to_string()
            } else {
                escape(byte)
            }
        })
        .collect()
}

/// The `%XX` spelling of one byte, upper case like a browser's.
pub(super) fn escape(byte: u8) -> String {
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F',
    ];
    let high = usize::from(byte >> 4);
    let low = usize::from(byte & 0x0F);
    format!("%{}{}", HEX[high], HEX[low])
}

/// `PUT …/reactions/{emoji}` as `token`; `status` is the id's text, so the miss
/// probes can feed any spelling of it.
pub(super) async fn put(
    app: &Router,
    token: &str,
    status: &str,
    emoji_text: &str,
) -> common::TestResponse {
    common::TestRequest::put(&route(status, Some(&segment(emoji_text))))
        .bearer(token)
        .send(app)
        .await
}

/// `DELETE …/reactions/{emoji}` as `token`.
pub(super) async fn remove(
    app: &Router,
    token: &str,
    status: &str,
    emoji_text: &str,
) -> common::TestResponse {
    common::TestRequest::delete(&route(status, Some(&segment(emoji_text))))
        .bearer(token)
        .send(app)
        .await
}

/// `GET …/reactions[/{emoji}][?{query}]`, anonymously when `token` is `None`.
pub(super) async fn get(
    app: &Router,
    token: Option<&str>,
    status: &str,
    emoji_text: Option<&str>,
    query: &str,
) -> common::TestResponse {
    let wanted = emoji_text.map(segment);
    let suffix = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    let request =
        common::TestRequest::get(&format!("{}{suffix}", route(status, wanted.as_deref())));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// A `PUT` a test only needs to have happened, answered as JSON.
pub(super) async fn reacted(app: &Router, token: &str, status: &Uuid, emoji_text: &str) -> Value {
    let response = put(app, token, &status.to_string(), emoji_text).await;
    common::assert_answer(&response, StatusCode::OK, "the reaction stores");
    response.json_value()
}

/// A `PUT` a test only needs to have happened: the reaction is stored, and the
/// body it answers is the test's business elsewhere.
pub(super) async fn react_ok(app: &Router, token: &str, status: &Uuid, emoji_text: &str) {
    let response = put(app, token, &status.to_string(), emoji_text).await;
    common::assert_answer(&response, StatusCode::OK, "the reaction stores");
}

/// A `DELETE` a test only needs to have happened.
pub(super) async fn withdraw_ok(app: &Router, token: &str, status: &str, emoji_text: &str) {
    let response = remove(app, token, status, emoji_text).await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        "the withdrawal answers the status",
    );
}

/// A reactions read, asserted 200 and answered as JSON.
pub(super) async fn listed(
    app: &Router,
    token: Option<&str>,
    status: &Uuid,
    emoji_text: Option<&str>,
) -> Value {
    let response = get(app, token, &status.to_string(), emoji_text, "").await;
    common::assert_answer(&response, StatusCode::OK, "the reactions read");
    response.json_value()
}

/// The `pleroma.emoji_reactions` of `id` as `token` sees it, read through the
/// ordinary status endpoint (`None`: anonymously).
pub(super) async fn field(app: &Router, token: Option<&str>, id: &Uuid) -> Value {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}"));
    let response = match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    };
    common::assert_answer(&response, StatusCode::OK, "the status reads");
    response.json_value()["pleroma"]["emoji_reactions"].clone()
}

/// Every stored reaction as `(user, status, emoji)` text, ordered by row id, so
/// an assertion says what the table holds rather than what a route showed.
pub(super) async fn stored(pool: &PgPool) -> Vec<(String, String, String)> {
    let rows: Vec<(Uuid, Uuid, String)> = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        "SELECT user_id, status_id, emoji FROM emoji_reactions ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("the table reads");
    rows.into_iter()
        .map(|(user_id, status_id, emoji)| (user_id.to_string(), status_id.to_string(), emoji))
        .collect()
}

/// Moves `reactor`'s reaction `emoji_text` on `status` to `when`: how an
/// ordering test says which emoji was reacted with first, whatever the database
/// clock decided between two inserts.
pub(super) async fn stamp(
    pool: &PgPool,
    status: &Uuid,
    reactor: &User,
    emoji_text: &str,
    when: &str,
) {
    let instant = parse_time(when).expect("a fixture time parses");
    let moved = sqlx::query(
        "UPDATE emoji_reactions SET inserted_at = $3
          WHERE status_id = $1 AND user_id = $2 AND emoji = $4",
    )
    .bind(status)
    .bind(reactor.id)
    .bind(instant)
    .bind(emoji_text)
    .execute(pool)
    .await
    .expect("the stamp updates")
    .rows_affected();
    assert_eq!(moved, 1, "the fixture reaction was there to stamp");
}

/// The keys of `value`, sorted, for the exact-shape assertions.
pub(super) fn keys(value: &Value) -> Vec<&str> {
    let mut names: Vec<&str> = value
        .as_object()
        .expect("a reaction entry is an object")
        .keys()
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    names
}

/// The `name`s of a reaction array, in the order it came back.
pub(super) fn names(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the reactions are an array")
        .iter()
        .map(|entry| {
            entry["name"]
                .as_str()
                .expect("every entry names its emoji")
                .to_owned()
        })
        .collect()
}

/// One status-field entry, in the five keys it renders.
pub(super) fn summary(name: &str, count: i64, me: bool, url: &Value, ids: &[&Uuid]) -> Value {
    json!({
        "name": name,
        "count": count,
        "me": me,
        "url": url,
        "account_ids": ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>(),
    })
}

/// The three keys of a reactor's [`Account`] entity these tests read: the id and
/// the two handle spellings (decision 7).
pub(super) fn known(who: &User) -> Value {
    json!({
        "id": who.id.to_string(),
        "username": who.nickname,
        "acct": who.nickname,
    })
}

/// The `accounts` of entry `index`, narrowed to what [`known`] states.
pub(super) fn shown(body: &Value, index: usize) -> Vec<Value> {
    body[index]["accounts"]
        .as_array()
        .expect("an entry carries its accounts")
        .iter()
        .map(|who| {
            json!({
                "id": who["id"],
                "username": who["username"],
                "acct": who["acct"],
            })
        })
        .collect()
}

/// A refusal: `status` with the `{"error": message}` body, for `who`.
pub(super) fn refused(
    response: &common::TestResponse,
    status: StatusCode,
    message: &str,
    who: &str,
) {
    common::assert_answer(response, status, who);
    common::assert_error(response, message, who);
}

/// The `body` of a `…/reactions/{emoji}` read as one entry's key set.
pub(super) fn entry_keys(body: &Value, index: usize) -> Vec<&str> {
    keys(&body[index])
}
