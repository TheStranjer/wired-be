//! Emoji reactions over HTTP (`/api/v1/pleroma/statuses/{id}/reactions`,
//! WBE-94): reacting, withdrawing a reaction, and reading them back.
//!
//! The two writes are Pleroma's `create` and `delete`: a user token with
//! `write:statuses`, the status resolved through the shared visibility gate (a
//! malformed, unknown, deleted or invisible id is the one 404), and the
//! percent-decoded `{emoji}` segment either one unicode emoji grapheme or a
//! `:shortcode:` of a local, enabled custom emoji — anything else is the 400
//! `{"error":"Invalid emoji"}`. Both answer the reacted-to `Status` entity as
//! the caller sees it, whose `pleroma.emoji_reactions` carries the roll-up: one
//! entry per emoji, the whole count, the viewer's own `me`, the custom emoji's
//! image URL and at most five reactor ids. Both writes are idempotent, so a
//! repeat `PUT` is one reaction and a `DELETE` of a reaction that is not there
//! is the unchanged status with 200.
//!
//! The two reads are Pleroma's `index`, with and without the emoji filter:
//! optional auth (`read:statuses` demanded of a *presented* user token), one
//! object per emoji — `{name, count, me, url, accounts}` — ordered by each
//! emoji's first reaction and then by the emoji text, and every reactor as a
//! full `Account` entity. Reactions of accounts the viewer blocked are hidden,
//! and so are those of accounts they muted unless they sent `with_muted`; an
//! emoji left with no visible reactor leaves the array, and a deactivated
//! reactor is dropped from its emoji's list and count, Pleroma's `fetch_users`
//! doing the same.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        block,
        custom_emoji::{self, NewCustomEmoji},
        emoji_reaction::MAX_SUMMARY_REACTORS,
        mute::{self, MuteOptions},
        status::Visibility,
        user::User,
    },
    util::time::parse_time,
};

use crate::common;

/// The path prefix every route in this file lives under.
const ROOT: &str = "/api/v1/pleroma/statuses";
/// The 400 body of an unusable `{emoji}` segment.
const INVALID: &str = "Invalid emoji";
/// The 404 body of the shared status gate.
const NOT_FOUND: &str = "Record not found";
/// The 401 body `AuthUser` sends when no token was presented at all.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for an app-only token on a route that needs a user.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 wording of the write gate.
const NO_WRITE: &str = "Insufficient permissions: write:statuses.";
/// The 403 wording of the read gate, for a presented token without it.
const NO_READ: &str = "Insufficient permissions: read:statuses.";
/// The 401 body for a token that does not resolve.
const BAD_TOKEN: &str = "The access token is invalid";
/// The prefix of the 422 a malformed parameter gets.
const BAD_PARAM: &str = "Invalid parameters:";
/// One unicode emoji, as it is stored and as the API names it.
const THUMB: &str = "👍";
/// A second unicode emoji, for the multi-emoji assertions.
const PARTY: &str = "🎉";
/// A custom emoji's shortcode, in its stored `:shortcode:` spelling.
const CAT: &str = ":cat:";
/// Where [`CAT`]'s image lives.
const CAT_URL: &str = "https://test.example/emoji/cats/cat.png";
/// An id that names no stored status.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// How many accounts react in the test that outruns the summary cap.
const CROWD: usize = MAX_SUMMARY_REACTORS + 2;

/// A local user named `nickname` with a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` who may read and react.
async fn reader_and_writer(pool: &PgPool, nickname: &str) -> (User, String) {
    fixture(pool, nickname, "read:statuses write:statuses").await
}

/// A local user named `nickname` holding only the write half.
async fn writer_only(pool: &PgPool, nickname: &str) -> (User, String) {
    fixture(pool, nickname, "write:statuses").await
}

/// A public post by a fresh local author named `nickname`, as its id.
async fn public_post(pool: &PgPool, nickname: &str) -> Uuid {
    let author = common::create_local_user(pool, nickname).await;
    common::create_status(pool, &author, "a post", Visibility::Public)
        .await
        .id
}

/// A local, enabled custom emoji named `shortcode`.
async fn emoji(pool: &PgPool, shortcode: &str) {
    let stored = custom_emoji::upsert(pool, &NewCustomEmoji::local(shortcode, CAT_URL))
        .await
        .expect("the emoji fixture upserts");
    assert_eq!(
        stored.shortcode, shortcode,
        "the fixture emoji is the one asked for"
    );
}

/// A custom emoji named `shortcode` that moderation has retired.
async fn disabled_emoji(pool: &PgPool, shortcode: &str) {
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
fn route(status: &str, emoji_segment: Option<&str>) -> String {
    let base = format!("{ROOT}/{status}/reactions");
    emoji_segment.map_or_else(|| base.clone(), |wanted| format!("{base}/{wanted}"))
}

/// `text` as a percent-encoded path segment: every non-ASCII byte as its `%XX`
/// spelling, which is how a client sends an emoji in a path.
fn segment(text: &str) -> String {
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
fn escape(byte: u8) -> String {
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F',
    ];
    let high = usize::from(byte >> 4);
    let low = usize::from(byte & 0x0F);
    format!("%{}{}", HEX[high], HEX[low])
}

/// `PUT …/reactions/{emoji}` as `token`; `status` is the id's text, so the miss
/// probes can feed any spelling of it.
async fn put(app: &Router, token: &str, status: &str, emoji_text: &str) -> common::TestResponse {
    common::TestRequest::put(&route(status, Some(&segment(emoji_text))))
        .bearer(token)
        .send(app)
        .await
}

/// `DELETE …/reactions/{emoji}` as `token`.
async fn remove(app: &Router, token: &str, status: &str, emoji_text: &str) -> common::TestResponse {
    common::TestRequest::delete(&route(status, Some(&segment(emoji_text))))
        .bearer(token)
        .send(app)
        .await
}

/// `GET …/reactions[/{emoji}][?{query}]`, anonymously when `token` is `None`.
async fn get(
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
async fn reacted(app: &Router, token: &str, status: &Uuid, emoji_text: &str) -> Value {
    let response = put(app, token, &status.to_string(), emoji_text).await;
    common::assert_answer(&response, StatusCode::OK, "the reaction stores");
    response.json_value()
}

/// A `PUT` a test only needs to have happened: the reaction is stored, and the
/// body it answers is the test's business elsewhere.
async fn react_ok(app: &Router, token: &str, status: &Uuid, emoji_text: &str) {
    let response = put(app, token, &status.to_string(), emoji_text).await;
    common::assert_answer(&response, StatusCode::OK, "the reaction stores");
}

/// A `DELETE` a test only needs to have happened.
async fn withdraw_ok(app: &Router, token: &str, status: &str, emoji_text: &str) {
    let response = remove(app, token, status, emoji_text).await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        "the withdrawal answers the status",
    );
}

/// A reactions read, asserted 200 and answered as JSON.
async fn listed(
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
async fn field(app: &Router, token: Option<&str>, id: &Uuid) -> Value {
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
async fn stored(pool: &PgPool) -> Vec<(String, String, String)> {
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
async fn stamp(pool: &PgPool, status: &Uuid, reactor: &User, emoji_text: &str, when: &str) {
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
fn keys(value: &Value) -> Vec<&str> {
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
fn names(body: &Value) -> Vec<String> {
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
fn summary(name: &str, count: i64, me: bool, url: &Value, ids: &[&Uuid]) -> Value {
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
fn known(who: &User) -> Value {
    json!({
        "id": who.id.to_string(),
        "username": who.nickname,
        "acct": who.nickname,
    })
}

/// The `accounts` of entry `index`, narrowed to what [`known`] states.
fn shown(body: &Value, index: usize) -> Vec<Value> {
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
fn refused(response: &common::TestResponse, status: StatusCode, message: &str, who: &str) {
    common::assert_answer(response, status, who);
    common::assert_error(response, message, who);
}

/// The `body` of a `…/reactions/{emoji}` read as one entry's key set.
fn entry_keys(body: &Value, index: usize) -> Vec<&str> {
    keys(&body[index])
}

#[test]
fn the_emoji_segment_arrives_percent_encoded() {
    assert_eq!(segment(THUMB), "%F0%9F%91%8D", "a client's emoji");
    assert_eq!(segment(CAT), ":cat:", "colons ride unencoded");
    assert_eq!(
        segment("%F0%9F%91%8D"),
        "%25F0%259F%2591%258D",
        "a literal percent doubles, so a route that decodes its segment is \
         decoding what the client sent"
    );
}

#[tokio::test]
async fn a_unicode_reaction_answers_the_status_with_the_reaction_on_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;

    let body = reacted(&app, &token, &status_id, THUMB).await;
    assert_eq!(body["id"], status_id.to_string(), "the status itself");
    assert_eq!(
        body["pleroma"]["emoji_reactions"],
        json!([summary(THUMB, 1, true, &Value::Null, &[&alice.id])]),
        "the caller's own reaction: counted, flagged, and without an image"
    );
    assert_eq!(
        entry_keys(&body["pleroma"]["emoji_reactions"], 0),
        vec!["account_ids", "count", "me", "name", "url"],
        "Pleroma's five keys on the status-field entry"
    );
    assert_eq!(
        stored(&pool).await,
        vec![(
            alice.id.to_string(),
            status_id.to_string(),
            THUMB.to_owned()
        )],
        "one row, the triple the table keys on"
    );
    let ap_id: Option<String> = sqlx::query_scalar("SELECT ap_id FROM emoji_reactions")
        .fetch_optional(&pool)
        .await
        .expect("the activity id reads");
    let minted = ap_id.as_deref().unwrap_or("no id at all");
    assert!(
        minted.starts_with(&format!("{TEST_BASE_URL}/activities/")),
        "decision 6's local activity id is minted with the row, not {minted}"
    );
}

#[tokio::test]
async fn a_custom_reaction_carries_its_shortcode_and_image() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    emoji(&pool, "cat").await;

    let body = reacted(&app, &token, &status_id, CAT).await;
    assert_eq!(
        body["pleroma"]["emoji_reactions"],
        json!([summary(CAT, 1, true, &json!(CAT_URL), &[&alice.id])]),
        "the shortcode is the name and the emoji's image is the url"
    );

    // An unknown shortcode, another instance's emoji, and a retired one are all
    // the same refusal, and none of them stores a row.
    let remote = common::create_remote_user(&pool, "far", "far.example").await;
    let mirrored = custom_emoji::upsert(
        &pool,
        &NewCustomEmoji {
            shortcode: "far_cat".to_owned(),
            domain: remote.domain.clone(),
            image_url: "https://far.example/emoji/far_cat.png".to_owned(),
            ..NewCustomEmoji::default()
        },
    )
    .await
    .expect("the remote emoji upserts");
    assert_eq!(
        mirrored.domain.as_deref(),
        Some("far.example"),
        "the emoji that refuses is another instance's, not a local one"
    );
    disabled_emoji(&pool, "gone").await;

    let status_text = status_id.to_string();
    for unusable in [":dog:", ":far_cat:", ":gone:"] {
        refused(
            &put(&app, &token, &status_text, unusable).await,
            StatusCode::BAD_REQUEST,
            INVALID,
            unusable,
        );
        refused(
            &remove(&app, &token, &status_text, unusable).await,
            StatusCode::BAD_REQUEST,
            INVALID,
            unusable,
        );
    }
    assert_eq!(
        stored(&pool).await,
        vec![(alice.id.to_string(), status_text, CAT.to_owned())],
        "the one accepted reaction is the only row"
    );
}

#[tokio::test]
async fn an_emoji_that_is_not_one_emoji_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    let status_text = status_id.to_string();
    let two_thumbs = segment(&format!("{THUMB}{THUMB}"));
    let thumb_and_letter = format!("{}x", segment(THUMB));

    for junk in [
        "cat",
        ":cat",
        "cat:",
        "::",
        ":a b:",
        ":a-b:",
        "hello",
        &two_thumbs,
        &thumb_and_letter,
    ] {
        refused(
            &put(&app, &token, &status_text, junk).await,
            StatusCode::BAD_REQUEST,
            INVALID,
            junk,
        );
        refused(
            &remove(&app, &token, &status_text, junk).await,
            StatusCode::BAD_REQUEST,
            INVALID,
            junk,
        );
    }
    assert!(stored(&pool).await.is_empty(), "nothing usable was stored");
}

#[tokio::test]
async fn reacting_twice_is_one_reaction_and_a_second_emoji_is_a_second_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;

    let first = reacted(&app, &token, &status_id, THUMB).await;
    let again = reacted(&app, &token, &status_id, THUMB).await;
    assert_eq!(
        first["pleroma"]["emoji_reactions"], again["pleroma"]["emoji_reactions"],
        "a repeat reaction restates the fact instead of adding to it"
    );
    assert_eq!(
        stored(&pool).await,
        vec![(
            alice.id.to_string(),
            status_id.to_string(),
            THUMB.to_owned()
        )],
        "one row for one pair"
    );

    react_ok(&app, &token, &status_id, PARTY).await;
    assert_eq!(
        stored(&pool).await.len(),
        2,
        "a different emoji of the same account is a second reaction"
    );
    assert_eq!(
        field(&app, Some(&token), &status_id).await,
        json!([
            summary(THUMB, 1, true, &Value::Null, &[&alice.id]),
            summary(PARTY, 1, true, &Value::Null, &[&alice.id]),
        ]),
        "and the roll-up names both, each group ordered by its own first \
         reaction"
    );
}

#[tokio::test]
async fn withdrawing_a_reaction_takes_it_and_repeating_changes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    react_ok(&app, &token, &status_id, THUMB).await;
    let status_text = status_id.to_string();

    let after = remove(&app, &token, &status_text, THUMB).await;
    common::assert_answer(&after, StatusCode::OK, "the withdrawal");
    assert_eq!(
        after.json_value()["pleroma"]["emoji_reactions"],
        json!([]),
        "the status answers without the reaction it no longer has"
    );
    assert!(stored(&pool).await.is_empty(), "the row went");

    let repeated = remove(&app, &token, &status_text, THUMB).await;
    common::assert_answer(&repeated, StatusCode::OK, "a repeat withdrawal");
    assert_eq!(
        repeated.json_value()["pleroma"]["emoji_reactions"],
        json!([]),
        "withdrawing what is already gone is the same answer, not an error"
    );
    let stranger = remove(&app, &token, &status_text, PARTY).await;
    common::assert_answer(&stranger, StatusCode::OK, "an emoji never reacted with");
    assert_eq!(
        stranger.json_value()["id"],
        status_id.to_string(),
        "and it still answers the status"
    );
    refused(
        &remove(&app, &token, &status_text, ":never:").await,
        StatusCode::BAD_REQUEST,
        INVALID,
        "a shortcode that names no emoji of ours",
    );

    // The withdrawal is per emoji: another reaction of the same account stays.
    react_ok(&app, &token, &status_id, THUMB).await;
    react_ok(&app, &token, &status_id, PARTY).await;
    withdraw_ok(&app, &token, &status_text, THUMB).await;
    assert_eq!(
        stored(&pool).await,
        vec![(alice.id.to_string(), status_text, PARTY.to_owned())],
        "only the named emoji's row went"
    );
}

#[tokio::test]
async fn the_writes_want_a_user_token_that_writes_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_reader, read_token) = fixture(&pool, "reader", "read:statuses").await;
    let app_token = common::create_app_only_token(&pool, "write:statuses").await;
    let status_id = public_post(&pool, "bob").await;
    let path = route(&status_id.to_string(), Some(&segment(THUMB)));

    for verb in [
        common::TestRequest::put(&path),
        common::TestRequest::delete(&path),
    ] {
        refused(
            &verb.send(&app).await,
            StatusCode::UNAUTHORIZED,
            NO_TOKEN,
            "no token at all",
        );
    }
    for verb in [
        common::TestRequest::put(&path),
        common::TestRequest::delete(&path),
    ] {
        refused(
            &verb.bearer(&app_token).send(&app).await,
            StatusCode::FORBIDDEN,
            USER_REQUIRED,
            "an app-only token",
        );
    }
    for verb in [
        common::TestRequest::put(&path),
        common::TestRequest::delete(&path),
    ] {
        refused(
            &verb.bearer(&read_token).send(&app).await,
            StatusCode::FORBIDDEN,
            NO_WRITE,
            "a user token without write:statuses",
        );
    }
    refused(
        &put(&app, "not-a-token", &status_id.to_string(), THUMB).await,
        StatusCode::UNAUTHORIZED,
        BAD_TOKEN,
        "a token that does not resolve",
    );
    assert!(stored(&pool).await.is_empty(), "no refusal stored anything");
}

#[tokio::test]
async fn a_status_this_route_may_not_answer_is_the_shared_four_zero_four() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reader_and_writer(&pool, "alice").await;
    let author = common::create_local_user(&pool, "secret").await;
    let private = common::create_status(&pool, &author, "hidden", Visibility::Private)
        .await
        .id
        .to_string();
    let direct = common::create_status(&pool, &author, "only me", Visibility::Direct)
        .await
        .id
        .to_string();
    let doomed = public_post(&pool, "doomed").await;
    let deleted = sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
        .bind(doomed)
        .execute(&pool)
        .await
        .expect("the fixture deletes the status")
        .rows_affected();
    assert_eq!(deleted, 1, "the status was live to delete");

    for target in [
        private,
        direct,
        doomed.to_string(),
        UNKNOWN_ID.to_owned(),
        "junk".to_owned(),
    ] {
        refused(
            &put(&app, &token, &target, THUMB).await,
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("reacting to {target}"),
        );
        refused(
            &remove(&app, &token, &target, THUMB).await,
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("withdrawing on {target}"),
        );
        refused(
            &get(&app, Some(&token), &target, None, "").await,
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("reading {target}"),
        );
        refused(
            &get(&app, Some(&token), &target, Some(THUMB), "").await,
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("reading one emoji of {target}"),
        );
    }
    assert!(
        stored(&pool).await.is_empty(),
        "an unreachable status took no reactions"
    );
}

#[tokio::test]
async fn the_reactions_read_lists_every_account_behind_each_emoji() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = reader_and_writer(&pool, "alice").await;
    let (bob, bob_token) = reader_and_writer(&pool, "bob").await;
    let status_id = public_post(&pool, "carol").await;
    emoji(&pool, "cat").await;

    react_ok(&app, &alice_token, &status_id, THUMB).await;
    react_ok(&app, &bob_token, &status_id, THUMB).await;
    react_ok(&app, &bob_token, &status_id, CAT).await;

    let anonymous = listed(&app, None, &status_id, None).await;
    assert_eq!(
        entry_keys(&anonymous, 0),
        vec!["accounts", "count", "me", "name", "url"],
        "the endpoint render names accounts, not ids"
    );
    assert_eq!(
        (
            names(&anonymous),
            shown(&anonymous, 0),
            shown(&anonymous, 1)
        ),
        (
            vec![THUMB.to_owned(), CAT.to_owned()],
            vec![known(&alice), known(&bob)],
            vec![known(&bob)],
        ),
        "each emoji with its whole reactor list, in reaction order"
    );
    assert_eq!(
        (
            anonymous[0]["count"].as_i64(),
            anonymous[1]["count"].as_i64(),
        ),
        (Some(2), Some(1)),
        "counted per emoji"
    );
    assert_eq!(
        (anonymous[0]["url"].clone(), anonymous[0]["me"].as_bool(),),
        (Value::Null, Some(false)),
        "a unicode emoji has no image, and an anonymous viewer is never `me`"
    );
    assert_eq!(
        anonymous[1]["url"],
        json!(CAT_URL),
        "the custom emoji's image rides along"
    );

    let as_alice = listed(&app, Some(&alice_token), &status_id, None).await;
    assert_eq!(
        (as_alice[0]["me"].as_bool(), as_alice[1]["me"].as_bool()),
        (Some(true), Some(false)),
        "alice is `me` on the emoji she reacted with only"
    );
    assert_eq!(
        field(&app, Some(&alice_token), &status_id).await,
        json!([
            summary(THUMB, 2, true, &Value::Null, &[&alice.id, &bob.id]),
            summary(CAT, 1, false, &json!(CAT_URL), &[&bob.id]),
        ]),
        "and the status field of the same viewer says the same thing, by id"
    );
}

#[tokio::test]
async fn the_single_emoji_variant_answers_only_that_emoji() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    react_ok(&app, &token, &status_id, THUMB).await;
    react_ok(&app, &token, &status_id, PARTY).await;

    let one = listed(&app, Some(&token), &status_id, Some(THUMB)).await;
    assert_eq!(
        (
            entry_keys(&one, 0),
            one[0]["name"].as_str(),
            one[0]["count"].as_i64(),
            one[0]["me"].as_bool(),
            one[0]["url"].clone(),
            shown(&one, 0),
        ),
        (
            vec!["accounts", "count", "me", "name", "url"],
            Some(THUMB),
            Some(1),
            Some(true),
            Value::Null,
            vec![known(&alice)],
        ),
        "the filtered read answers that one emoji, in the same shape"
    );
    for absent in [":nobody:", "not-an-emoji", CAT] {
        assert_eq!(
            listed(&app, None, &status_id, Some(absent)).await,
            json!([]),
            "{absent} names no reaction: the segment is a filter on a read, \
             not a validated write, so the answer is the empty array"
        );
    }
    assert_eq!(
        names(&listed(&app, None, &status_id, Some(PARTY)).await),
        vec![PARTY.to_owned()],
        "and the other emoji stays out of the answer"
    );
}

#[tokio::test]
async fn the_order_is_each_emoji_s_first_reaction_then_its_name() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = reader_and_writer(&pool, "alice").await;
    let (bob, bob_token) = reader_and_writer(&pool, "bob").await;
    let status_id = public_post(&pool, "carol").await;
    react_ok(&app, &alice_token, &status_id, PARTY).await;
    react_ok(&app, &bob_token, &status_id, THUMB).await;
    react_ok(&app, &alice_token, &status_id, THUMB).await;

    // alice's party was the first reaction of anything and her thumb the last:
    // a group is ordered by its earliest row, not by its newest.
    stamp(&pool, &status_id, &alice, PARTY, "2024-01-01T00:00:00.000Z").await;
    stamp(&pool, &status_id, &bob, THUMB, "2024-02-01T00:00:00.000Z").await;
    stamp(&pool, &status_id, &alice, THUMB, "2024-06-01T00:00:00.000Z").await;

    let body = listed(&app, None, &status_id, None).await;
    assert_eq!(
        names(&body),
        vec![PARTY.to_owned(), THUMB.to_owned()],
        "january's party leads although its group's newest reaction is June's"
    );
    assert_eq!(
        shown(&body, 1),
        vec![known(&bob), known(&alice)],
        "and inside a group the reactors are in their own reaction order"
    );

    // Two emojis that began at the same instant order by their text.
    let twin = public_post(&pool, "dave").await;
    react_ok(&app, &alice_token, &twin, PARTY).await;
    react_ok(&app, &alice_token, &twin, THUMB).await;
    for wanted in [PARTY, THUMB] {
        stamp(&pool, &twin, &alice, wanted, "2024-03-01T00:00:00.000Z").await;
    }
    let twinned = listed(&app, Some(&alice_token), &twin, None).await;
    assert_eq!(
        names(&twinned),
        vec![PARTY.to_owned(), THUMB.to_owned()],
        "\u{1F389} is the lower text, so the tie breaks towards the party \
         first"
    );
    assert_eq!(
        field(&app, Some(&alice_token), &twin).await[0]["name"],
        json!(PARTY),
        "the status field uses the same order"
    );
}

#[tokio::test]
async fn blocked_reactors_are_hidden_from_the_viewer_who_blocked_them() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = reader_and_writer(&pool, "alice").await;
    let (bob, bob_token) = reader_and_writer(&pool, "bob").await;
    let (carol, carol_token) = reader_and_writer(&pool, "carol").await;
    let status_id = public_post(&pool, "dave").await;
    react_ok(&app, &alice_token, &status_id, THUMB).await;
    react_ok(&app, &bob_token, &status_id, THUMB).await;
    react_ok(&app, &carol_token, &status_id, PARTY).await;

    let refusal = block::block(&pool, TEST_BASE_URL, &carol, &alice)
        .await
        .expect("carol blocks alice");
    assert_eq!(refusal.blocked_id, alice.id, "the edge the read filters on");

    let as_carol = listed(&app, Some(&carol_token), &status_id, None).await;
    assert_eq!(
        (names(&as_carol), shown(&as_carol, 0)),
        (vec![THUMB.to_owned(), PARTY.to_owned()], vec![known(&bob)],),
        "alice drops out of bob's group, and carol's own emoji is untouched"
    );
    assert_eq!(as_carol[0]["count"], json!(1), "counted without her");
    assert_eq!(
        shown(&as_carol, 1),
        vec![known(&carol)],
        "a blocker still sees her own reaction"
    );
    assert_eq!(
        field(&app, Some(&carol_token), &status_id).await[0]["count"],
        json!(2),
        "the batched status field is the unfiltered roll-up: the ticket puts \
         the exclusion on the reactions endpoints, so Pleroma's status-view \
         filter waits for the renderer to preload relationships"
    );

    // An emoji whose only reactor is blocked leaves the array entirely.
    let alone = public_post(&pool, "erin").await;
    react_ok(&app, &alice_token, &alone, THUMB).await;
    assert_eq!(
        names(&listed(&app, Some(&alice_token), &alone, None).await),
        vec![THUMB.to_owned()],
        "alice sees her own reaction on the other post"
    );
    assert_eq!(
        listed(&app, Some(&carol_token), &alone, None).await,
        json!([]),
        "and carol does not see that emoji at all"
    );
    assert_eq!(
        listed(&app, Some(&bob_token), &status_id, None)
            .await
            .as_array()
            .map(Vec::len),
        Some(2),
        "another viewer's block list has nothing to do with this one"
    );
    assert_eq!(
        listed(&app, None, &status_id, None)
            .await
            .as_array()
            .map(Vec::len),
        Some(2),
        "and an anonymous read hides nobody"
    );
}

#[tokio::test]
async fn muted_reactors_need_an_explicit_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = reader_and_writer(&pool, "alice").await;
    let (bob, bob_token) = reader_and_writer(&pool, "bob").await;
    let (carol, carol_token) = reader_and_writer(&pool, "carol").await;
    let status_id = public_post(&pool, "dave").await;
    react_ok(&app, &alice_token, &status_id, THUMB).await;
    react_ok(&app, &bob_token, &status_id, THUMB).await;
    react_ok(&app, &carol_token, &status_id, PARTY).await;

    let silence = mute::mute(&pool, &carol, &bob, MuteOptions::default())
        .await
        .expect("carol mutes bob");
    assert_eq!(silence.muted_id, bob.id, "the edge `with_muted` lifts");

    let plain = listed(&app, Some(&carol_token), &status_id, None).await;
    assert_eq!(
        (names(&plain), shown(&plain, 0)),
        (
            vec![THUMB.to_owned(), PARTY.to_owned()],
            vec![known(&alice)],
        ),
        "by default the muted reactor is out of the thumb group"
    );
    for spelling in ["with_muted=true", "with_muted=1", "with_muted=yes"] {
        let asked = get(
            &app,
            Some(&carol_token),
            &status_id.to_string(),
            None,
            spelling,
        )
        .await;
        common::assert_answer(&asked, StatusCode::OK, spelling);
        assert_eq!(
            shown(&asked.json_value(), 0),
            vec![known(&alice), known(&bob)],
            "{spelling} asks for muted reactors and gets bob back"
        );
    }
    assert_eq!(
        shown(&plain, 0),
        vec![known(&alice)],
        "the flag is per request: the earlier answer stands as it was"
    );
    assert_eq!(
        shown(&listed(&app, Some(&alice_token), &status_id, None).await, 0),
        vec![known(&alice), known(&bob)],
        "and it belongs to the muter's view only"
    );
}

#[tokio::test]
async fn the_status_field_caps_ids_while_the_read_lists_everyone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let status_id = public_post(&pool, "author").await;
    let mut crowd: Vec<(User, String)> = Vec::new();
    for n in 0..CROWD {
        let nickname = format!("reactor{n}");
        let member = reader_and_writer(&pool, &nickname).await;
        react_ok(&app, &member.1, &status_id, THUMB).await;
        crowd.push(member);
    }
    let total = i64::try_from(crowd.len()).expect("the crowd is countable");
    let onlooker = crowd.first().expect("the crowd has a first member");

    let body = listed(&app, Some(&onlooker.1), &status_id, None).await;
    assert_eq!(body[0]["count"], json!(total), "the endpoint counts all");
    assert_eq!(
        shown(&body, 0),
        crowd
            .iter()
            .map(|(who, _)| known(who))
            .collect::<Vec<Value>>(),
        "and lists every one of them as an account, in reaction order"
    );
    assert_eq!(body[0]["me"], json!(true), "the asker is among them");

    let capped = field(&app, None, &status_id).await;
    assert_eq!(
        entry_keys(&capped, 0),
        vec!["account_ids", "count", "me", "name", "url"],
        "the status-field entry is the same five keys with ids instead"
    );
    assert_eq!(capped[0]["count"], json!(total), "the count is the whole");
    assert_eq!(
        capped[0]["account_ids"],
        json!(
            crowd
                .iter()
                .take(MAX_SUMMARY_REACTORS)
                .map(|(who, _)| who.id.to_string())
                .collect::<Vec<String>>()
        ),
        "and the ids are the first {MAX_SUMMARY_REACTORS} reactors"
    );
    assert_eq!(
        capped[0]["me"],
        json!(false),
        "an anonymous field render flags nobody, however many reacted"
    );
}

#[tokio::test]
async fn a_deactivated_reactor_leaves_its_emoji_s_list_and_count() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = reader_and_writer(&pool, "alice").await;
    let (ghost, ghost_token) = reader_and_writer(&pool, "ghost").await;
    let status_id = public_post(&pool, "bob").await;
    react_ok(&app, &alice_token, &status_id, THUMB).await;
    react_ok(&app, &ghost_token, &status_id, THUMB).await;
    react_ok(&app, &ghost_token, &status_id, PARTY).await;

    let gone = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(ghost.id)
        .execute(&pool)
        .await
        .expect("the fixture deactivates the reactor")
        .rows_affected();
    assert_eq!(gone, 1, "the account was there to suspend");

    let body = listed(&app, Some(&alice_token), &status_id, None).await;
    assert_eq!(
        (names(&body), shown(&body, 0)),
        (
            vec![THUMB.to_owned(), PARTY.to_owned()],
            vec![known(&alice)],
        ),
        "the suspended account is out of its groups, Pleroma's `fetch_users` \
         keeping only the active ones"
    );
    assert_eq!(body[0]["count"], json!(1), "so the shared emoji counts one");
    assert_eq!(body[1]["count"], json!(0), "and the ghost's own emoji none");
    assert_eq!(
        body[1]["accounts"],
        json!([]),
        "the emoji is still named, with nothing behind it"
    );
}

#[tokio::test]
async fn the_reads_need_no_token_and_a_presented_one_needs_read_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = reader_and_writer(&pool, "alice").await;
    let (_writer, write_only) = writer_only(&pool, "writer").await;
    let app_token = common::create_app_only_token(&pool, "read:statuses").await;
    let status_id = public_post(&pool, "bob").await;
    react_ok(&app, &alice_token, &status_id, THUMB).await;
    let status_text = status_id.to_string();

    for variant in [None, Some(THUMB)] {
        let anonymous = get(&app, None, &status_text, variant, "").await;
        common::assert_answer(&anonymous, StatusCode::OK, "an anonymous read");
        assert_eq!(
            anonymous.json_value()[0]["me"],
            json!(false),
            "and `me` is false without a viewer"
        );
    }
    assert_eq!(
        listed(&app, Some(&app_token), &status_id, None).await[0]["me"],
        json!(false),
        "an app-only token proceeds unauthenticated"
    );
    for variant in [None, Some(THUMB)] {
        refused(
            &get(&app, Some(&write_only), &status_text, variant, "").await,
            StatusCode::FORBIDDEN,
            NO_READ,
            "a user token without read:statuses",
        );
        refused(
            &get(&app, Some("not-a-token"), &status_text, variant, "").await,
            StatusCode::UNAUTHORIZED,
            BAD_TOKEN,
            "a token that does not resolve",
        );
    }
    assert_eq!(
        listed(&app, Some(&alice_token), &status_id, Some(THUMB)).await[0]["me"],
        json!(true),
        "the reactor's own token is the one that sees `me`"
    );
    assert_eq!(
        field(&app, Some(&alice_token), &status_id).await,
        json!([summary(THUMB, 1, true, &Value::Null, &[&alice.id])]),
        "in the status field as well"
    );
}

#[tokio::test]
async fn a_with_muted_that_is_not_a_boolean_is_a_four_two_two() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;

    for spelling in ["with_muted=maybe", "with_muted=2"] {
        let response = get(&app, Some(&token), &status_id.to_string(), None, spelling).await;
        common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, spelling);
        let message = response.json_value()["error"]
            .as_str()
            .expect("an error body")
            .to_owned();
        assert!(
            message.starts_with(BAD_PARAM),
            "the parameters extractor states what it could not read: {message}"
        );
    }
}

#[tokio::test]
async fn the_field_rides_on_the_batch_render_and_a_nested_original() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let author = common::create_local_user(&pool, "author").await;
    let original = common::create_status(&pool, &author, "original", Visibility::Public)
        .await
        .id;
    let reblogger = common::create_local_user(&pool, "reblogger").await;
    let reblog = common::create_status(&pool, &reblogger, "", Visibility::Public)
        .await
        .id;
    let linked = sqlx::query("UPDATE statuses SET reblog_of_id = $1 WHERE id = $2")
        .bind(original)
        .bind(reblog)
        .execute(&pool)
        .await
        .expect("the fixture reblogs the original")
        .rows_affected();
    assert_eq!(linked, 1, "both rows were there to link");
    react_ok(&app, &token, &original, THUMB).await;

    let response =
        common::TestRequest::get(&format!("/api/v1/statuses?id[]={original}&id[]={reblog}"))
            .bearer(&token)
            .send(&app)
            .await;
    common::assert_answer(&response, StatusCode::OK, "the batch read");
    let body = response.json_value();
    let mine = json!([summary(THUMB, 1, true, &Value::Null, &[&alice.id])]);
    assert_eq!(
        body[0]["pleroma"]["emoji_reactions"], mine,
        "the original's roll-up rides on the batch render"
    );
    assert_eq!(
        body[1]["pleroma"]["emoji_reactions"],
        json!([]),
        "the reblog row itself was reacted to by nobody"
    );
    assert_eq!(
        body[1]["reblog"]["pleroma"]["emoji_reactions"], mine,
        "and the nested original carries its own"
    );
}
