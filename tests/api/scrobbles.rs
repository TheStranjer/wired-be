//! Scrobbles over HTTP (`/api/v1/pleroma`, WBE-121): `POST
//! /api/v1/pleroma/scrobble` and `GET /api/v1/pleroma/accounts/{id}/scrobbles`.
//!
//! The tests pin what the ticket promises: a created scrobble answers the
//! nine-key `Scrobble` entity with its account embedded and is then listed
//! newest first; the JSON and form spellings of the parameters both work and
//! `externalLink` shares `external_link`'s slot; a blank, absent or
//! 256-character title, an unknown `visibility` and a non-integer `length`
//! answer the 422s; `private` and `direct` scrobbles reach their owner only,
//! while `public` and `unlisted` ones reach anybody; the listing is
//! keyset-paginated (`limit`, `max_id`, `min_id`, `since_id`) with Pleroma's
//! `Link` header and no header on an empty page; an unknown, malformed or
//! deactivated account is a 404; and each route keeps its half of the auth
//! pipeline (`write` to scrobble, optional auth to list, with a bad token
//! still a 401).

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    scrobble::{self, NewScrobble, Scrobble as Stored},
    status::Visibility,
    user::User,
};

use crate::common;

/// The path a scrobble is posted to.
const SCROBBLE: &str = "/api/v1/pleroma/scrobble";
/// The instance origin [`common::test_config`] runs the app on.
const BASE: &str = "https://test.example";
/// Every JSON key a scrobble entity must carry, sorted.
const EXPECTED_KEYS: [&str; 9] = [
    "account",
    "album",
    "artist",
    "created_at",
    "externalLink",
    "external_link",
    "id",
    "length",
    "title",
];
/// The 422 body for an absent or blank title.
const TITLE_BLANK_MESSAGE: &str = "title: can't be blank";
/// The 422 body for a visibility outside the four the endpoint lists.
const VISIBILITY_MESSAGE: &str = "visibility: is not included in the list";
/// The 422 body for a title over 255 characters.
const TITLE_LONG_MESSAGE: &str = "title: is too long (maximum is 255 characters)";
/// A title of exactly the longest length the endpoint accepts.
fn long_title() -> String {
    "y".repeat(255)
}

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// Inserts a scrobble straight through the model — `title` and `visibility`,
/// the column defaults otherwise — and returns its row.
async fn seed(pool: &PgPool, owner: &User, title: &str, visibility: Visibility) -> Stored {
    scrobble::insert(
        pool,
        BASE,
        owner.id,
        &NewScrobble {
            title: title.to_owned(),
            visibility,
            ..NewScrobble::default()
        },
    )
    .await
    .expect("fixture scrobble inserts")
}

/// The listing path of the account `account` (an id, or anything else a
/// client might put in the segment), with `query` appended.
fn list_path(account: &str, query: &str) -> String {
    format!("/api/v1/pleroma/accounts/{account}/scrobbles{query}")
}

/// The listing path of the user `account`, with `query` appended.
fn account_path(account: &User, query: &str) -> String {
    list_path(&account.id.to_string(), query)
}

/// `POST /api/v1/pleroma/scrobble` with the JSON `body` as `token`.
async fn create_json(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(SCROBBLE)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `GET` of `path` as `token` (`None` for an anonymous request).
async fn list(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    let request = match token {
        Some(found) => request.bearer(found),
        None => request,
    };
    request.send(app).await
}

/// The `title` of every entity of a listing response, in response order.
fn titles(body: &Value) -> Vec<&str> {
    body.as_array()
        .expect("the listing is an array")
        .iter()
        .map(|row| row["title"].as_str().expect("a scrobble title"))
        .collect()
}

/// How many rows a listing response holds.
fn page_len(body: &Value) -> usize {
    body.as_array().expect("the listing is an array").len()
}

/// The exact error body Pleroma's fallback renders for `message`.
fn error_body(message: &str) -> Value {
    json!({"error": message})
}

#[tokio::test]
async fn a_full_scrobble_is_created_and_lists_back() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = fixture(&pool, "alice", "write").await;

    let created = create_json(
        &app,
        &token,
        json!({
            "title": "Midnight City",
            "artist": "M83",
            "album": "Hurry Up, We're Dreaming",
            "length": 243_000,
            "externalLink": "https://example.test/midnight-city",
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "Pleroma answers 200");
    let entity = created.json_value();
    assert_eq!(
        common::sorted_keys(&entity),
        EXPECTED_KEYS,
        "exactly the keys"
    );
    let id: Uuid = entity["id"]
        .as_str()
        .and_then(|text| Uuid::parse_str(text).ok())
        .expect("the id is id text");
    assert_eq!(entity["id"], id.to_string(), "as the row knows it");
    assert_eq!(entity["title"], "Midnight City", "the title verbatim");
    assert_eq!(entity["artist"], "M83", "the artist");
    assert_eq!(entity["album"], "Hurry Up, We're Dreaming", "the album");
    assert_eq!(entity["length"], 243_000, "the length in milliseconds");
    assert_eq!(
        entity["external_link"], "https://example.test/midnight-city",
        "the camelCase parameter lands in `external_link`"
    );
    assert_eq!(
        entity["externalLink"], "https://example.test/midnight-city",
        "and the camelCase twin Pleroma-FE reads carries it too"
    );
    assert_eq!(
        entity["account"]["id"],
        alice.id.to_string(),
        "the account is the scrobbler"
    );
    assert_eq!(
        entity["account"]["acct"], "alice",
        "as a local handle, no domain"
    );
    assert_eq!(
        entity["account"]["url"],
        format!("{BASE}/users/alice"),
        "rendered as the Mastodon Account entity"
    );
    assert!(
        entity["created_at"]
            .as_str()
            .is_some_and(|text| text.contains('T') && text.ends_with('Z')),
        "created_at is a Mastodon timestamp: {}",
        entity["created_at"]
    );

    let listed = list(&app, Some(&token), &account_path(&alice, "")).await;
    assert_eq!(listed.status, StatusCode::OK, "the listing answers 200");
    assert_eq!(
        titles(&listed.json_value()),
        ["Midnight City"],
        "and the scrobble is in it"
    );
    assert_eq!(
        listed.json_value()[0]["id"],
        entity["id"],
        "the same row either way"
    );
}

#[tokio::test]
async fn a_form_body_creates_the_same_scrobble_and_snake_case_wins() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = fixture(&pool, "alice", "write").await;

    let created = common::TestRequest::post(SCROBBLE)
        .bearer(&token)
        .form(&[
            ("title", "  Nightcall  "),
            ("length", "180000"),
            ("external_link", "https://example.test/nightcall"),
        ])
        .send(&app)
        .await;
    assert_eq!(created.status, StatusCode::OK, "form parameters work");
    let entity = created.json_value();
    assert_eq!(
        entity["title"], "  Nightcall  ",
        "the title is stored verbatim, spaces included"
    );
    assert_eq!(entity["length"], 180_000, "a form integer is a number");
    assert_eq!(
        entity["external_link"], "https://example.test/nightcall",
        "and the snake_case spelling is accepted too"
    );
    assert!(entity["artist"].is_null(), "absent fields are null");
    assert!(entity["album"].is_null(), "both of them");

    // Pleroma's `put_new` rule: an `external_link` parameter wins over the
    // deprecated `externalLink` one.
    let both = create_json(
        &app,
        &token,
        json!({
            "title": "Both spellings",
            "externalLink": "https://example.test/camel",
            "external_link": "https://example.test/snake",
        }),
    )
    .await;
    assert_eq!(
        both.json_value()["external_link"],
        "https://example.test/snake",
        "snake_case wins, and sending both is no error"
    );

    let listed = list(&app, Some(&token), &account_path(&alice, "?limit=40")).await;
    assert_eq!(
        titles(&listed.json_value()),
        ["Both spellings", "  Nightcall  "],
        "newest first, verbatim"
    );
}

#[tokio::test]
async fn an_empty_body_and_a_blank_title_are_the_documented_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = fixture(&pool, "alice", "write").await;

    let responses = [
        (
            "no body at all",
            common::TestRequest::post(SCROBBLE)
                .bearer(&token)
                .send(&app)
                .await,
        ),
        (
            "an empty JSON object",
            create_json(&app, &token, json!({})).await,
        ),
        (
            "a null title",
            create_json(&app, &token, json!({"title": null})).await,
        ),
        (
            "an empty title",
            create_json(&app, &token, json!({"title": ""})).await,
        ),
        (
            "a blank title",
            create_json(&app, &token, json!({"title": "   "})).await,
        ),
        (
            "only other parameters",
            create_json(&app, &token, json!({"artist": "M83"})).await,
        ),
    ];
    for (label, response) in responses {
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{label} is a 422"
        );
        assert_eq!(
            response.json_value(),
            error_body(TITLE_BLANK_MESSAGE),
            "and says so about `title`: {label}"
        );
    }

    let listed = list(&app, Some(&token), &account_path(&alice, "")).await;
    assert_eq!(listed.json_value(), json!([]), "and none of them stored");
}

#[tokio::test]
async fn the_other_parameter_rules_are_422s_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = fixture(&pool, "alice", "write").await;

    let too_long = create_json(&app, &token, json!({"title": "x".repeat(256)})).await;
    assert_eq!(
        too_long.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "256 characters are too many"
    );
    assert_eq!(
        too_long.json_value(),
        error_body(TITLE_LONG_MESSAGE),
        "with the wording"
    );

    let edge = create_json(&app, &token, json!({"title": long_title()})).await;
    assert_eq!(
        edge.status,
        StatusCode::OK,
        "while exactly 255 characters pass"
    );

    for value in ["", "local", "Public", "unreadable"] {
        let response = create_json(&app, &token, json!({"title": "x", "visibility": value})).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{value}` is no scrobble visibility"
        );
        assert_eq!(
            response.json_value(),
            error_body(VISIBILITY_MESSAGE),
            "and the 422 names the field"
        );
    }

    for body in [
        json!({"title": "x", "length": "abc"}),
        json!({"title": "x", "length": 1.5}),
        json!({"title": "x", "length": 2_147_483_648_i64}),
    ] {
        let response = create_json(&app, &token, body.clone()).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{body}` is no millisecond count"
        );
        assert_eq!(
            common::sorted_keys(&response.json_value()),
            ["error"],
            "and the body is the one-key error shape"
        );
    }

    // Only the 255-character title went in, whatever the rejects were.
    let _later: Stored = seed(&pool, &alice, "seeded after", Visibility::Public).await;
    let listed = list(&app, Some(&token), &account_path(&alice, "")).await;
    assert_eq!(
        titles(&listed.json_value()),
        ["seeded after", long_title().as_str()],
        "one accepted create, newest first"
    );
}

#[tokio::test]
async fn private_scrobbles_are_visible_to_their_owner_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "alice", "read write").await;
    let (_bob, bob_token) = fixture(&pool, "bob", "read").await;
    for (title, visibility) in [
        ("public one", Visibility::Public),
        ("unlisted one", Visibility::Unlisted),
        ("private one", Visibility::Private),
        ("direct one", Visibility::Direct),
    ] {
        let _row: Stored = seed(&pool, &alice, title, visibility).await;
    }

    let hers = list(&app, Some(&alice_token), &account_path(&alice, "")).await;
    assert_eq!(
        titles(&hers.json_value()),
        ["direct one", "private one", "unlisted one", "public one"],
        "the owner sees all four, newest first"
    );

    let stranger = list(&app, Some(&bob_token), &account_path(&alice, "")).await;
    let anonymous = list(&app, None, &account_path(&alice, "")).await;
    assert_eq!(
        titles(&stranger.json_value()),
        ["unlisted one", "public one"],
        "another account sees the two open ones"
    );
    assert_eq!(
        titles(&anonymous.json_value()),
        ["unlisted one", "public one"],
        "and so does nobody"
    );

    // Posting one privately works, and the entity keeps its shape.
    let private = create_json(
        &app,
        &alice_token,
        json!({"title": "quietly", "visibility": "direct"}),
    )
    .await;
    assert_eq!(private.status, StatusCode::OK, "direct is accepted");
    assert_eq!(
        common::sorted_keys(&private.json_value()),
        EXPECTED_KEYS,
        "and answers the plain entity, which names no visibility"
    );
    let after = list(&app, Some(&bob_token), &account_path(&alice, "")).await;
    assert_eq!(
        titles(&after.json_value()),
        ["unlisted one", "public one"],
        "and it stays out of bob's view"
    );
}

#[tokio::test]
async fn private_scrobbles_need_a_token_granting_read_scrobbles() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, read_token) = fixture(&pool, "alice", "read").await;
    let granular = common::create_app_and_token(&pool, &alice, "read:scrobbles").await;
    let write_only = common::create_app_and_token(&pool, &alice, "write").await;
    let narrow = common::create_app_and_token(&pool, &alice, "write:statuses push").await;
    for (title, visibility) in [
        ("public one", Visibility::Public),
        ("unlisted one", Visibility::Unlisted),
        ("private one", Visibility::Private),
        ("direct one", Visibility::Direct),
    ] {
        let _row: Stored = seed(&pool, &alice, title, visibility).await;
    }
    let path = account_path(&alice, "");
    let all = ["direct one", "private one", "unlisted one", "public one"];
    let open = ["unlisted one", "public one"];

    for (label, token) in [("`read`", &read_token), ("`read:scrobbles`", &granular)] {
        let response = list(&app, Some(token), &path).await;
        assert_eq!(
            titles(&response.json_value()),
            all,
            "the owner's {label} token sees every scrobble"
        );
    }
    for (label, token) in [("`write`", &write_only), ("`write:statuses push`", &narrow)] {
        let response = list(&app, Some(token), &path).await;
        assert_eq!(response.status, StatusCode::OK, "{label} may still list");
        assert_eq!(
            titles(&response.json_value()),
            open,
            "but the owner's {label} token reads as no token"
        );
    }
    let anonymous = list(&app, None, &path).await;
    assert_eq!(
        titles(&anonymous.json_value()),
        open,
        "and anonymous sees only public and unlisted"
    );
}

/// `GET`s the scrobble list of `owner`'s own account with `query` (its leading
/// `?` and all) as `token`.
async fn own_page(app: &Router, token: &str, owner: &User, query: &str) -> common::TestResponse {
    list(app, Some(token), &list_path(&owner.id.to_string(), query)).await
}

#[tokio::test]
async fn the_listing_pages_by_id_cursor_and_links() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = fixture(&pool, "alice", "write").await;
    let mut rows: Vec<Stored> = Vec::new();
    for n in 0..5 {
        let row = seed(&pool, &alice, &format!("track {n}"), Visibility::Public).await;
        rows.push(row);
    }
    let path = account_path(&alice, "");

    let first = own_page(&app, &token, &alice, "?limit=2").await;
    let page = first.json_value();
    assert_eq!(titles(&page), ["track 4", "track 3"], "two newest first");
    assert_eq!(
        first.header("link"),
        Some(common::expected_link(&path, &rows[4].id, &rows[3].id).as_str()),
        "Pleroma's add_link_headers, over the account's own path"
    );

    let older = own_page(
        &app,
        &token,
        &alice,
        &format!("?limit=2&max_id={}", rows[3].id),
    )
    .await;
    assert_eq!(
        titles(&older.json_value()),
        ["track 2", "track 1"],
        "the next cursor walks back"
    );

    let forward = own_page(
        &app,
        &token,
        &alice,
        &format!("?limit=2&min_id={}", rows[2].id),
    )
    .await;
    assert_eq!(
        titles(&forward.json_value()),
        ["track 4", "track 3"],
        "min_id answers the newest-first page just after the cursor"
    );

    let polled = own_page(&app, &token, &alice, &format!("?since_id={}", rows[3].id)).await;
    assert_eq!(
        titles(&polled.json_value()),
        ["track 4"],
        "since_id gives only what is newer"
    );
    assert_eq!(
        polled.header("link"),
        Some(common::expected_link(&path, &rows[4].id, &rows[4].id).as_str()),
        "a one-row page carries its own id on both cursors"
    );

    let empty = own_page(&app, &token, &alice, &format!("?since_id={}", rows[4].id)).await;
    assert_eq!(empty.status, StatusCode::OK, "nothing new is still a 200");
    assert_eq!(empty.json_value(), json!([]), "and the empty array");
    assert_eq!(empty.header("link"), None, "with no Link header at all");

    let bad_account = list(&app, Some(&token), &list_path("not-an-id", "")).await;
    assert_eq!(
        bad_account.status,
        StatusCode::NOT_FOUND,
        "an account segment that is no id is the 404"
    );

    let bad_page = list(&app, Some(&token), &account_path(&alice, "?max_id=nope")).await;
    assert_eq!(
        bad_page.status,
        StatusCode::BAD_REQUEST,
        "while a malformed cursor is Pleroma's 400"
    );
    assert_eq!(
        bad_page.json_value(),
        error_body("Invalid id"),
        "with its message"
    );

    let bad_both = list(&app, Some(&token), &list_path("not-an-id", "?max_id=nope")).await;
    assert_eq!(
        bad_both.status,
        StatusCode::BAD_REQUEST,
        "and the cursor wins: the page extractor runs before the handler"
    );
}

#[tokio::test]
async fn the_default_page_is_twenty_and_the_cap_is_forty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = fixture(&pool, "alice", "read").await;
    for n in 0..45 {
        let _row: Stored = seed(&pool, &alice, &format!("track {n:02}"), Visibility::Public).await;
    }
    let path = account_path(&alice, "");

    let default_page = list(&app, Some(&token), &path).await;
    let body = default_page.json_value();
    assert_eq!(titles(&body)[0], "track 44", "the newest first");
    assert_eq!(page_len(&body), 20, "and twenty rows by default");

    let greedy = own_page(&app, &token, &alice, "?limit=100").await;
    assert_eq!(
        page_len(&greedy.json_value()),
        40,
        "and at most forty, Pleroma's `@max_limit`"
    );
}

#[tokio::test]
async fn an_account_with_no_scrobbles_lists_empty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = fixture(&pool, "alice", "read").await;
    let (bob, _) = fixture(&pool, "bob", "read").await;

    let _row: Stored = seed(&pool, &alice, "mine", Visibility::Public).await;
    let empty = list(&app, Some(&token), &account_path(&bob, "")).await;
    assert_eq!(
        empty.status,
        StatusCode::OK,
        "an account with none is a 200"
    );
    assert_eq!(empty.json_value(), json!([]), "and the empty array");
    assert_eq!(empty.header("link"), None, "with no Link header");

    let all = list(&app, Some(&token), &account_path(&alice, "")).await;
    assert_eq!(
        titles(&all.json_value()),
        ["mine"],
        "while her own listing holds her row only"
    );
}

#[tokio::test]
async fn unknown_malformed_and_deactivated_accounts_are_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = fixture(&pool, "alice", "read").await;
    let ghost = Uuid::now_v7();
    let deactivated =
        common::create_deactivated_user_with_password(&pool, "gone", "password1").await;

    for (label, path) in [
        ("an unknown id", list_path(&ghost.to_string(), "")),
        ("an id that is no id", list_path("not-an-id", "")),
        (
            "a deactivated account",
            list_path(&deactivated.id.to_string(), ""),
        ),
    ] {
        let with_token = list(&app, Some(&token), &path).await;
        let anonymous = list(&app, None, &path).await;
        assert_eq!(
            with_token.status,
            StatusCode::NOT_FOUND,
            "{label} is a 404 for a reader"
        );
        assert_eq!(
            anonymous.status,
            StatusCode::NOT_FOUND,
            "and for nobody: {label}"
        );
        assert_eq!(
            with_token.json_value(),
            error_body("Record not found"),
            "with Pleroma's body"
        );
        assert_eq!(
            anonymous.json_value(),
            error_body("Record not found"),
            "the same either way"
        );
    }
}

#[tokio::test]
async fn creating_needs_a_user_token_granting_write() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, read_only) = fixture(&pool, "alice", "read").await;
    let (bob, bob_token) = fixture(&pool, "bob", "write").await;
    let app_only = common::create_app_only_token(&pool, "write").await;
    let body = json!({"title": "who is asking"});

    let anonymous = common::TestRequest::post(SCROBBLE)
        .json(&body)
        .send(&app)
        .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token is a 401"
    );
    assert_eq!(
        anonymous.json_value(),
        error_body("Invalid credentials"),
        "Pleroma's wording"
    );

    let wrong_scope = create_json(&app, &read_only, body.clone()).await;
    assert_eq!(
        wrong_scope.status,
        StatusCode::FORBIDDEN,
        "a read token cannot scrobble"
    );
    assert_eq!(
        wrong_scope.json_value(),
        error_body("Insufficient permissions: write:scrobbles."),
        "and is told which scope it lacks"
    );

    let (_carol, narrow) = fixture(&pool, "carol", "write:scrobbles").await;
    let granular = create_json(&app, &narrow, body.clone()).await;
    assert_eq!(
        granular.status,
        StatusCode::OK,
        "while the granular `write:scrobbles` scope is enough"
    );
    let (_dave, status_only) = fixture(&pool, "dave", "write:statuses").await;
    let sibling = create_json(&app, &status_only, body.clone()).await;
    assert_eq!(
        sibling.status,
        StatusCode::FORBIDDEN,
        "and a sibling write scope is not"
    );

    let app_token = create_json(&app, &app_only, body.clone()).await;
    assert_eq!(
        app_token.status,
        StatusCode::FORBIDDEN,
        "and an app-only token has no account to scrobble for"
    );

    let bogus = create_json(&app, "not-a-token", body.clone()).await;
    assert_eq!(
        bogus.status,
        StatusCode::UNAUTHORIZED,
        "a bogus token is a 401"
    );
    assert_eq!(
        bogus.json_value(),
        error_body("The access token is invalid"),
        "Pleroma's wording"
    );

    let written = create_json(&app, &bob_token, body).await;
    assert_eq!(
        written.status,
        StatusCode::OK,
        "while a `write` token scrobbles"
    );
    assert_eq!(
        written.json_value()["account"]["id"],
        bob.id.to_string(),
        "for its own account"
    );

    let alice_view = list(&app, Some(&read_only), &account_path(&alice, "")).await;
    assert_eq!(
        alice_view.json_value(),
        json!([]),
        "and nothing landed on the rejected readers"
    );
}

#[tokio::test]
async fn listing_answers_anonymously_but_still_checks_a_present_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = fixture(&pool, "alice", "read").await;
    let _row: Stored = seed(&pool, &alice, "open", Visibility::Public).await;
    let path = account_path(&alice, "");

    let anonymous = list(&app, None, &path).await;
    assert_eq!(
        anonymous.status,
        StatusCode::OK,
        "Pleroma proceeds unauthenticated"
    );
    assert_eq!(
        titles(&anonymous.json_value()),
        ["open"],
        "and answers the public scrobbles"
    );
    assert_eq!(
        anonymous.json_value()[0]["account"]["pleroma"]["relationship"],
        json!({}),
        "with the empty relationship Pleroma's account view renders"
    );

    let bogus = list(&app, Some("not-a-token"), &path).await;
    assert_eq!(
        bogus.status,
        StatusCode::UNAUTHORIZED,
        "but a bad token is never ignored"
    );
    assert_eq!(
        bogus.json_value(),
        error_body("The access token is invalid"),
        "and answers the 401"
    );

    let app_reader = common::create_app_only_token(&pool, "read").await;
    let app_only = list(&app, Some(&app_reader), &path).await;
    assert_eq!(
        app_only.status,
        StatusCode::OK,
        "an app-only token is no reader, so it sees the public rows"
    );
    assert_eq!(
        titles(&app_only.json_value()),
        ["open"],
        "exactly like nobody"
    );

    let scoped = list(&app, Some(&token), &path).await;
    assert_eq!(
        scoped.status,
        StatusCode::OK,
        "and any read scope (or none at all) may list"
    );
}
