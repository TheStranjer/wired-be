//! Markers over HTTP (`/api/v1/markers`, WBE-97): the read positions one
//! account stores for its `home` and `notifications` timelines.
//!
//! The tests pin what the ticket promises: `POST` answers the `{timeline:
//! marker}` object and `GET` reads the same object back — `last_read_id`,
//! `version`, `updated_at` in the Mastodon time format and Pleroma's
//! `pleroma.unread_count` — for JSON and for `home[last_read_id]=…` form
//! bodies alike; every rewrite adds one to `version` and keeps one row per
//! timeline; `GET` answers only the timelines `timeline` names (as a list or a
//! single value) and only those that have a marker; a position is stored
//! verbatim up to 255 characters; unknown timeline keys are ignored, so an
//! empty body writes nothing; one account never reads or moves another's
//! markers; and both routes enforce their half of the `AuthUser` + scope
//! pipeline (`read:statuses` to look, `write:statuses` to store).

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    models::{
        marker::{self, Marker},
        user::User,
    },
    util::time::{mastodon_time, parse_time},
};

/// The path both endpoints live at.
const ROOT: &str = "/api/v1/markers";
/// `GET` asking for the `home` marker, the way an array-parameter client
/// spells `timeline[]=home`.
const HOME_QUERY: &str = "?timeline%5B%5D=home";
/// The same for `notifications`.
const NOTIFICATIONS_QUERY: &str = "?timeline%5B%5D=notifications";
/// Both timelines at once.
const BOTH_QUERY: &str = "?timeline%5B%5D=home&timeline%5B%5D=notifications";
/// Every JSON key a rendered marker must carry, sorted.
const EXPECTED_KEYS: [&str; 4] = ["last_read_id", "pleroma", "updated_at", "version"];
/// The longest position the endpoint stores.
const MAX_LAST_READ_ID_CHARS: usize = 255;

/// `GET`s the markers route with no credential at all.
async fn read_anonymous(app: &Router) -> common::TestResponse {
    common::TestRequest::get(ROOT).send(app).await
}

/// `POST`s `body` to the markers route with no credential at all.
async fn post_anonymous(app: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post(ROOT).json(body).send(app).await
}

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// `GET /api/v1/markers` with the raw `query` (its leading `?` and all) as
/// `token`.
async fn read(app: &Router, token: &str, query: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("{ROOT}{query}"))
        .bearer(token)
        .send(app)
        .await
}

/// `GET` asserted 200 and parsed as the `{timeline: marker}` object.
async fn read_object(app: &Router, token: &str, query: &str) -> Value {
    let response = read(app, token, query).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a reader's GET answers 200: {}",
        response.text()
    );
    response.json_value()
}

/// `POST /api/v1/markers` with the JSON `body` as `token`.
async fn post_json(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(ROOT)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `POST /api/v1/markers` with the form `fields` as `token`.
async fn post_form(app: &Router, token: &str, fields: &[(&str, &str)]) -> common::TestResponse {
    common::TestRequest::post(ROOT)
        .bearer(token)
        .form(fields)
        .send(app)
        .await
}

/// `POST` asserted 200 and parsed.
async fn posted(app: &Router, token: &str, body: Value) -> Value {
    let response = post_json(app, token, body).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a writer's POST answers 200: {}",
        response.text()
    );
    response.json_value()
}

/// Marks both timelines through the model and returns the rows, ordered by
/// timeline: the fixture the reading tests start from.
async fn seed_both(pool: &PgPool, owner: &User) -> Vec<Marker> {
    marker::upsert_many(
        pool,
        owner.id,
        vec![
            (marker::HOME, "0193home"),
            (marker::NOTIFICATIONS, "0193notes"),
        ],
    )
    .await
    .expect("the fixture markers upsert")
}

/// The account's markers for both timelines, through the model.
async fn stored(pool: &PgPool, owner: &User) -> Vec<Marker> {
    marker::get_many(pool, owner.id, &[marker::HOME, marker::NOTIFICATIONS])
        .await
        .expect("the model reads the markers back")
}

/// The top-level keys of a `{timeline: marker}` object, sorted.
fn timeline_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("markers are a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The keys of `value`'s marker for `timeline`, sorted.
fn marker_keys<'a>(value: &'a Value, timeline: &str) -> Vec<&'a str> {
    let mut keys: Vec<&str> = value[timeline]
        .as_object()
        .unwrap_or_else(|| panic!("`{timeline}` holds a marker object"))
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// An account that has stored nothing answers the empty object, whether or
/// not it names timelines — and the response is JSON either way.
#[tokio::test]
async fn an_account_without_markers_answers_the_empty_object() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:statuses").await;
    let app = common::test_app(pool);

    for query in ["", "?", HOME_QUERY, NOTIFICATIONS_QUERY, BOTH_QUERY] {
        let response = read(&app, &token, query).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "`{query}` on an empty account is a 200"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "`{query}` is answered as JSON"
        );
        assert_eq!(
            response.json_value(),
            json!({}),
            "`{query}` asked for timelines with no markers"
        );
    }
}

/// `POST` answers the marker it wrote and `GET` answers the very same object:
/// the four keys, the position verbatim, version `0` for a first marker, the
/// time in the Mastodon format and Pleroma's zero unread count.
#[tokio::test]
async fn posting_a_position_round_trips_through_the_read() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:statuses write:statuses").await;
    let app = common::test_app(pool);

    let written = posted(&app, &token, json!({"home": {"last_read_id": "01934567"}})).await;
    assert_eq!(timeline_keys(&written), ["home"], "only what was posted");
    assert_eq!(
        marker_keys(&written, "home"),
        EXPECTED_KEYS,
        "exactly the entity keys"
    );
    assert_eq!(
        written["home"]["last_read_id"], "01934567",
        "the position verbatim"
    );
    assert_eq!(written["home"]["version"], 0, "a first marker");
    assert_eq!(
        written["home"]["pleroma"],
        json!({"unread_count": 0}),
        "home has no unread count"
    );
    let stamped = written["home"]["updated_at"]
        .as_str()
        .expect("updated_at is text");
    let when = parse_time(stamped).expect("the stamp is an RFC 3339 time");
    assert_eq!(
        mastodon_time(&when),
        stamped,
        "in exactly the Mastodon spelling of decision 5"
    );
    assert!(when <= Utc::now(), "and not from the future: {stamped}");

    let read_back = read_object(&app, &token, HOME_QUERY).await;
    assert_eq!(read_back, written, "and the read says the same");
}

/// Every rewrite of one timeline adds one to its `version` and replaces its
/// position, in the same row; the other timeline counts on its own.
#[tokio::test]
async fn each_post_bumps_the_version_it_answers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:statuses write:statuses").await;
    let app = common::test_app(pool.clone());

    for (position, version) in [("first", 0), ("second", 1), ("third", 2)] {
        let written = posted(&app, &token, json!({"home": {"last_read_id": position}})).await;
        assert_eq!(
            written["home"]["version"], version,
            "`{position}` is written as version {version}"
        );
        assert_eq!(
            written["home"]["last_read_id"], position,
            "and stored at that position"
        );
    }

    let notes = posted(
        &app,
        &token,
        json!({"notifications": {"last_read_id": "n-1"}}),
    )
    .await;
    assert_eq!(
        notes["notifications"]["version"], 0,
        "the other timeline starts its own count"
    );
    assert_eq!(
        notes["notifications"]["pleroma"],
        json!({"unread_count": 0}),
        "and its count is 0 until the notifications ticket fills it in"
    );

    let both = read_object(&app, &token, BOTH_QUERY).await;
    assert_eq!(
        timeline_keys(&both),
        ["home", "notifications"],
        "two markers are stored now"
    );
    assert_eq!(both["home"]["version"], 2, "home has moved three times");
    assert_eq!(
        both["home"]["last_read_id"], "third",
        "and rests at its last position"
    );
    assert_eq!(both["notifications"]["version"], 0, "notes moved once");

    let rows = stored(&pool, &alice).await;
    assert_eq!(rows.len(), 2, "one row per timeline, not one per post");
}

/// A form body with the `home[last_read_id]=…` spelling writes both
/// timelines, answered in the same object a JSON body would return.
#[tokio::test]
async fn a_form_body_with_nested_keys_writes_both_timelines() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:statuses write:statuses").await;
    let app = common::test_app(pool.clone());

    let written = post_form(
        &app,
        &token,
        &[
            ("home[last_read_id]", "form-home"),
            ("notifications[last_read_id]", "form-notes"),
        ],
    )
    .await;
    assert_eq!(
        written.status,
        StatusCode::OK,
        "the form writes: {}",
        written.text()
    );
    let body = written.json_value();
    assert_eq!(
        timeline_keys(&body),
        ["home", "notifications"],
        "both nested keys became markers"
    );
    assert_eq!(
        body["home"]["last_read_id"], "form-home",
        "at the posted position"
    );
    assert_eq!(
        body["notifications"]["last_read_id"], "form-notes",
        "and the same for notes"
    );
    assert_eq!(
        read_object(&app, &token, BOTH_QUERY).await,
        body,
        "and the read agrees with the form write"
    );

    let ignored = post_form(&app, &token, &[("direct[last_read_id]", "x")]).await;
    assert_eq!(
        ignored.status,
        StatusCode::OK,
        "an unknown key is not an error"
    );
    assert_eq!(
        ignored.json_value(),
        json!({}),
        "and answers nothing at all"
    );

    let rows = stored(&pool, &alice).await;
    assert_eq!(rows.len(), 2, "the stray key wrote no third marker");
    assert_eq!(
        rows.first().expect("home").last_read_id,
        "form-home",
        "and moved nothing that was there"
    );
}

/// `GET` answers only the timelines it is asked for, in either spelling, and
/// leaves out the ones with no marker; asking for nothing answers nothing.
#[tokio::test]
async fn reading_filters_by_the_timeline_parameter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:statuses").await;
    let rows = seed_both(&pool, &alice).await;
    assert_eq!(rows.len(), 2, "the fixture wrote both markers");
    let app = common::test_app(pool);

    let home = read_object(&app, &token, HOME_QUERY).await;
    assert_eq!(timeline_keys(&home), ["home"], "one array key");
    assert_eq!(
        home["home"]["last_read_id"], "0193home",
        "the stored position, and only that timeline"
    );

    let single = read_object(&app, &token, "?timeline=notifications").await;
    assert_eq!(
        timeline_keys(&single),
        ["notifications"],
        "the single-value spelling"
    );
    assert_eq!(
        single["notifications"]["last_read_id"], "0193notes",
        "reads the same marker the list would"
    );

    let both = read_object(&app, &token, BOTH_QUERY).await;
    assert_eq!(
        timeline_keys(&both),
        ["home", "notifications"],
        "both keys, both markers"
    );

    let repeated = read_object(&app, &token, "?timeline=home&timeline=notifications").await;
    assert_eq!(
        timeline_keys(&repeated),
        ["home", "notifications"],
        "and the repeated plain key, which the form parser also makes a list"
    );

    for query in [
        "",
        "?",
        "?timeline=",
        "?timeline=bogus",
        "?timeline%5B%5D=bogus&other=1",
    ] {
        let none = read_object(&app, &token, query).await;
        assert_eq!(
            none,
            json!({}),
            "`{query}` names no timeline whose marker exists"
        );
    }

    let mixed = read_object(&app, &token, "?timeline%5B%5D=bogus&timeline%5B%5D=home").await;
    assert_eq!(
        timeline_keys(&mixed),
        ["home"],
        "an unknown name among known ones is simply not had"
    );
}

/// Keys that name no marker timeline are ignored, and an empty body — in
/// either content type — answers and writes nothing.
#[tokio::test]
async fn unknown_keys_and_empty_bodies_write_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:statuses write:statuses").await;
    let app = common::test_app(pool.clone());

    for body in [
        json!({}),
        json!({"direct": {"last_read_id": "1"}, "dropped": {}}),
        json!({"home": null, "notifications": null}),
    ] {
        let written = posted(&app, &token, body).await;
        assert_eq!(written, json!({}), "nothing posted, nothing answered");
    }
    let empty_form = post_form(&app, &token, &[("other[last_read_id]", "x")]).await;
    assert_eq!(
        empty_form.status,
        StatusCode::OK,
        "an all-unknown form is accepted"
    );
    assert_eq!(empty_form.json_value(), json!({}), "and answers nothing");

    assert!(
        stored(&pool, &alice).await.is_empty(),
        "and the account still has no markers"
    );
    assert_eq!(
        read_object(&app, &token, BOTH_QUERY).await,
        json!({}),
        "which the read reports the same way"
    );
}

/// A position longer than 255 characters is stored cut to 255, and a
/// multi-byte one is cut at a character boundary, never mid-character.
#[tokio::test]
async fn a_position_is_capped_at_two_hundred_fifty_five_characters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:statuses write:statuses").await;
    let app = common::test_app(pool.clone());

    let long = format!("{}tail", "x".repeat(MAX_LAST_READ_ID_CHARS + 40));
    let written = posted(&app, &token, json!({"home": {"last_read_id": long}})).await;
    let ascii = written["home"]["last_read_id"]
        .as_str()
        .expect("the position is text");
    assert_eq!(
        ascii.chars().count(),
        MAX_LAST_READ_ID_CHARS,
        "the first 255 characters are kept"
    );
    assert!(
        long.starts_with(ascii),
        "verbatim up to there, and nothing after: {ascii}"
    );

    let wide = "…".repeat(MAX_LAST_READ_ID_CHARS + 7);
    let multi = posted(
        &app,
        &token,
        json!({"notifications": {"last_read_id": wide}}),
    )
    .await;
    let dots = multi["notifications"]["last_read_id"]
        .as_str()
        .expect("the position is text");
    assert_eq!(
        dots.chars().count(),
        MAX_LAST_READ_ID_CHARS,
        "the cap counts characters"
    );
    assert!(
        dots.len() > MAX_LAST_READ_ID_CHARS,
        "so a multi-byte position is more bytes than that: {}",
        dots.len()
    );

    let read_back = read_object(&app, &token, BOTH_QUERY).await;
    assert_eq!(
        read_back["home"]["last_read_id"], ascii,
        "the read agrees about home"
    );
    assert_eq!(
        read_back["notifications"]["last_read_id"], dots,
        "and about notifications"
    );
    let rows = stored(&pool, &alice).await;
    assert_eq!(
        rows.first().expect("home").last_read_id,
        ascii,
        "and the row holds exactly what the API answered"
    );
}

/// Another account neither reads nor moves a marker: their timelines are empty
/// until they post, and their post is version `0` in its own row.
#[tokio::test]
async fn markers_are_each_account_s_own() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses write:statuses").await;
    let (bob, bob_token) = fixture(&pool, "bob", "read:statuses write:statuses").await;
    let _seeded: Vec<Marker> = seed_both(&pool, &alice).await;
    let app = common::test_app(pool.clone());

    assert_eq!(
        read_object(&app, &bob_token, BOTH_QUERY).await,
        json!({}),
        "bob sees none of alice's markers"
    );

    let posted_by_bob = posted(&app, &bob_token, json!({"home": {"last_read_id": "bob"}})).await;
    assert_eq!(
        posted_by_bob["home"]["version"], 0,
        "his first marker starts at 0"
    );

    let alices = read_object(&app, &alice_token, BOTH_QUERY).await;
    assert_eq!(
        alices["home"]["last_read_id"], "0193home",
        "alice's position is untouched by bob's write"
    );
    assert_eq!(alices["home"]["version"], 0, "and still her first write");

    let hers = stored(&pool, &alice).await;
    let his = stored(&pool, &bob).await;
    assert_eq!(hers.len(), 2, "alice has her two markers");
    assert_eq!(his.len(), 1, "bob has the one he posted");
    assert_ne!(
        hers.first().expect("alice's home").id,
        his.first().expect("bob's home").id,
        "and the same timeline is a row of its own per account"
    );
}

/// A body whose timeline value is not a marker object is the extractor's 422,
/// and it writes nothing.
#[tokio::test]
async fn a_misshapen_marker_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:statuses").await;
    let app = common::test_app(pool.clone());

    for body in [
        json!({"home": "0193abcd"}),
        json!({"notifications": 42}),
        json!({"home": {"last_read_id": ["one", "two"]}}),
    ] {
        let response = post_json(&app, &token, body.clone()).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{body}` is not a marker object"
        );
        assert!(
            response.json_value()["error"]
                .as_str()
                .unwrap_or_default()
                .starts_with("Invalid parameters"),
            "and the error says what was wrong: {}",
            response.text()
        );
    }

    assert!(
        stored(&pool, &alice).await.is_empty(),
        "a rejected body wrote nothing"
    );
}

/// The scope pipeline lets the tokens that carry the route's scope through,
/// in both its narrow and its broad spelling: `read:statuses` (or `read`)
/// reads, `write:statuses` (or `write`) writes, and a write answers the
/// position it just stored.
#[tokio::test]
async fn tokens_with_the_scope_use_both_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, broad) = fixture(&pool, "alice", "read write").await;
    let _seeded: Vec<Marker> = seed_both(&pool, &alice).await;
    let (_reader, read_only) = fixture(&pool, "reader", "read").await;
    let (_status_reader, read_statuses) = fixture(&pool, "status_reader", "read:statuses").await;
    let (_writer, write_broad) = fixture(&pool, "writer", "write").await;
    let (_status_writer, write_statuses) = fixture(&pool, "status_writer", "write:statuses").await;
    let app = common::test_app(pool.clone());

    let granted_reads = [
        read(&app, &broad, HOME_QUERY).await,
        read(&app, &read_only, HOME_QUERY).await,
        read(&app, &read_statuses, HOME_QUERY).await,
    ];
    common::assert_all_answered(&granted_reads, StatusCode::OK, "a read token reads");

    let both = read(&app, &broad, BOTH_QUERY).await;
    assert_eq!(
        timeline_keys(&both.json_value()),
        ["home", "notifications"],
        "and the token of an account with markers is answered its own two"
    );

    let posted = json!({"home": {"last_read_id": "written"}});
    let granted_writes = [
        post_json(&app, &broad, posted.clone()).await,
        post_json(&app, &write_broad, posted.clone()).await,
        post_json(&app, &write_statuses, posted).await,
    ];
    common::assert_all_answered(&granted_writes, StatusCode::OK, "a write token writes");
    for response in &granted_writes {
        assert_eq!(
            response.json_value()["home"]["last_read_id"],
            "written",
            "and answers what it stored"
        );
    }
}

/// A token without the route's scope is the 403 naming it, on either route,
/// and the refused write stores nothing: not for the read-only account, not
/// for the narrow one, and the writer that was only denied a read keeps the
/// marker it posted itself.
#[tokio::test]
async fn tokens_without_the_scope_are_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, broad) = fixture(&pool, "alice", "read write").await;
    let (reader, read_only) = fixture(&pool, "reader", "read").await;
    let (_writer, write_broad) = fixture(&pool, "writer", "write").await;
    let (narrow, wrong_scopes) = fixture(&pool, "narrow", "read:accounts write:accounts").await;
    let app = common::test_app(pool.clone());

    let refused_reads = [
        read(&app, &write_broad, BOTH_QUERY).await,
        read(&app, &wrong_scopes, BOTH_QUERY).await,
    ];
    common::assert_all_refused(
        &refused_reads,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: read:statuses.",
        "a token without `read:statuses`",
    );

    let denied_body = json!({"home": {"last_read_id": "x"}});
    let refused_writes = [
        post_json(&app, &read_only, denied_body.clone()).await,
        post_json(&app, &wrong_scopes, denied_body.clone()).await,
    ];
    common::assert_all_refused(
        &refused_writes,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: write:statuses.",
        "a token without `write:statuses`",
    );

    assert!(
        stored(&pool, &reader).await.is_empty(),
        "a read-only token's rejected write stored nothing"
    );
    assert!(
        stored(&pool, &narrow).await.is_empty(),
        "and neither did the narrow token's, on either route"
    );
    let granted_write = post_json(&app, &broad, denied_body).await;
    common::assert_answer(&granted_write, StatusCode::OK, "the broad token writes");
    let hers = stored(&pool, &alice).await;
    assert_eq!(
        hers.len(),
        1,
        "and a token's own write is kept, as one row per timeline"
    );
    assert_eq!(
        hers.first().expect("home").last_read_id,
        "x",
        "at the position it wrote"
    );
}

/// The credential half of the pipeline: no token at all is the 401 with
/// Pleroma's "Invalid credentials", an unresolvable token the 401 that names
/// the token, and an app-only token the 403 demanding a user — on both routes.
#[tokio::test]
async fn absent_and_app_only_credentials_are_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool);
    let denied_body = json!({"home": {"last_read_id": "x"}});

    let anonymous = [
        read_anonymous(&app).await,
        post_anonymous(&app, &denied_body).await,
    ];
    common::assert_all_refused(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "Invalid credentials",
        "no token at all",
    );

    let bogus = read(&app, "not-a-real-token", HOME_QUERY).await;
    common::assert_answer(&bogus, StatusCode::UNAUTHORIZED, "a token no row carries");
    common::assert_error(
        &bogus,
        "The access token is invalid",
        "the invalid credential",
    );

    let userless = [
        read(&app, &app_only, HOME_QUERY).await,
        post_json(&app, &app_only, denied_body).await,
    ];
    common::assert_all_refused(
        &userless,
        StatusCode::FORBIDDEN,
        "This API requires an authenticated user",
        "an app-only token",
    );
}
