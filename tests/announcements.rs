//! Announcements over HTTP (`/api/v1/announcements`, WBE-117): the active
//! list and the dismissal route.
//!
//! The tests pin what the ticket promises: only notices whose activity window
//! holds this instant are listed, newest first; `read` reports the caller's own
//! dismissal and `with_dismissed` decides whether dismissed notices appear at
//! all; a dismissal is one account's and changes nobody else's list; the entity
//! carries exactly the Mastodon keys plus Pleroma's `pleroma.raw_content`; and
//! both routes enforce their half of the `AuthUser` + scope pipeline
//! (`read:accounts` to look, `write:accounts` to dismiss), answering 404 for an
//! id that names no notice.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    models::{
        announcement::{self, Announcement, NewAnnouncement},
        user::User,
    },
    util::time::{mastodon_time, parse_time},
};

/// Every JSON key one listed announcement must carry, sorted.
const EXPECTED_KEYS: [&str; 14] = [
    "all_day",
    "content",
    "emojis",
    "ends_at",
    "id",
    "mentions",
    "pleroma",
    "published_at",
    "reactions",
    "read",
    "starts_at",
    "statuses",
    "tags",
    "updated_at",
];
/// A window bound far enough in the past to be open already.
const PAST: &str = "2000-06-01T12:00:00.250Z";
/// A window bound far enough in the future to be closed only much later.
const FUTURE: &str = "2100-06-01T12:00:00.250Z";
/// The root path both endpoints live under.
const ROOT: &str = "/api/v1/announcements";

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// An instant from its ISO 8601 text.
fn at(text: &str) -> DateTime<Utc> {
    parse_time(text).expect("the fixture times are timestamps")
}

/// Creates a notice from `new` and returns its row.
async fn create(pool: &PgPool, new: &NewAnnouncement) -> Announcement {
    announcement::create(pool, new)
        .await
        .expect("fixture notice creates")
}

/// Creates a notice whose two text columns are both `text` — the model
/// sanitizes the content, which leaves plain text alone — with an open window.
async fn seed(pool: &PgPool, text: &str) -> Announcement {
    create(pool, &NewAnnouncement::from(text)).await
}

/// Creates a notice active only in the window `starts_at`/`ends_at`, leaving
/// the other bound open.
async fn seed_window(
    pool: &PgPool,
    text: &str,
    starts_at: Option<DateTime<Utc>>,
    ends_at: Option<DateTime<Utc>>,
) -> Announcement {
    create(
        pool,
        &NewAnnouncement {
            starts_at,
            ends_at,
            ..NewAnnouncement::from(text)
        },
    )
    .await
}

/// `GET /api/v1/announcements{query}` as `token`.
async fn list(app: &Router, token: &str, query: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("{ROOT}{query}"))
        .bearer(token)
        .send(app)
        .await
}

/// `GET /api/v1/announcements{query}` as `token`, asserted 200 and parsed.
async fn listed(app: &Router, token: &str, query: &str) -> Vec<Value> {
    let response = list(app, token, query).await;
    assert_eq!(response.status, StatusCode::OK, "the list answers 200");
    response.json()
}

/// `POST /api/v1/announcements/{id}/dismiss` as `token`.
async fn dismiss(app: &Router, token: &str, id: Uuid) -> common::TestResponse {
    common::TestRequest::post(&format!("{ROOT}/{id}/dismiss"))
        .bearer(token)
        .send(app)
        .await
}

/// The `content` of every element of `rows`, in order.
fn contents(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .map(|row| {
            row["content"]
                .as_str()
                .expect("every announcement carries its content")
        })
        .collect()
}

/// The `(content, read)` pairs of `rows`, in order.
fn flags(rows: &[Value]) -> Vec<(&str, bool)> {
    rows.iter()
        .map(|row| {
            (
                row["content"].as_str().expect("content is a string"),
                row["read"].as_bool().expect("read is a boolean"),
            )
        })
        .collect()
}

/// The keys of a JSON object, sorted, for exact key-set assertions.
fn json_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("announcements render objects")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// How many dismissal rows the database holds.
async fn marker_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM announcement_reads")
        .fetch_one(pool)
        .await
        .expect("the dismissal rows count")
}

/// A notice with nothing in the table answers 200 with the empty JSON array.
#[tokio::test]
async fn empty_database_answers_the_empty_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let app = common::test_app(pool);

    let response = list(&app, &token, "").await;
    assert_eq!(response.status, StatusCode::OK, "200 for a reader");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the list is JSON"
    );
    assert_eq!(response.text(), "[]", "no notices, the empty array");
}

/// Only notices whose window holds this instant are listed, and the newest of
/// them first.
#[tokio::test]
async fn list_serves_active_notices_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _oldest: Announcement = seed(&pool, "oldest").await;
    let _middle: Announcement = seed(&pool, "middle").await;
    let _newest: Announcement = seed(&pool, "newest").await;
    let _expired: Announcement = seed_window(&pool, "expired", None, Some(at(PAST))).await;
    let _upcoming: Announcement = seed_window(&pool, "upcoming", Some(at(FUTURE)), None).await;
    let _spanning: Announcement =
        seed_window(&pool, "spanning", Some(at(PAST)), Some(at(FUTURE))).await;
    let (_alice, token) = fixture(&pool, "alice", "read").await;
    let app = common::test_app(pool);

    let body = listed(&app, &token, "").await;
    assert_eq!(
        contents(&body),
        ["spanning", "newest", "middle", "oldest"],
        "the four notices active now, newest first: `expired` has ended and \
         `upcoming` has not started"
    );
    assert!(
        body.iter().all(|row| row["read"] == json!(false)),
        "nobody has dismissed anything"
    );
}

/// One element of the list is the Mastodon `Announcement` entity: exactly its
/// keys, the row's stamps under Mastodon's names, the window in the Mastodon
/// time format, Pleroma's `raw_content` in the `pleroma` block, and the five
/// lookups as present-but-empty arrays.
#[tokio::test]
async fn a_notice_renders_the_announcement_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let stored = create(
        &pool,
        &NewAnnouncement {
            content: "<p>Scheduled maintenance</p>".to_owned(),
            raw_content: "Scheduled **maintenance**".to_owned(),
            starts_at: Some(at(PAST)),
            ends_at: Some(at(FUTURE)),
            all_day: true,
        },
    )
    .await;
    let (_alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let app = common::test_app(pool);

    let body = listed(&app, &token, "").await;
    assert_eq!(body.len(), 1, "the seeded notice, and only it");
    let value = &body[0];

    assert_eq!(json_keys(value), EXPECTED_KEYS, "exactly the entity keys");
    assert_eq!(value["id"], stored.id.to_string(), "the id as text");
    assert_eq!(
        value["content"], "<p>Scheduled maintenance</p>",
        "the sanitized html the row holds"
    );
    assert_eq!(
        value["pleroma"]["raw_content"], "Scheduled **maintenance**",
        "the source text in the `pleroma` block"
    );
    assert_eq!(
        json_keys(&value["pleroma"]),
        ["raw_content"],
        "which carries that key alone"
    );
    assert_eq!(value["starts_at"], PAST, "the window, both bounds");
    assert_eq!(value["ends_at"], FUTURE, "in the Mastodon time format");
    assert_eq!(value["all_day"], true, "the stored date-only flag");
    assert_eq!(value["read"], false, "a viewer who has dismissed nothing");
    for key in ["mentions", "statuses", "tags", "emojis", "reactions"] {
        assert_eq!(value[key], json!([]), "`{key}` is there, and empty");
    }

    let published = value["published_at"]
        .as_str()
        .expect("published_at is a string");
    assert_eq!(
        mastodon_time(&at(published)),
        published,
        "published_at is the row's insertion stamp in the Mastodon format"
    );
    assert_eq!(
        value["updated_at"], published,
        "and a notice nobody edited carries the same stamp twice"
    );
}

/// A dismissal takes the notice out of its owner's default list, shows it to
/// them with `read: true` when they ask for everything, and changes nothing for
/// anybody else.
#[tokio::test]
async fn dismissal_is_its_owners_own_state() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let hidden = seed(&pool, "dismissed").await;
    let seen = seed(&pool, "kept").await;
    let (_alice, alice_token) = fixture(&pool, "alice", "read write:accounts").await;
    let (_bob, bob_token) = fixture(&pool, "bob", "read:accounts").await;
    let app = common::test_app(pool);

    let response = dismiss(&app, &alice_token, hidden.id).await;
    assert_eq!(response.status, StatusCode::OK, "dismissing answers 200");
    assert_eq!(response.json_value(), json!({}), "with the empty object");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "which is JSON"
    );

    let mine = listed(&app, &alice_token, "").await;
    assert_eq!(
        flags(&mine),
        [("kept", false)],
        "the dismissed notice is out of her default list"
    );

    let everything = listed(&app, &alice_token, "?with_dismissed=true").await;
    assert_eq!(
        flags(&everything),
        [("kept", false), ("dismissed", true)],
        "it comes back, newest first and flagged read, on request"
    );

    let theirs = listed(&app, &bob_token, "").await;
    assert_eq!(
        flags(&theirs),
        [("kept", false), ("dismissed", false)],
        "bob still sees both, and reads neither"
    );
    assert_eq!(theirs[0]["id"], seen.id.to_string(), "newest first for him");
    assert_eq!(
        theirs[1]["id"],
        hidden.id.to_string(),
        "both rows are seeded"
    );
}

/// `with_dismissed` is a boolean, in the spellings a form client sends too, and
/// defaults to hiding dismissed notices.
#[tokio::test]
async fn with_dismissed_accepts_the_boolean_spellings() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let hidden = seed(&pool, "dismissed").await;
    let (_alice, token) = fixture(&pool, "alice", "read:accounts write:accounts").await;
    let app = common::test_app(pool);
    let response = dismiss(&app, &token, hidden.id).await;
    assert_eq!(response.status, StatusCode::OK, "the fixture dismisses");

    for query in [
        "?with_dismissed=true",
        "?with_dismissed=1",
        "?with_dismissed=yes",
    ] {
        let body = listed(&app, &token, query).await;
        assert_eq!(
            flags(&body),
            [("dismissed", true)],
            "`{query}` includes the dismissed notice"
        );
    }
    for query in [
        "",
        "?with_dismissed=false",
        "?with_dismissed=0",
        "?with_dismissed=no",
    ] {
        let body = listed(&app, &token, query).await;
        assert!(
            body.is_empty(),
            "`{query}` hides the dismissed notice, as the default does"
        );
    }
}

/// A `with_dismissed` that is not a boolean in any accepted spelling is a
/// validation error, not a silently ignored parameter.
#[tokio::test]
async fn an_unparsable_with_dismissed_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let app = common::test_app(pool);

    let response = list(&app, &token, "?with_dismissed=maybe").await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unusable parameter is a 422"
    );
    let body = response.json_value();
    let message = body["error"].as_str().expect("the error body is text");
    assert!(
        message.contains("expected a boolean"),
        "and says what it wanted: {message}"
    );
}

/// Dismissing twice stores one marker and answers the empty object both times.
#[tokio::test]
async fn dismissal_is_idempotent() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let news = seed(&pool, "news").await;
    let (_alice, token) = fixture(&pool, "alice", "read:accounts write:accounts").await;
    let app = common::test_app(pool.clone());

    for attempt in 1..=2 {
        let response = dismiss(&app, &token, news.id).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "attempt {attempt} dismisses with 200"
        );
        assert_eq!(
            response.text(),
            "{}",
            "attempt {attempt} answers the empty object"
        );
    }
    assert_eq!(marker_count(&pool).await, 1, "one marker row for two calls");

    let everything = listed(&app, &token, "?with_dismissed=true").await;
    assert_eq!(flags(&everything), [("news", true)], "listed once, read");
}

/// An id that names no notice — never minted, already deleted, or not an id at
/// all — is a 404, and stores nothing.
#[tokio::test]
async fn dismiss_of_a_notice_that_is_not_there_is_a_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let gone = seed(&pool, "withdrawn").await;
    let (_alice, token) = fixture(&pool, "alice", "read write").await;
    let app = common::test_app(pool.clone());
    assert!(
        announcement::delete(&pool, gone.id)
            .await
            .expect("the fixture notice deletes"),
        "the notice is there to be withdrawn"
    );

    for (what, target) in [
        ("never minted", Uuid::now_v7().to_string()),
        ("already deleted", gone.id.to_string()),
        ("not an id", "not-a-uuid".to_owned()),
    ] {
        let response = common::TestRequest::post(&format!("{ROOT}/{target}/dismiss"))
            .bearer(&token)
            .send(&app)
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{what} is a 404");
        assert_eq!(
            response.json_value()["error"],
            "Record not found",
            "with Pleroma's wording for {what}"
        );
    }
    assert_eq!(marker_count(&pool).await, 0, "and no dismissal was stored");
}

/// A notice outside its window is still a notice: dismissing it answers 200,
/// because the row exists even though the list hides it.
#[tokio::test]
async fn a_notice_outside_its_window_still_dismisses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let upcoming = seed_window(&pool, "upcoming", Some(at(FUTURE)), None).await;
    let (_alice, token) = fixture(&pool, "alice", "read write").await;
    let app = common::test_app(pool.clone());

    assert!(
        listed(&app, &token, "").await.is_empty(),
        "and it is not listed"
    );
    let response = dismiss(&app, &token, upcoming.id).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "dismissing an unlisted notice still works"
    );
    assert_eq!(marker_count(&pool).await, 1, "and stores the marker");
}

/// `GET /api/v1/announcements` needs a user token: nothing at all is a 401, a
/// bogus token a 401 naming the token, and an app-only token the 403 that says
/// a user is required.
#[tokio::test]
async fn list_requires_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _news: Announcement = seed(&pool, "news").await;
    let app_only = common::create_app_only_token(&pool, "read").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::get(ROOT).send(&app).await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token is a 401"
    );
    assert_eq!(
        anonymous.json_value()["error"],
        "Invalid credentials",
        "Pleroma's wording for an absent token"
    );

    let bogus = common::TestRequest::get(ROOT)
        .bearer("not-a-real-token")
        .send(&app)
        .await;
    assert_eq!(
        bogus.status,
        StatusCode::UNAUTHORIZED,
        "a bogus token is a 401"
    );
    assert_eq!(
        bogus.json_value()["error"],
        "The access token is invalid",
        "and says the token is the problem"
    );

    let as_app = list(&app, &app_only, "").await;
    assert_eq!(
        as_app.status,
        StatusCode::FORBIDDEN,
        "an app token is a 403"
    );
    assert_eq!(
        as_app.json_value()["error"],
        "This API requires an authenticated user",
        "naming what is missing"
    );
}

/// Listing needs `read:accounts`: a token that grants only another read
/// sub-scope, or only writes, is a 403, while the broader `read` covers it.
#[tokio::test]
async fn list_requires_read_accounts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_reader, narrow_read) = fixture(&pool, "reader", "read:statuses").await;
    let (_writer, write_only) = fixture(&pool, "writer", "write").await;
    let (_alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let (_bob, broad_read) = fixture(&pool, "bob", "read").await;
    let app = common::test_app(pool);

    for (who, other) in [("read:statuses", &narrow_read), ("write", &write_only)] {
        let response = list(&app, other, "").await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "`{who}` may not list"
        );
        assert_eq!(
            response.json_value()["error"],
            "Insufficient permissions: read:accounts.",
            "the denial names the missing scope for `{who}`"
        );
    }
    assert_eq!(
        list(&app, &token, "").await.status,
        StatusCode::OK,
        "`read:accounts` is enough"
    );
    assert_eq!(
        list(&app, &broad_read, "").await.status,
        StatusCode::OK,
        "and the broader `read` covers it"
    );
}

/// Dismissing needs `write:accounts`, which the broader `write` also grants; a
/// read-only caller — even one who reads accounts — is turned away, and so is a
/// request with no token at all.
#[tokio::test]
async fn dismiss_requires_write_accounts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let news = seed(&pool, "news").await;
    let (_reader, read_only) = fixture(&pool, "reader", "read").await;
    let (_account, account_read) = fixture(&pool, "account", "read:accounts").await;
    let (_writer, write_broad) = fixture(&pool, "writer", "write").await;
    let (_alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let app = common::test_app(pool.clone());

    for (who, other) in [("read", &read_only), ("read:accounts", &account_read)] {
        let response = dismiss(&app, other, news.id).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "`{who}` may not dismiss"
        );
        assert_eq!(
            response.json_value()["error"],
            "Insufficient permissions: write:accounts.",
            "the denial names the missing scope for `{who}`"
        );
    }

    let anonymous = common::TestRequest::post(&format!("{ROOT}/{}/dismiss", news.id))
        .send(&app)
        .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token is a 401"
    );

    assert_eq!(
        dismiss(&app, &token, news.id).await.status,
        StatusCode::OK,
        "`write:accounts` is enough"
    );
    assert_eq!(
        dismiss(&app, &write_broad, news.id).await.status,
        StatusCode::OK,
        "and the broader `write` covers it"
    );
    assert_eq!(
        marker_count(&pool).await,
        2,
        "both markers stored, one per account"
    );
}
