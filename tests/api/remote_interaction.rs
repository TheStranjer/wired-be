//! Remote interaction over HTTP (WBE-188): the `POST
//! /api/v1/pleroma/remote_interaction` endpoint, the `/ostatus_subscribe`
//! login form and follow write, `GET /authorize_interaction`'s frontend
//! redirects and the `/main/ostatus` remote-subscribe pair
//! (`src/api/remote_interaction.rs`).
//!
//! The WebFinger half of the flow needs a live `https://{domain}` peer,
//! so the HTTP success path is pinned by the module's own mock-server
//! unit tests; what this suite pins over the real router is the
//! parameter refusals (every case is network-free: an unresolvable
//! profile is spelled with a single-label domain that the `WebFinger`
//! client rejects before any request), the pages' HTML shape and
//! escaping, the form-authenticated follow write against stored rows,
//! and the redirects a locally stored actor, handle and status get.

#![cfg(test)]

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::{
    follow::{self, FollowState},
    status::Visibility,
    user::User,
};

use crate::common;

/// The JSON endpoint under test.
const ROUTE: &str = "/api/v1/pleroma/remote_interaction";
/// The instance origin every fixture URL is built from.
const BASE: &str = "https://test.example";
/// A local actor URL, the `ap_id` the endpoint is asked to expand.
const AP_ID: &str = "https://test.example/users/alice";
/// A handle `WebFinger` refuses before any request (single-label
/// domain), so the refusal tests stay network-free.
const UNRESOLVABLE: &str = "ghost@invalid";
/// The 400 body both unresolvable-profile paths share.
const COULD_NOT_FIND_USER: &str = r#"{"error":"Couldn't find user"}"#;
/// The 404 body of `GET /authorize_interaction`.
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;

/// The 400 an absent, blank or `null` `ap_id` is refused with.
#[tokio::test]
async fn remote_interaction_refuses_a_missing_ap_id() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let no_field = common::TestRequest::post(ROUTE)
        .form(&[("profile", "bob@remote.example")])
        .send(&app)
        .await;
    let blank = common::TestRequest::post(ROUTE)
        .form(&[("ap_id", "   "), ("profile", "bob@remote.example")])
        .send(&app)
        .await;
    let nulled = common::TestRequest::post(ROUTE)
        .json(&json!({"ap_id": null, "profile": "bob@remote.example"}))
        .send(&app)
        .await;
    let nothing = common::TestRequest::post(ROUTE).send(&app).await;
    for response in [&no_field, &blank, &nulled, &nothing] {
        common::assert_answer(response, StatusCode::BAD_REQUEST, "a missing ap_id");
        common::assert_error(response, "Missing parameter: ap_id", "a missing ap_id");
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "refusals are JSON (decision 8)"
        );
    }
}

/// The 400 an absent, blank or `null` `profile` is refused with — only
/// once `ap_id` is in, since the checks run in parameter order.
#[tokio::test]
async fn remote_interaction_refuses_a_missing_profile() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let no_field = common::TestRequest::post(ROUTE)
        .form(&[("ap_id", AP_ID)])
        .send(&app)
        .await;
    let blank = common::TestRequest::post(ROUTE)
        .form(&[("ap_id", AP_ID), ("profile", "")])
        .send(&app)
        .await;
    let nulled = common::TestRequest::post(ROUTE)
        .json(&json!({"ap_id": AP_ID, "profile": null}))
        .send(&app)
        .await;
    for response in [&no_field, &blank, &nulled] {
        common::assert_answer(response, StatusCode::BAD_REQUEST, "a missing profile");
        common::assert_error(response, "Missing parameter: profile", "a missing profile");
    }
}

/// A profile this instance cannot resolve — an unusable handle grammar,
/// so no `WebFinger` request ever leaves — is the 400
/// `{"error":"Couldn't find user"}`, from a form and from JSON alike.
#[tokio::test]
async fn remote_interaction_refuses_an_unresolvable_profile() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let form = common::TestRequest::post(ROUTE)
        .form(&[("ap_id", AP_ID), ("profile", UNRESOLVABLE)])
        .send(&app)
        .await;
    let json_body = common::TestRequest::post(ROUTE)
        .json(&json!({"ap_id": AP_ID, "profile": "nobody"}))
        .send(&app)
        .await;
    for response in [&form, &json_body] {
        common::assert_answer(response, StatusCode::BAD_REQUEST, "an unresolvable profile");
        assert_eq!(
            response.text(),
            COULD_NOT_FIND_USER,
            "the body Pleroma's controller answers: {}",
            response.text()
        );
    }
}

/// `GET /ostatus_subscribe?acct=…` renders the login form: HTML under a
/// framing-refusing policy, posting the escaped `acct` plus the nested
/// credentials to `POST /ostatus_subscribe`.
#[tokio::test]
async fn the_subscribe_form_escapes_its_acct() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let response =
        common::TestRequest::get("/ostatus_subscribe?acct=%3Cb%3Eevil%22%40remote.example")
            .send(&app)
            .await;

    common::assert_answer(&response, StatusCode::OK, "the subscribe form");
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "the page is HTML"
    );
    assert_eq!(
        response.header("x-frame-options"),
        Some("DENY"),
        "a login page is not framable"
    );
    let body = response.text();
    assert!(
        body.contains("action=\"/ostatus_subscribe\""),
        "the form posts to the subscribe route: {body}"
    );
    assert!(
        body.contains("name=\"acct\" value=\"&lt;b&gt;evil&quot;@remote.example\""),
        "the acct is escaped, not echoed raw: {body}"
    );
    assert!(
        !body.contains("value=\"<b>"),
        "no tag opens from the query: {body}"
    );
    assert!(
        body.contains("name=\"authorization[name]\"")
            && body.contains("name=\"authorization[password]\""),
        "the form carries the nested credentials: {body}"
    );
    assert!(
        body.contains("type=\"hidden\""),
        "acct rides as a hidden field"
    );
}

/// A form without an `acct` parameter still renders, with the hidden
/// field empty.
#[tokio::test]
async fn the_subscribe_form_renders_without_an_acct() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let response = common::TestRequest::get("/ostatus_subscribe")
        .send(&app)
        .await;

    common::assert_answer(&response, StatusCode::OK, "the bare subscribe form");
    let body = response.text();
    assert!(
        body.contains("name=\"acct\" value=\"\""),
        "an absent acct renders as an empty field: {body}"
    );
}

/// `POST /ostatus_subscribe` with the local user's credentials follows
/// the stored remote handle: a fresh pending row (the target's instance
/// answers later) and the success page naming the followed handle.
#[tokio::test]
async fn the_subscribe_post_follows_for_a_logged_in_user() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_user_with_password(&pool, "alice", "s3cret-hunter").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post("/ostatus_subscribe")
        .form(&[
            ("acct", "bob@remote.example"),
            ("authorization[name]", "alice"),
            ("authorization[password]", "s3cret-hunter"),
        ])
        .send(&app)
        .await;

    common::assert_answer(&response, StatusCode::OK, "the follow write");
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "the answer is the success page"
    );
    let body = response.text();
    assert!(
        body.contains("You are now following <b>bob@remote.example</b>."),
        "the page names the followed handle: {body}"
    );
    let relation = follow::get(&pool, alice.id, bob.id)
        .await
        .expect("the follow read")
        .expect("the subscribe write left a row");
    assert_eq!(
        relation.state,
        FollowState::Pending,
        "a remote follow waits for its instance's answer"
    );
}

/// The same write accepts the login by email too, and a password
/// containing spaces rides through the form verbatim.
#[tokio::test]
async fn the_subscribe_post_authenticates_by_email_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_user_with_password(&pool, "carol", "correct horse").await;
    let dave = common::create_remote_user(&pool, "dave", "remote.example").await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post("/ostatus_subscribe")
        .form(&[
            ("acct", "dave@remote.example"),
            ("authorization[name]", "carol@example.org"),
            ("authorization[password]", "correct horse"),
        ])
        .send(&app)
        .await;

    common::assert_answer(&response, StatusCode::OK, "the email login write");
    assert!(
        response.text().contains("You are now following"),
        "the fixture email authenticates: {}",
        response.text()
    );
    let relation = follow::get(&pool, carol.id, dave.id)
        .await
        .expect("the follow read")
        .expect("the email-login write left a row");
    assert_eq!(
        relation.state,
        FollowState::Pending,
        "pending like any other"
    );
}

/// A wrong (or absent) credential re-renders the form with Pleroma's
/// error line, the submitted `acct` kept, and no follow row written.
#[tokio::test]
async fn the_subscribe_post_refuses_a_bad_login() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _bob: User = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool.clone());

    let wrong = common::TestRequest::post("/ostatus_subscribe")
        .form(&[
            ("acct", "bob@remote.example"),
            ("authorization[name]", "alice"),
            ("authorization[password]", "not-her-password"),
        ])
        .send(&app)
        .await;
    let absent = common::TestRequest::post("/ostatus_subscribe")
        .form(&[("acct", "bob@remote.example")])
        .send(&app)
        .await;
    for response in [&wrong, &absent] {
        common::assert_answer(response, StatusCode::OK, "a refused login");
        let body = response.text();
        assert!(
            body.contains("<p class=\"error\">Wrong username or password</p>"),
            "the form re-renders with the error line: {body}"
        );
        assert!(
            body.contains("name=\"acct\" value=\"bob@remote.example\""),
            "the submitted acct is kept: {body}"
        );
    }
    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM follows")
        .fetch_one(&pool)
        .await
        .expect("the follows count");
    assert_eq!(rows.0, 0, "a refused login follows nobody");
}

/// A live login against a handle that resolves to nobody re-renders the
/// form with `Could not find user` (`ghost@invalid` needs no network:
/// the handle grammar fails first).
#[tokio::test]
async fn the_subscribe_post_refuses_an_unknown_acct() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice: User = common::create_user_with_password(&pool, "alice", "s3cret-hunter").await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post("/ostatus_subscribe")
        .form(&[
            ("acct", UNRESOLVABLE),
            ("authorization[name]", "alice"),
            ("authorization[password]", "s3cret-hunter"),
        ])
        .send(&app)
        .await;

    common::assert_answer(&response, StatusCode::OK, "an unknown acct");
    let body = response.text();
    assert!(
        body.contains("<p class=\"error\">Could not find user</p>"),
        "Pleroma's fetch error line: {body}"
    );
    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM follows")
        .fetch_one(&pool)
        .await
        .expect("the follows count");
    assert_eq!(rows.0, 0, "an unknown acct is followed by nobody");
}

/// Following yourself through the form is the model's own refusal,
/// rendered as its wording on the re-rendered form.
#[tokio::test]
async fn the_subscribe_post_refuses_a_self_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice: User = common::create_user_with_password(&pool, "alice", "s3cret-hunter").await;
    let app = common::test_app(pool);

    let response = common::TestRequest::post("/ostatus_subscribe")
        .form(&[
            ("acct", "alice"),
            ("authorization[name]", "alice"),
            ("authorization[password]", "s3cret-hunter"),
        ])
        .send(&app)
        .await;

    common::assert_answer(&response, StatusCode::OK, "the self follow");
    assert!(
        response
            .text()
            .contains("<p class=\"error\">Can not follow yourself</p>"),
        "the model's refusal wording: {}",
        response.text()
    );
}

/// `GET /authorize_interaction?uri=<local actor url>` 302s the browser
/// to the frontend profile page, `{FRONTEND_BASE_URL}/users/{id}` — the
/// configured frontend origin, not `BASE_URL`, is what the redirect
/// names.
#[tokio::test]
async fn authorize_interaction_redirects_an_actor_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mut config = common::test_config();
    config.frontend_base_url = "https://frontend.example".to_owned();
    let app = common::test_app_with_config(pool, config);

    let response = common::TestRequest::get(&format!(
        "/authorize_interaction?uri={}",
        urlencoding(AP_ID)
    ))
    .send(&app)
    .await;

    common::assert_answer(&response, StatusCode::FOUND, "the actor redirect");
    assert_eq!(
        response.header("location"),
        Some(format!("https://frontend.example/users/{}", alice.id).as_str()),
        "the frontend page for the resolved actor"
    );
}

/// A `user@domain` handle stored as a remote mirror resolves from the
/// database (no network), with its `@` prefix folded.
#[tokio::test]
async fn authorize_interaction_redirects_a_handle() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool);

    let response = common::TestRequest::get(&format!(
        "/authorize_interaction?uri={}",
        urlencoding("@bob@remote.example")
    ))
    .send(&app)
    .await;

    common::assert_answer(&response, StatusCode::FOUND, "the handle redirect");
    assert_eq!(
        response.header("location"),
        Some(format!("{BASE}/users/{}", bob.id).as_str()),
        "the default frontend origin is the instance origin"
    );
}

/// A status URL resolves through the object reader and 302s to
/// `{FRONTEND_BASE_URL}/notice/{id}`.
#[tokio::test]
async fn authorize_interaction_redirects_a_status_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "an interacted note", Visibility::Public).await;
    let app = common::test_app(pool);

    let response = common::TestRequest::get(&format!(
        "/authorize_interaction?uri={}",
        urlencoding(&note.uri)
    ))
    .send(&app)
    .await;

    common::assert_answer(&response, StatusCode::FOUND, "the status redirect");
    assert_eq!(
        response.header("location"),
        Some(format!("{BASE}/notice/{}", note.id).as_str()),
        "the frontend page for the resolved status"
    );
}

/// Whatever the `uri` cannot resolve to — a handle that fails the
/// grammar, an unknown actor URL on this instance's own origin, or no
/// `uri` at all — is the 404 JSON. None of these reach the network.
#[tokio::test]
async fn authorize_interaction_refuses_unresolvable_uris() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let handle = common::TestRequest::get(&format!(
        "/authorize_interaction?uri={}",
        urlencoding(UNRESOLVABLE)
    ))
    .send(&app)
    .await;
    let unknown_actor = common::TestRequest::get(&format!(
        "/authorize_interaction?uri={}",
        urlencoding("https://test.example/users/nobody")
    ))
    .send(&app)
    .await;
    let missing = common::TestRequest::get("/authorize_interaction")
        .send(&app)
        .await;
    for response in [&handle, &unknown_actor, &missing] {
        common::assert_answer(response, StatusCode::NOT_FOUND, "an unresolvable uri");
        assert_eq!(
            response.text(),
            NOT_FOUND,
            "the 404 is JSON: {}",
            response.text()
        );
    }
}

/// `GET /main/ostatus` renders the remote-subscribe form with both
/// nested fields, the query pair escaped into them.
#[tokio::test]
async fn the_main_ostatus_form_escapes_its_fields() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let response = common::TestRequest::get(
        "/main/ostatus?nickname=%3Ci%3Ealice&profile=%22evil%22%40remote.example",
    )
    .send(&app)
    .await;

    common::assert_answer(&response, StatusCode::OK, "the subscribe page");
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "the page is HTML"
    );
    let body = response.text();
    assert!(
        body.contains("action=\"/main/ostatus\""),
        "the form posts back to the same route: {body}"
    );
    assert!(
        body.contains("name=\"user[nickname]\" value=\"&lt;i&gt;alice\""),
        "the local account renders escaped: {body}"
    );
    assert!(
        body.contains("name=\"user[profile]\" value=\"&quot;evil&quot;@remote.example\""),
        "the remote handle renders escaped: {body}"
    );
    assert!(
        !body.contains("<i>alice"),
        "no tag from the query survives: {body}"
    );
}

/// `POST /main/ostatus` with a nickname that is no live local account —
/// unknown, or the remote mirror this page may not act as — re-renders
/// the form with `Could not find user`, both submitted values kept.
#[tokio::test]
async fn the_main_ostatus_post_refuses_a_missing_actor() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _bob: User = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool);

    let unknown = common::TestRequest::post("/main/ostatus")
        .form(&[
            ("user[nickname]", "nobody"),
            ("user[profile]", UNRESOLVABLE),
        ])
        .send(&app)
        .await;
    let mirror = common::TestRequest::post("/main/ostatus")
        .form(&[
            ("user[nickname]", "bob@remote.example"),
            ("user[profile]", UNRESOLVABLE),
        ])
        .send(&app)
        .await;
    let empty = common::TestRequest::post("/main/ostatus").send(&app).await;
    for response in [&unknown, &mirror, &empty] {
        common::assert_answer(response, StatusCode::OK, "a refused subscribe");
        let body = response.text();
        assert!(
            body.contains("<p class=\"error\">Could not find user</p>"),
            "the form says who was missing: {body}"
        );
    }
    assert!(
        unknown.text().contains("value=\"nobody\""),
        "the nickname is kept on the re-render: {}",
        unknown.text()
    );
}

/// `POST /main/ostatus` with a real local account but a profile that
/// resolves to no subscribe answer re-renders the form with the
/// catch-all error line (the handle grammar fails `WebFinger` before any
/// request, so this stays network-free).
#[tokio::test]
async fn the_main_ostatus_post_refuses_an_unresolvable_profile() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice: User = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);

    let response = common::TestRequest::post("/main/ostatus")
        .form(&[("user[nickname]", "alice"), ("user[profile]", UNRESOLVABLE)])
        .send(&app)
        .await;

    common::assert_answer(&response, StatusCode::OK, "the profile refusal");
    let body = response.text();
    assert!(
        body.contains("<p class=\"error\">Something went wrong.</p>"),
        "Pleroma's catch-all line: {body}"
    );
    assert!(
        body.contains("value=\"alice\"") && body.contains(&format!("value=\"{UNRESOLVABLE}\"")),
        "both values are kept on the re-render: {body}"
    );
}

/// The `path` as a percent-encoded query value, so fixture URLs and
/// handles survive the query string intact.
fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
