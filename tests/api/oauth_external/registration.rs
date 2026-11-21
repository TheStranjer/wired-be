//! The registration hand-off itself: the prefilled form of `GET
//! /oauth/registration_details` and its escaping, the states that cannot
//! register, `POST /oauth/register` creating the user and minting a code
//! that redeems, the next callback going straight to the code, and a
//! taken nickname re-rendering the form.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::{
    oauth_token,
    user::{self, register},
    user_identity,
};

use super::provider::{PROVIDER, REDIRECT, UNLINKED_UID};
use super::support::{
    assert_no_store, count_codes, count_identities, count_states, count_users, error_text, expire,
    fixture, location, pending_nickname, pending_state, post_form, prepare, query_param,
    set_pending,
};
use crate::common;

/// `GET /oauth/registration_details` renders the form for `state`: 200
/// HTML with the `pending` account's nickname, email and name prefilled,
/// a `bio` field, the hidden state and the post target, under no-store.
#[tokio::test]
async fn registration_details_renders_the_prefilled_form() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "rita", "read").await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = common::TestRequest::get(&format!("/oauth/registration_details?state={state}"))
        .send(&site.http)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the form renders");
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "and it is a page"
    );
    assert_no_store(&response);

    let page = response.text();
    assert!(
        page.contains("<form method=\"post\" action=\"/oauth/register\">"),
        "the form posts to /oauth/register: {page}"
    );
    assert!(
        page.contains(&format!(
            "name=\"nickname\" value=\"{}\"",
            pending_nickname()
        )),
        "the provider's nickname is prefilled: {page}"
    );
    assert!(
        page.contains(&format!(
            "name=\"email\" value=\"user-{UNLINKED_UID}@example.org\""
        )),
        "so is the email: {page}"
    );
    assert!(
        page.contains(&format!("name=\"name\" value=\"User {UNLINKED_UID}\"")),
        "and the name: {page}"
    );
    assert!(page.contains("name=\"bio\""), "and the bio is editable");
    assert!(
        page.contains(&format!(
            "<input type=\"hidden\" name=\"state\" value=\"{state}\">"
        )),
        "the state rides along hidden: {page}"
    );
    site.provider.stop();
}

/// Provider info is escaped on the page: markup in a nickname, email or
/// name cannot open a tag or break out of its attribute.
#[tokio::test]
async fn registration_details_escapes_the_provider_values() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "sara", "read").await;
    let state = prepare(&site.http, &site.app, "").await;
    set_pending(
        &pool,
        &state,
        &json!({
            "provider": PROVIDER,
            "uid": "1",
            "nickname": "\"><script>alert(1)</script>",
            "email": "a&b@example.org",
            "name": "<img src=x onerror=alert(2)>",
        }),
    )
    .await;

    let page = common::TestRequest::get(&format!("/oauth/registration_details?state={state}"))
        .send(&site.http)
        .await
        .text();
    assert!(
        page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "the nickname shows escaped: {page}"
    );
    assert!(!page.contains("<script>alert(1)"), "and never raw");
    assert!(
        page.contains("value=\"a&amp;b@example.org\""),
        "ampersands escape in the email too"
    );
    assert!(
        page.contains("&lt;img src=x onerror=alert(2)&gt;"),
        "the name cannot embed an image"
    );
    assert!(
        !page.contains("\"><script>"),
        "and no value breaks out of its attribute"
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
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the refusal is JSON, not a page, for the state that {why}"
        );
        assert_eq!(error_text(&response), "Invalid state", "for {why} (GET)");
        assert_no_store(&response);
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
        assert_eq!(error_text(&response), "Invalid state", "for {why} (POST)");
        assert_no_store(&response);
    }
    assert_eq!(count_users(&pool).await, 1, "only the fixture user");
    assert_eq!(count_identities(&pool).await, 0, "nothing was linked");
    assert_eq!(count_codes(&pool).await, 0, "and no code was minted");
    site.provider.stop();
}

/// The full hand-off in one go: an unlinked callback's state comes back
/// from `POST /oauth/register` as the very answer the callback gives a
/// linked user — a 302 to the app's `redirect_uri` with `code` and the
/// client's `state`. The code is redeemable by its application, the
/// identity is linked, the state row is gone, and the account carries a
/// password no one can type: `authenticate` refuses it.
#[tokio::test]
async fn registration_creates_the_user_and_mints_a_working_code() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "uma", "read write").await;
    let state = pending_state(&site.http, &site.app, "&state=client-state").await;

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "octocat@example.org"),
            ("name", "The Octocat"),
            ("bio", "lives in a repo"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::FOUND, "the app is redirected");
    assert_no_store(&response);
    let url = location(&response);
    let code = query_param(&url, "code").expect("a code on the redirect");
    assert_eq!(
        url,
        format!("{REDIRECT}?code={code}&state=client-state"),
        "the registered URI, the code and the client's state — nothing else"
    );
    assert_eq!(code.len(), 43, "the 43-character Pleroma code shape");

    let redeemed = oauth_token::consume_authorization(&pool, &code, site.app.id)
        .await
        .expect("the redemption reaches the database")
        .expect("the owning app can redeem the code");

    let created = user::get_by_nickname(&pool, "octocat")
        .await
        .expect("the lookup runs")
        .expect("the registration created the user");
    assert_eq!(redeemed.user_id, created.id, "the code names the new user");
    assert_eq!(created.email.as_deref(), Some("octocat@example.org"));
    assert_eq!(created.name, "The Octocat", "the submitted display name");
    assert_eq!(created.raw_bio, "lives in a repo", "and the bio");
    assert!(
        created.password_hash.is_some(),
        "the row carries a hash — of a random password no one knows"
    );
    assert!(
        !created.approval_pending.get(),
        "plain instance: nothing waits on it"
    );
    assert!(
        created.email_confirmed.get(),
        "and plain instances do not demand confirmation"
    );
    for guess in ["", "lives in a repo", "octocat", "hunter2"] {
        assert!(
            register::authenticate(&pool, "octocat", guess)
                .await
                .is_none(),
            "no guess logs the external account in by password"
        );
    }

    assert_eq!(
        count_states(&pool).await,
        0,
        "the finished state row is deleted"
    );
    let linked = user_identity::find_user(&pool, PROVIDER, UNLINKED_UID)
        .await
        .expect("the identity lookup runs")
        .expect("the provider account is linked");
    assert_eq!(linked.id, created.id, "to the freshly registered user");
    site.provider.stop();
}

/// The next login through the provider needs no form any more: the
/// callback finds the identity and goes straight to the code.
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
    assert_eq!(response.status, StatusCode::FOUND, "first login done");

    let again = prepare(&site.http, &site.app, "").await;
    let second = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={UNLINKED_UID}&state={again}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(second.status, StatusCode::FOUND, "a direct login");
    let url = location(&second);
    assert!(
        !url.contains("registration_details"),
        "no form on the second trip: {url}"
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

/// A nickname that is taken re-renders the form at 200 with the field
/// message, and changes nothing on the database side: no user, no link,
/// and the state is still live for another submission.
#[tokio::test]
async fn registration_reports_a_duplicate_nickname_on_the_form() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "wen", "read").await;
    let taken = common::create_local_user(&pool, "octocat").await;
    assert_eq!(taken.nickname, "octocat", "the collision fixture exists");
    let state = pending_state(&site.http, &site.app, "").await;

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "unique@example.org"),
        ],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the form comes back at 200"
    );
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "as a page"
    );
    assert_no_store(&response);
    let page = response.text();
    assert!(
        page.contains("nickname has already been taken"),
        "with the field message: {page}"
    );
    assert!(
        page.contains(&format!(
            "<input type=\"hidden\" name=\"state\" value=\"{state}\">"
        )),
        "and the same state to retry with"
    );
    assert!(
        page.contains("value=\"octocat\""),
        "keeping what the user typed"
    );
    assert_eq!(count_users(&pool).await, 2, "nothing new was inserted");
    assert_eq!(count_identities(&pool).await, 0, "nothing was linked");
    assert_eq!(count_codes(&pool).await, 0, "and nothing was minted");
    assert_eq!(count_states(&pool).await, 1, "the state is still there");
    site.provider.stop();
}
