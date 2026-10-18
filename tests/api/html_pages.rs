//! Tests for the HTML helper pages of `src/api/html_pages.rs` (WBE-220):
//! the `GET`/`POST` pair on `/frontend_switcher`, the pick-list shape and
//! its empty case, the cookie-carrying redirect, the 422 refusals and the
//! module headers every answer carries. Later tickets append their embed,
//! player and unsubscribe coverage to this file.
//!
//! The endpoints read [`Config::available_frontends`] only, never the
//! database; they still lease a slot so the full router can be mounted.
//!
//! [`Config::available_frontends`]: wired_be::config::Config::available_frontends

#![cfg(test)]

use axum::{Router, http::StatusCode};
use sqlx::PgPool;

use crate::common;

/// The exact CSP every page in the module must carry.
const PAGE_CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; img-src * data:; media-src *";
/// The media type every page in the module must answer with.
const PAGE_CONTENT_TYPE: &str = "text/html; charset=utf-8";

/// The real router with `AVAILABLE_FRONTENDS` set to `frontends`, over
/// `pool`.
fn switcher_app(pool: PgPool, frontends: &[&str]) -> Router {
    let mut config = common::test_config();
    config.available_frontends = frontends.iter().map(|name| (*name).to_owned()).collect();
    common::test_app_with_config(pool, config)
}

/// The module headers `response` must carry: HTML media type, the CSP and
/// `X-Frame-Options: DENY` (every page here is a protected one).
fn assert_module_headers(response: &common::TestResponse, who: &str) {
    assert_eq!(
        response.header("content-type"),
        Some(PAGE_CONTENT_TYPE),
        "{who} answers as HTML"
    );
    assert_eq!(
        response.header("content-security-policy"),
        Some(PAGE_CSP),
        "{who} carries the module CSP"
    );
    assert_eq!(
        response.header("x-frame-options"),
        Some("DENY"),
        "{who} cannot be framed"
    );
}

/// `GET /frontend_switcher` renders one labelled radio per configured
/// frontend, in configuration order, inside a form posting back to the
/// switcher, with the module headers on the page.
#[tokio::test]
async fn the_switcher_page_lists_each_configured_frontend() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = switcher_app(pool, &["pleroma-fe", "mochi-pub"]);
    let response = common::TestRequest::get("/frontend_switcher")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the pick-list page");
    assert_module_headers(&response, "the GET page");
    let html = response.text();
    assert!(
        html.starts_with("<!DOCTYPE html>"),
        "a full HTML document: {html}"
    );
    assert!(
        html.contains("<form method=\"post\" action=\"/frontend_switcher\">"),
        "the form posts back to the switcher: {html}"
    );
    assert_eq!(
        html.matches("type=\"radio\" name=\"frontend\"").count(),
        2,
        "one labelled radio per configured frontend: {html}"
    );
    let first = html
        .find("value=\"pleroma-fe\"")
        .expect("the first frontend is listed");
    let second = html
        .find("value=\"mochi-pub\"")
        .expect("the second frontend is listed");
    assert!(first < second, "the radios follow the configuration order");
    assert!(
        html.contains(">mochi-pub</label>"),
        "each radio is labelled with the frontend's own name"
    );
}

/// With `AVAILABLE_FRONTENDS` empty (the test default), the page says no
/// alternative frontends are installed and offers no form and no radios.
#[tokio::test]
async fn the_switcher_page_offers_nothing_when_nothing_is_installed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/frontend_switcher")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "an empty list is still a page"
    );
    assert_module_headers(&response, "the empty GET page");
    let html = response.text();
    assert!(
        html.contains("No alternative frontends are installed"),
        "the page says so: {html}"
    );
    assert!(!html.contains("<form"), "there is no form to fill: {html}");
    assert!(
        !html.contains("type=\"radio\""),
        "and no radio to pick: {html}"
    );
}

/// A frontend name containing markup arrives escaped on the page, in its
/// attribute and in its label.
#[tokio::test]
async fn frontend_names_on_the_page_are_html_escaped() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = switcher_app(pool, &[r#"<script>alert("xss")</script>"#]);
    let response = common::TestRequest::get("/frontend_switcher")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a hostile name is still a page"
    );
    let html = response.text();
    assert!(
        !html.contains("<script>"),
        "the name cannot open a tag: {html}"
    );
    assert!(
        html.contains("value=\"&lt;script&gt;alert(&quot;xss&quot;)&lt;/script&gt;\""),
        "the attribute value is escaped: {html}"
    );
    assert!(
        html.contains(">&lt;script&gt;alert(&quot;xss&quot;)&lt;/script&gt;</label>"),
        "the label text is escaped too: {html}"
    );
}

/// `POST /frontend_switcher` with a pick on the list answers 302 to `/`
/// with the year-long `preferred_frontend` cookie and the module headers
/// on the redirect.
#[tokio::test]
async fn a_valid_pick_sets_the_cookie_and_redirects_home() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = switcher_app(pool, &["pleroma-fe", "mochi-pub"]);
    let response = common::TestRequest::post("/frontend_switcher")
        .form(&[("frontend", "mochi-pub")])
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::FOUND, "the pick redirects");
    assert_eq!(
        response.header("location"),
        Some("/"),
        "to the instance root"
    );
    assert_eq!(
        response.header("set-cookie"),
        Some("preferred_frontend=mochi-pub; Path=/; Max-Age=31536000; SameSite=Lax"),
        "the year-long frontend cookie, exactly"
    );
    assert_module_headers(&response, "the redirect");
}

/// A name that is not on the configured list is a 422 `Invalid frontend`
/// HTML page, headers included.
#[tokio::test]
async fn a_pick_off_the_list_is_refused_with_an_html_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = switcher_app(pool, &["pleroma-fe"]);
    let response = common::TestRequest::post("/frontend_switcher")
        .form(&[("frontend", "not-installed")])
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unlisted pick is 422"
    );
    assert_module_headers(&response, "the refused pick");
    assert!(
        response.text().contains("Invalid frontend"),
        "the page names the failure: {}",
        response.text()
    );
}

/// A POST without a `frontend` field at all is the same 422 HTML page.
#[tokio::test]
async fn a_missing_pick_is_refused_with_the_same_html_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = switcher_app(pool, &["pleroma-fe"]);
    let response = common::TestRequest::post("/frontend_switcher")
        .form(&[])
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no pick is 422"
    );
    assert_module_headers(&response, "the missing pick");
    assert!(
        response.text().contains("Invalid frontend"),
        "the page names the failure: {}",
        response.text()
    );
}
