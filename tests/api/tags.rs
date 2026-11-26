//! Tags over HTTP (`/api/v1/tags/{name}`, its `follow`/`unfollow`,
//! `/api/v1/followed_tags` and the `/api/v1/trends` stubs, WBE-116).
//!
//! The tests pin what the ticket promises. `GET /api/v1/tags/{name}` answers
//! the full Tag: the seven-day `history` newest first, counts as strings, the
//! soft-deleted status uncounted — with `following` only when a viewer is
//! present; an unknown or unusable name is the 404 `{"error":"Hashtag not
//! found"}`. `follow` creates the tag row on demand and is idempotent, a name
//! normalization rejects is the 422 `{"error":"Invalid hashtag"}`, and both
//! writes answer the Tag with `following` set; `unfollow` removes without
//! creating, twice removes once, and an unknown tag is the 404 again.
//! `GET /api/v1/followed_tags` lists the caller's follows newest-first with a
//! `Link` header keyed on the `hashtag_follows.position` ordinal (default
//! page 20, `limit=0` every follow, no header for an empty page), and the
//! four trends routes answer `[]` to anyone. Every gated route runs the
//! `AuthUser` + `read:follows`/`write:follows` pipeline.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use chrono::{DateTime, NaiveTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::{hashtag, hashtag_follow, status, status::Visibility};
use wired_be::pagination::MAX_LIMIT;

use crate::common;

/// The show/follow route tree.
const TAGS: &str = "/api/v1/tags";
/// The followed-tags listing.
const FOLLOWED: &str = "/api/v1/followed_tags";
/// The instance `test_config` serves as: what entity urls and the `Link`
/// header spell their targets with.
const BASE: &str = "https://test.example";
/// 404 body for a name no tag row holds (the ticket's text).
const NOT_FOUND: &str = "Hashtag not found";
/// 422 body for an unusable name on the writes (the ticket's text).
const INVALID: &str = "Invalid hashtag";
/// The longest name the tag storage keeps ([`MAX_HASHTAG_LEN`] in the
/// crate); one character more is an invalid hashtag.
const MAX_NAME: usize = 100;

/// Noon UTC of the day `days_ago` before `anchor`'s date: the fixture
/// status timestamps. Noon (never a boundary) and one `anchor` shared by
/// seeding and asserting mean the day each status is bucketed into is
/// pinned, and the only midnight the test can cross is between taking the
/// anchor and issuing the request — milliseconds apart.
fn noon(anchor: DateTime<Utc>, days_ago: i64) -> DateTime<Utc> {
    anchor
        .date_naive()
        .checked_sub_days(chrono::Days::new(
            u64::try_from(days_ago).expect("the fixture is recent"),
        ))
        .expect("the fixture day exists")
        .and_hms_opt(12, 0, 0)
        .expect("noon")
        .and_utc()
}

/// The unix-seconds text of the UTC midnight of `noon`'s day: the
/// `history` entries' `day` field, computed from the same anchor as the
/// fixtures so both name the same calendar days.
fn midnight(anchor: DateTime<Utc>, days_ago: i64) -> String {
    noon(anchor, days_ago)
        .date_naive()
        .and_time(NaiveTime::MIN)
        .and_utc()
        .timestamp()
        .to_string()
}

/// A local user named `nickname` and a bearer token granting `scopes`.
async fn user_and_token(pool: &PgPool, nickname: &str, scopes: &str) -> String {
    let created = common::create_local_user(pool, nickname).await;
    common::create_app_and_token(pool, &created, scopes).await
}

/// `GET /api/v1/tags/{name}` (a non-empty `suffix` is `POST`ed).
async fn tag_request(
    app: &Router,
    bearer: Option<&str>,
    name: &str,
    suffix: &str,
) -> common::TestResponse {
    let path = format!("{TAGS}/{name}{suffix}");
    let request = if suffix.is_empty() {
        common::TestRequest::get(&path)
    } else {
        common::TestRequest::post(&path)
    };
    match bearer {
        Some(token) => request.bearer(token).send(app).await,
        None => request.send(app).await,
    }
}

/// `GET` of an absolute path as `bearer`.
async fn get(app: &Router, bearer: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path)
        .bearer(bearer)
        .send(app)
        .await
}

/// The names of the tags in a rendered array, in order.
fn tag_names(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .map(|tag| tag["name"].as_str().expect("every tag carries its name"))
        .collect()
}

/// A public status of `#rust`-tagger `nickname`, bucketed onto fixed UTC
/// days: the fixture behind the history numbers — today two live uses by
/// two accounts, yesterday one, the tombstone never. Every timestamp is
/// pinned to [`noon`] of a day derived from one anchor the caller keeps, so
/// the same anchor makes the `midnight` expectations and the read is immune
/// to a UTC midnight landing inside the test body.
///
/// Returns the anchor.
async fn seed_rust_history(pool: &PgPool) -> DateTime<Utc> {
    let anchor = Utc::now();
    let alice = common::create_local_user(pool, "alice").await;
    let bob = common::create_local_user(pool, "bob").await;
    let carol = common::create_local_user(pool, "carol").await;
    let dave = common::create_local_user(pool, "dave").await;
    // One status naming `#rust` twice is still one use of the tag.
    let one = common::create_status(pool, &alice, "#rust twice #rust", Visibility::Public).await;
    let two = common::create_status(pool, &bob, "#RUST", Visibility::Public).await;
    assert!(
        !one.uri.is_empty() && !two.uri.is_empty(),
        "the fixture statuses are stored"
    );
    let aged = common::create_status(pool, &carol, "old #rust", Visibility::Public).await;
    let gone = common::create_status(pool, &dave, "gone #rust", Visibility::Public).await;
    for (id, days_ago) in [(one.id, 0), (two.id, 0), (aged.id, 1), (gone.id, 0)] {
        let shifted = sqlx::query("UPDATE statuses SET inserted_at = $2 WHERE id = $1")
            .bind(id)
            .bind(noon(anchor, days_ago))
            .execute(pool)
            .await
            .expect("the fixture status is pinned to its day");
        assert_eq!(shifted.rows_affected(), 1, "the fixture status moved");
    }
    let tombstone = status::soft_delete(pool, gone.id)
        .await
        .expect("the fixture status soft-deletes")
        .expect("the fixture status exists");
    assert!(tombstone.deleted_at.is_some(), "and it is a tombstone now");
    anchor
}

/// The `history` array of `body`, asserted to be the full seven days, with
/// the `(uses, accounts)` strings pinned per day, newest first. The `day`
/// fields are checked against the same `anchor` the fixtures were pinned
/// to, so seed, handler, and assertion all name the same calendar days.
fn assert_history(body: &Value, anchor: DateTime<Utc>, counts: [(&str, &str); 7]) {
    let history = body["history"].as_array().expect("history is an array");
    assert_eq!(history.len(), 7, "seven UTC days");
    for (offset, entry) in history.iter().enumerate() {
        let day = i64::try_from(offset).expect("seven is in range");
        assert_eq!(
            entry["day"].as_str(),
            Some(midnight(anchor, day)).as_deref(),
            "day {offset} is the UTC midnight {offset} days back, as text"
        );
        let (want_uses, want_accounts) = counts[offset];
        assert_eq!(
            entry["uses"].as_str(),
            Some(want_uses),
            "uses of day {offset}"
        );
        assert_eq!(
            entry["accounts"].as_str(),
            Some(want_accounts),
            "accounts of day {offset}"
        );
    }
}

/// The `history` array of `body`, asserted to be seven consecutive UTC days
/// (newest first, each entry the UTC midnight one day before the last), with
/// the given `(uses, accounts)` strings per slot. Unlike [`assert_history`]
/// this does not pin the absolute day, so it is for tags whose statuses were
/// created at the real `now()` and read milliseconds later: the week shape
/// and counts are checked, the calendar is left to [`assert_history`].
fn assert_week_shape(body: &Value, counts: [(&str, &str); 7]) {
    let history = body["history"].as_array().expect("history is an array");
    assert_eq!(history.len(), 7, "seven UTC days");
    let mut newest: Option<i64> = None;
    for (offset, entry) in history.iter().enumerate() {
        let day: i64 = entry["day"]
            .as_str()
            .unwrap_or_else(|| panic!("day {offset} is text"))
            .parse()
            .expect("day is a unix integer");
        assert_eq!(day % 86_400, 0, "day {offset} is a UTC midnight");
        if let Some(previous) = newest {
            assert_eq!(previous - day, 86_400, "days step back exactly one");
        }
        newest = Some(day);
        let (want_uses, want_accounts) = counts[offset];
        assert_eq!(
            entry["uses"].as_str(),
            Some(want_uses),
            "uses of day {offset}"
        );
        assert_eq!(
            entry["accounts"].as_str(),
            Some(want_accounts),
            "accounts of day {offset}"
        );
    }
}

/// `GET /api/v1/tags/rust` renders the full Tag: seven days of computed
/// counts, newest first, no `following` key for an anonymous request.
#[tokio::test]
async fn show_renders_the_tag_with_a_computed_week() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let anchor = seed_rust_history(&pool).await;
    let app = common::test_app(pool);

    let response = tag_request(&app, None, "rust", "").await;
    common::assert_answer(&response, StatusCode::OK, "an anonymous show");
    let body = response.json_value();
    assert_eq!(body["name"], "rust", "the stored name");
    assert_eq!(body["url"], format!("{BASE}/tags/rust"), "the tag page");
    assert!(
        body.get("following").is_none(),
        "no viewer, no `following` key at all: {}",
        response.text()
    );
    assert_history(
        &body,
        anchor,
        [
            ("2", "2"),
            ("1", "1"),
            ("0", "0"),
            ("0", "0"),
            ("0", "0"),
            ("0", "0"),
            ("0", "0"),
        ],
    );
}

/// The path segment goes through the storage normalization: `#RUST` shows
/// `rust`, and a viewer sees its own follow state in the answer.
#[tokio::test]
async fn show_normalizes_the_name_and_answers_the_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bearer = user_and_token(&pool, "alice", "read write").await;
    let stranger = user_and_token(&pool, "bob", "read").await;
    let app = common::test_app(pool);

    let missing = tag_request(&app, Some(&bearer), "%23RUST", "").await;
    assert_eq!(
        missing.status,
        StatusCode::NOT_FOUND,
        "the tag is unknown yet"
    );

    let followed = tag_request(&app, Some(&bearer), "%23RUST", "/follow").await;
    common::assert_answer(&followed, StatusCode::OK, "follow spells it into being");
    assert_eq!(
        followed.json_value()["name"],
        "RUST",
        "the written casing is the display name"
    );

    let shown = tag_request(&app, Some(&bearer), "%23Rust", "").await;
    common::assert_answer(&shown, StatusCode::OK, "any spelling shows it");
    assert_eq!(shown.json_value()["following"], true, "and she follows it");

    let anonymous = tag_request(&app, None, "RUST", "").await;
    assert_eq!(
        anonymous.json_value()["following"],
        Value::Null,
        "an anonymous show omits the key"
    );

    let not_following = tag_request(&app, Some(&stranger), "rust", "").await;
    assert_eq!(
        not_following.json_value()["following"],
        false,
        "a viewer who does not follow sees `following: false`"
    );
}

/// An unknown name, a segment that normalizes to nothing, and an over-long
/// name are one and the same 404 with Pleroma's wording — not the empty tag
/// Mastodon would build. A presented-but-bogus token is the 401.
#[tokio::test]
async fn show_of_an_unknown_tag_is_a_worded_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);

    let long = "t".repeat(MAX_NAME + 1);
    for name in ["nope", "%23", "%23%23", "+++", long.as_str()] {
        let response = tag_request(&app, None, name, "").await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{name}` is no tag: {}",
            response.text()
        );
        assert_eq!(
            response.json_value(),
            json!({"error": NOT_FOUND}),
            "the ticket's wording for `{name}`"
        );
    }

    let invalid_token = tag_request(&app, Some("not-a-real-token"), "rust", "").await;
    common::assert_answer(
        &invalid_token,
        StatusCode::UNAUTHORIZED,
        "a bogus token is a 401, never a silently anonymous show",
    );
    common::assert_error(
        &invalid_token,
        "The access token is invalid",
        "the named 401",
    );
}

/// `follow` creates the tag row on demand, is idempotent, and answers the
/// full Tag with `following: true`; `unfollow` answers `following: false`,
/// twice removes once, and an unknown tag is the 404 it owes.
#[tokio::test]
async fn following_creates_the_tag_and_unfollowing_does_not() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bearer = user_and_token(&pool, "alice", "read:follows write:follows").await;
    let app = common::test_app(pool.clone());

    let first = tag_request(&app, Some(&bearer), "newtag", "/follow").await;
    common::assert_answer(
        &first,
        StatusCode::OK,
        "follow of an unknown tag creates it",
    );
    let body = first.json_value();
    assert_eq!(body["name"], "newtag");
    assert_eq!(body["url"], format!("{BASE}/tags/newtag"));
    assert_eq!(body["following"], true, "and answers the Tag as followed");
    assert_week_shape(&body, [("0", "0"); 7]);
    assert!(
        hashtag::get_by_name(&pool, "newtag")
            .await
            .unwrap()
            .is_some(),
        "the hashtags row was created"
    );

    let twice = tag_request(&app, Some(&bearer), "newtag", "/follow").await;
    common::assert_answer(&twice, StatusCode::OK, "following twice is idempotent");
    let listed = get(&app, &bearer, FOLLOWED).await;
    assert_eq!(
        tag_names(&listed.json::<Vec<Value>>()),
        ["newtag"],
        "and still exactly one follow"
    );

    let gone = tag_request(&app, Some(&bearer), "newtag", "/unfollow").await;
    common::assert_answer(&gone, StatusCode::OK, "unfollow answers the Tag");
    assert_eq!(gone.json_value()["following"], false, "no longer followed");
    let retwice = tag_request(&app, Some(&bearer), "newtag", "/unfollow").await;
    common::assert_answer(&retwice, StatusCode::OK, "and unfollow is idempotent too");
    let cleared = get(&app, &bearer, FOLLOWED).await;
    assert_eq!(cleared.text(), "[]", "and the follow really went");

    let unknown = tag_request(&app, Some(&bearer), "nevermade", "/unfollow").await;
    assert_eq!(
        unknown.status,
        StatusCode::NOT_FOUND,
        "unfollow creates nothing"
    );
    assert_eq!(unknown.json_value()["error"], NOT_FOUND);
    assert!(
        hashtag::get_by_name(&pool, "nevermade")
            .await
            .unwrap()
            .is_none(),
        "not even the tag row"
    );
}

/// Both writes refuse a name that normalization rejects — ahead of any
/// lookup or creation — with the ticket's 422.
#[tokio::test]
async fn the_writes_refuse_invalid_hashtags() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bearer = user_and_token(&pool, "alice", "read:follows write:follows").await;
    let app = common::test_app(pool.clone());
    let long = "t".repeat(MAX_NAME + 1);

    for name in [
        "%23",
        "%20",
        "%23%20",
        "foo%20bar",
        "a%3Fb",
        "123",
        long.as_str(),
    ] {
        for suffix in ["/follow", "/unfollow"] {
            let refused = tag_request(&app, Some(&bearer), name, suffix).await;
            assert_eq!(
                refused.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "`{name}{suffix}` is the 422"
            );
            assert_eq!(
                refused.json_value(),
                json!({"error": INVALID}),
                "with the ticket's body"
            );
        }
    }
    let untouched = get(&app, &bearer, FOLLOWED).await;
    assert_eq!(untouched.text(), "[]", "and nothing was stored");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM hashtags")
            .fetch_one(&pool)
            .await
            .expect("the tag rows count"),
        0,
        "the refused names created no tag rows either"
    );
}

/// A local user named `nickname` following `names` in order through the
/// model, with a token for the full follow scope pair.
async fn follower(pool: &PgPool, nickname: &str, names: &[&str]) -> String {
    let who = common::create_local_user(pool, nickname).await;
    for name in names {
        let tag = hashtag::find_or_create(pool, name)
            .await
            .expect("the fixture tag creates")
            .expect("the fixture name is usable");
        let new = hashtag_follow::follow(pool, who.id, tag.id)
            .await
            .expect("the fixture follow writes");
        assert_eq!(new, 1, "the fixture follow is new");
    }
    common::create_app_and_token(pool, &who, "read:follows write:follows").await
}

/// The `Link` header value for one `followed_tags` page: both cursors are
/// position ordinals, and the url is the listing's own path on this
/// instance.
fn assert_followed_link(link: &str, next: &str, prev: &str) {
    let expected = format!(
        "<{BASE}{FOLLOWED}?max_id={next}>; rel=\"next\", \
         <{BASE}{FOLLOWED}?min_id={prev}>; rel=\"prev\""
    );
    assert_eq!(
        link, expected,
        "the Link header is the two ordinals and nothing else"
    );
}

/// The `Link` header's cursor of a `rel`, so the page walk can follow the
/// ordinals the header itself named.
fn cursor(link: &str, rel: &str) -> String {
    let marker = format!("rel=\"{rel}\"");
    let entry = link
        .split(", ")
        .find(|entry| entry.contains(&marker))
        .unwrap_or_else(|| panic!("the link header names {rel}: {link}"));
    let url = entry
        .split(['<', '>'])
        .nth(1)
        .unwrap_or_else(|| panic!("a bracketed url in {entry}"));
    url.rsplit_once('=')
        .unwrap_or_else(|| panic!("a cursor in {url}"))
        .1
        .to_owned()
}

/// `GET /api/v1/followed_tags`: the caller's follows newest first, each in
/// the full Tag form with `following: true` and a week of history, the
/// `Link` header keyed on the follow ordinal, `limit=0` every follow, and
/// the empty array (with no header) for a user following nothing.
#[tokio::test]
async fn followed_tags_pages_over_the_follow_order() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let token = follower(&pool, "alice", &["alpha", "beta", "gamma"]).await;
    let other = follower(&pool, "bob", &["alpha"]).await;
    let carol = user_and_token(&pool, "carol", "read:follows").await;
    let app = common::test_app(pool);

    let every = get(&app, &token, &format!("{FOLLOWED}?limit=0")).await;
    common::assert_answer(&every, StatusCode::OK, "limit=0 answers every follow");
    let tags = every.json::<Vec<Value>>();
    assert_eq!(
        tag_names(&tags),
        ["gamma", "beta", "alpha"],
        "newest follow first"
    );
    assert!(
        tags.iter()
            .all(|tag| tag["following"] == Value::Bool(true) && tag["history"].is_array()),
        "every entry is the full Tag, followed"
    );

    let page = get(&app, &token, &format!("{FOLLOWED}?limit=2")).await;
    common::assert_answer(&page, StatusCode::OK, "a page of two reads");
    let link = page
        .header("link")
        .expect("a page that leaves a follow out links to the rest")
        .to_owned();
    let next = cursor(&link, "next");
    let newest = cursor(&link, "prev");
    assert_followed_link(&link, &next, &newest);

    let older = get(&app, &token, &format!("{FOLLOWED}?max_id={next}")).await;
    assert_eq!(
        tag_names(&older.json::<Vec<Value>>()),
        ["alpha"],
        "max_id walks to the older follows"
    );
    let older_link = older
        .header("link")
        .expect("one row is a page, and a page links")
        .to_owned();
    let back_cursor = cursor(&older_link, "prev");
    let back = get(
        &app,
        &token,
        &format!("{FOLLOWED}?limit=2&min_id={back_cursor}"),
    )
    .await;
    assert_eq!(
        tag_names(&back.json::<Vec<Value>>()),
        ["gamma", "beta"],
        "min_id walks back from the older page, still newest-first"
    );
    let forward = get(&app, &token, &format!("{FOLLOWED}?since_id={back_cursor}")).await;
    assert_eq!(
        tag_names(&forward.json::<Vec<Value>>()),
        ["gamma", "beta"],
        "and since_id reads everything newer than the cursor it polls with"
    );

    let alone = get(&app, &other, FOLLOWED).await;
    assert_eq!(
        tag_names(&alone.json::<Vec<Value>>()),
        ["alpha"],
        "and the listing is per user"
    );

    let nothing = get(&app, &carol, FOLLOWED).await;
    common::assert_answer(&nothing, StatusCode::OK, "following nothing reads");
    assert_eq!(nothing.text(), "[]", "the empty array");
    assert_eq!(nothing.header("link"), None, "and no Link header at all");
}

/// A page cursor that is not an ordinal is the pagination 400.
#[tokio::test]
async fn a_cursor_that_is_not_an_ordinal_is_a_bad_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let token = follower(&pool, "alice", &["alpha"]).await;
    let app = common::test_app(pool);

    for key in ["max_id", "min_id", "since_id"] {
        for value in ["nope", "1.5"] {
            let response = get(&app, &token, &format!("{FOLLOWED}?{key}={value}")).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "`{key}={value}` is a 400"
            );
            assert_eq!(
                response.json_value()["error"],
                "Invalid id",
                "the pagination wording for `{key}={value}`"
            );
        }
    }
}

/// The scope pipeline of the three gated routes: no token is the 401, a
/// bogus one the 401 that names it, and an app-only token the 403 that
/// names the missing user. The scope decisions themselves are
/// [`the_scope_checks_gate_the_tag_routes`].
#[tokio::test]
async fn the_token_pipeline_guards_the_tag_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool);

    let anonymous = [
        tag_request(&app, None, "x", "/follow").await,
        tag_request(&app, None, "x", "/unfollow").await,
        common::TestRequest::get(FOLLOWED).send(&app).await,
    ];
    common::assert_all_refused(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "Invalid credentials",
        "no token",
    );

    let bogus = [
        tag_request(&app, Some("not-a-real-token"), "x", "/follow").await,
        tag_request(&app, Some("not-a-real-token"), "x", "/unfollow").await,
        common::TestRequest::get(FOLLOWED)
            .bearer("not-a-real-token")
            .send(&app)
            .await,
    ];
    common::assert_all_refused(
        &bogus,
        StatusCode::UNAUTHORIZED,
        "The access token is invalid",
        "a bogus token",
    );

    let userless = [
        tag_request(&app, Some(&app_only), "x", "/follow").await,
        tag_request(&app, Some(&app_only), "x", "/unfollow").await,
        get(&app, &app_only, FOLLOWED).await,
    ];
    common::assert_all_refused(
        &userless,
        StatusCode::FORBIDDEN,
        "This API requires an authenticated user",
        "an app-only token",
    );
}

/// After the extractors: `write:follows` to follow and `read:follows` to
/// list, with the broad `read`/`write` trees and the `follow` root passing
/// and unrelated sub-scopes refused.
#[tokio::test]
async fn the_scope_checks_gate_the_tag_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let read_only = user_and_token(&pool, "reader", "read:statuses").await;
    let write_only = user_and_token(&pool, "writer", "write:statuses").await;
    let broad_read = user_and_token(&pool, "broad_r", "read").await;
    let broad_write = user_and_token(&pool, "broad_w", "write").await;
    let follows_root = user_and_token(&pool, "rooter", "follow").await;
    let app = common::test_app(pool);

    let writes_refused = [
        tag_request(&app, Some(&read_only), "x", "/follow").await,
        tag_request(&app, Some(&write_only), "x", "/follow").await,
        tag_request(&app, Some(&broad_read), "x", "/unfollow").await,
    ];
    common::assert_all_refused(
        &writes_refused,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: write:follows.",
        "a token without write:follows",
    );

    let read_refused = [
        get(&app, &read_only, FOLLOWED).await,
        get(&app, &write_only, FOLLOWED).await,
    ];
    common::assert_all_refused(
        &read_refused,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: read:follows.",
        "a token without read:follows",
    );

    let granted = [
        tag_request(&app, Some(&broad_write), "x", "/follow").await,
        tag_request(&app, Some(&follows_root), "x", "/unfollow").await,
        get(&app, &broad_read, FOLLOWED).await,
        get(&app, &follows_root, FOLLOWED).await,
    ];
    for response in &granted {
        common::assert_answer(
            response,
            StatusCode::OK,
            "a granted scope reaches the route",
        );
    }
}

/// `show` with a presented token runs Pleroma's scopes plug for the action:
/// the user token must grant `read` — an unrelated sub-scope is the 403,
/// the broad tree reaches the handler (where the unknown tag is the 404),
/// and no token at all proceeds anonymously.
#[tokio::test]
async fn show_gates_a_presented_token_behind_read() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let narrow = user_and_token(&pool, "narrow", "read:statuses").await;
    let reader = user_and_token(&pool, "reader", "read").await;
    let app = common::test_app(pool);

    let refused = tag_request(&app, Some(&narrow), "whatever", "").await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "a read:statuses token showing a tag",
    );
    common::assert_error(
        &refused,
        "Insufficient permissions: read.",
        "the show denial",
    );

    let reached = tag_request(&app, Some(&reader), "whatever", "").await;
    common::assert_answer(
        &reached,
        StatusCode::NOT_FOUND,
        "a `read` token reaches the handler: the tag is merely unknown",
    );
    let anonymous = tag_request(&app, None, "whatever", "").await;
    common::assert_answer(
        &anonymous,
        StatusCode::NOT_FOUND,
        "and so does no token at all",
    );
}

/// The four trends routes answer `[]` — to anyone, without a token, and
/// without a tag row behind the answer.
#[tokio::test]
async fn trends_answer_empty_arrays() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bearer = user_and_token(&pool, "alice", "read").await;
    let app = common::test_app(pool);

    for path in [
        "/api/v1/trends",
        "/api/v1/trends/tags",
        "/api/v1/trends/statuses",
        "/api/v1/trends/links",
    ] {
        let anonymous = common::TestRequest::get(path).send(&app).await;
        common::assert_answer(&anonymous, StatusCode::OK, path);
        assert_eq!(anonymous.text(), "[]", "{path} is empty");
        let authed = get(&app, &bearer, path).await;
        common::assert_answer(&authed, StatusCode::OK, &format!("{path} with a token"));
        assert_eq!(authed.text(), "[]", "still empty");
    }
}

/// The default page is 20 follows and an over-cap request falls to the cap.
#[tokio::test]
async fn the_default_page_is_twenty_follows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let names: Vec<String> = (0..42).map(|index| format!("tag{index}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let token = follower(&pool, "alice", &refs).await;
    let app = common::test_app(pool);

    let cap = usize::try_from(MAX_LIMIT).expect("the cap is a page size");
    assert_eq!(
        get(&app, &token, &format!("{FOLLOWED}?limit=0"))
            .await
            .json::<Vec<Value>>()
            .len(),
        42,
        "limit=0 answers every follow"
    );
    assert_eq!(
        get(&app, &token, FOLLOWED).await.json::<Vec<Value>>().len(),
        20,
        "the default page is 20 of them"
    );
    assert_eq!(
        get(&app, &token, &format!("{FOLLOWED}?limit=100"))
            .await
            .json::<Vec<Value>>()
            .len(),
        cap,
        "and an over-cap request falls to the cap"
    );
}

/// A tag row made by `follow` but never used in a status still shows the
/// zeroed week, and statuses posted afterwards count into the history.
#[tokio::test]
async fn a_followed_tag_counts_uses_as_they_come() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bearer = user_and_token(&pool, "alice", "read write:follows").await;
    let app = common::test_app(pool.clone());

    let empty = tag_request(&app, Some(&bearer), "lonely", "/follow").await;
    common::assert_answer(&empty, StatusCode::OK, "the follow answers");
    assert_week_shape(&empty.json_value(), [("0", "0"); 7]);

    let alice = common::create_local_user(&pool, "postert").await;
    let posted = common::create_status(&pool, &alice, "now #lonely", Visibility::Public).await;
    assert!(
        !posted.uri.is_empty(),
        "the status is stored, tagged `lonely`"
    );
    let shown = tag_request(&app, Some(&bearer), "lonely", "").await;
    assert_week_shape(
        &shown.json_value(),
        [
            ("1", "1"),
            ("0", "0"),
            ("0", "0"),
            ("0", "0"),
            ("0", "0"),
            ("0", "0"),
            ("0", "0"),
        ],
    );

    let listed = get(&app, &bearer, FOLLOWED).await;
    let tags = listed.json::<Vec<Value>>();
    assert_eq!(
        tag_names(&tags),
        ["lonely"],
        "and the listing renders it too"
    );
    assert_eq!(tags[0]["history"][0]["uses"], "1", "with the same week");
}

/// Followed tags stay out of the Home timeline: the ticket pins that this
/// API change has no timeline effect (Pleroma's rule, not Mastodon's). The
/// home route (WBE-100) now answers, so the pin reads through it: a
/// stranger's post carrying a tag the viewer follows — a post the tag
/// timeline would show — stays off the empty home page, which answers only
/// the viewer's own rows.
#[tokio::test]
async fn following_a_tag_does_not_reach_the_home_timeline() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let tag = hashtag::find_or_create(&pool, "quiet")
        .await
        .unwrap()
        .expect("the fixture tag is usable");
    let new = hashtag_follow::follow(&pool, alice.id, tag.id)
        .await
        .expect("the fixture follow writes");
    assert_eq!(new, 1, "the fixture follow is new");
    let carol = common::create_local_user(&pool, "carol").await;
    let tagged = common::create_status(&pool, &carol, "a #quiet post", Visibility::Public).await;
    assert_eq!(tagged.user_id, carol.id, "the stranger's post is seeded");
    let own = common::create_status(&pool, &alice, "alice's own talk", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read").await;
    let app = common::test_app(pool);

    let response = get(&app, &token, "/api/v1/timelines/home").await;
    assert_eq!(response.status, StatusCode::OK, "the home route answers");
    let page = response.json::<Vec<Value>>();
    let ids: Vec<&str> = page
        .iter()
        .filter_map(|status| status["id"].as_str())
        .collect();
    assert_eq!(
        ids,
        [own.id.to_string().as_str()],
        "the followed tag's stranger post never joins the home page, which \
         answers the viewer's own rows only"
    );
}
