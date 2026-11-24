//! The registration hand-off: the states that cannot register, the next
//! callback going straight to the code, and the registration routes
//! answering JSON whatever the request asks for — never `text/html`.

use axum::http::StatusCode;
use wired_be::models::{oauth_token, user};

use super::provider::{PROVIDER, UNLINKED_UID};
use super::support::{
    assert_no_store, count_codes, count_identities, count_states, count_users, error_text, expire,
    fixture, location, pending_nickname, pending_state, post_form, prepare, query_param,
};
use crate::common;

/// Asserts `response` is JSON and not a page.
fn assert_json_not_html(response: &common::TestResponse, what: &str) {
    let content_type = response
        .header("content-type")
        .unwrap_or_default()
        .to_owned();
    assert_eq!(content_type, "application/json", "{what} answers JSON");
    assert!(!content_type.contains("text/html"), "{what} is no page");
    assert_no_store(response);
}

/// Neither registration route answers `text/html`: not for a browser's
/// `Accept`, not for a form-encoded submission, not for a refused one —
/// each is JSON, with the account details, the 422 and the redirect.
#[tokio::test]
async fn registration_routes_never_answer_html() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "rita", "read").await;
    let taken = common::create_local_user(&pool, "octocat").await;
    assert_eq!(taken.nickname, "octocat", "the collision fixture exists");
    let state = pending_state(&site.http, &site.app, "").await;
    let details = format!("/oauth/registration_details?state={state}");

    let plain = common::TestRequest::get(&details).send(&site.http).await;
    assert_eq!(plain.status, StatusCode::OK, "the details answer");
    assert_json_not_html(&plain, "details without an Accept");
    assert_eq!(
        plain.json_value()["nickname"],
        pending_nickname(),
        "carrying the pending account"
    );

    let browser = common::TestRequest::get(&details)
        .header("accept", "text/html,application/xhtml+xml,*/*;q=0.8")
        .send(&site.http)
        .await;
    assert_eq!(browser.status, StatusCode::OK, "a browser gets the details");
    assert_json_not_html(&browser, "details for a browser");

    let refused = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "unique@example.org"),
        ],
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a taken nickname is refused"
    );
    assert_json_not_html(&refused, "a refused form");
    assert_eq!(
        error_text(&refused),
        r#"{"nickname":["has already been taken"]}"#,
        "with the field map"
    );
    assert_eq!(count_users(&pool).await, 2, "nothing new was inserted");
    assert_eq!(count_identities(&pool).await, 0, "nothing was linked");
    assert_eq!(count_codes(&pool).await, 0, "and nothing was minted");
    assert_eq!(count_states(&pool).await, 1, "the state is still there");

    let done = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "fresh"),
            ("email", "unique@example.org"),
        ],
    )
    .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.text());
    assert_json_not_html(&done, "a finished form");
    assert!(
        done.json_value()["redirect_uri"].is_string(),
        "the redirect rides in the JSON"
    );
    site.provider.stop();
}

/// Both registration halves refuse states alike: never issued, empty,
/// absent, expired, and one that never saw an unlinked callback are the
/// 400 `{"error":"Invalid state"}` — no page, no user, no link.
#[tokio::test]
async fn registration_refuses_states_that_cannot_register() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "tess", "read").await;
    let aged = prepare(&site.http, &site.app, "").await;
    expire(&pool, &aged).await;
    let no_pending = prepare(&site.http, &site.app, "").await;
    let aged_get = format!("/oauth/registration_details?state={aged}");
    let no_pending_get = format!("/oauth/registration_details?state={no_pending}");

    for (uri, why) in [
        (
            "/oauth/registration_details?state=never-issued",
            "never issued",
        ),
        ("/oauth/registration_details?state=", "empty"),
        ("/oauth/registration_details", "absent"),
        (aged_get.as_str(), "expired"),
        (no_pending_get.as_str(), "live but no pending account"),
    ] {
        let response = common::TestRequest::get(uri).send(&site.http).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{why} (GET)");
        assert_json_not_html(&response, why);
        assert_eq!(error_text(&response), "Invalid state", "for {why} (GET)");
    }

    for (state, why) in [
        ("never-issued", "never issued"),
        ("", "empty"),
        (aged.as_str(), "expired"),
        (no_pending.as_str(), "no pending account"),
    ] {
        let response = post_form(
            &site.http,
            &[
                ("state", state),
                ("nickname", "whoever"),
                ("email", "w@x.y"),
            ],
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{why} (POST)");
        assert_json_not_html(&response, why);
        assert_eq!(error_text(&response), "Invalid state", "for {why} (POST)");
    }
    assert_eq!(count_users(&pool).await, 1, "only the fixture user");
    assert_eq!(count_identities(&pool).await, 0, "nothing was linked");
    assert_eq!(count_codes(&pool).await, 0, "and no code was minted");
    site.provider.stop();
}

/// The next login through the provider needs no registration any more:
/// the callback finds the identity and goes straight to the code.
#[tokio::test]
async fn the_next_callback_goes_straight_to_the_code() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "vic", "read").await;
    let state = pending_state(&site.http, &site.app, "").await;
    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "octocat@example.org"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "first login done");

    let again = prepare(&site.http, &site.app, "").await;
    let second = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={UNLINKED_UID}&state={again}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(second.status, StatusCode::FOUND, "a direct login");
    let url = location(&second);
    assert!(
        !url.contains("/oauth/external/register"),
        "no registration on the second trip: {url}"
    );
    assert!(query_param(&url, "code").is_some(), "a code instead: {url}");

    let created = user::get_by_nickname(&pool, "octocat")
        .await
        .expect("the lookup runs")
        .expect("the registration stands");
    let stored: oauth_token::OauthAuthorization =
        sqlx::query_as("SELECT * FROM oauth_authorizations WHERE token = $1")
            .bind(query_param(&url, "code").expect("the code"))
            .fetch_one(&pool)
            .await
            .expect("the second code is stored");
    assert_eq!(stored.user_id, created.id, "for the same user");
    site.provider.stop();
}
