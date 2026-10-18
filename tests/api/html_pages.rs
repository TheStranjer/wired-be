//! Tests for the HTML helper pages of `src/api/html_pages.rs` (WBE-220):
//! the `GET`/`POST` pair on `/frontend_switcher`, the pick-list shape and
//! its empty case, the cookie-carrying redirect, the 422 refusals, the
//! `GET /embed/{id}` status card (public and unlisted statuses, remote
//! handles, media attachments, escaping, and the one 404 page every
//! refusal shares) and the module headers every answer carries. Later
//! tickets append their player and unsubscribe coverage to this file.
//!
//! The switcher endpoints read [`Config::available_frontends`] only, never
//! the database; the embed card reads statuses, authors and attachments.
//! Every test leases a slot so the full router can be mounted.
//!
//! [`Config::available_frontends`]: wired_be::config::Config::available_frontends

#![cfg(test)]

use axum::{Router, http::StatusCode};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    status::{self, NewStatus, Visibility},
    user::NewUser,
};

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

/// The headers of an embed answer: HTML media type, the CSP and no
/// `X-Frame-Options` — the card must be framable, and so must the 404
/// that hides it, or the status code alone would tell the cases apart.
fn assert_embed_headers(response: &common::TestResponse, who: &str) {
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
        None,
        "{who} can be framed"
    );
}

/// `GET /embed/{id}` over the mounted router.
async fn get_embed(app: &Router, id: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/embed/{id}"))
        .send(app)
        .await
}

/// `GET /embed/{id}` for a public status: the author's display name, the
/// `@`handle linked to the stored profile url, the sanitized content
/// verbatim, and a `mastodon_time` date — as a framable HTML page.
#[tokio::test]
async fn the_embed_page_shows_the_status_card() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user_with(&pool, "embedalice", |user| NewUser {
        name: "Alice Example".to_owned(),
        profile_url: Some("https://test.example/@embedalice".to_owned()),
        ..user
    })
    .await;
    let posted = common::create_status(&pool, &author, "Hello & friends", Visibility::Public).await;
    let response = get_embed(&app, &posted.id.to_string()).await;
    assert_eq!(response.status, StatusCode::OK, "a public status embeds");
    assert_embed_headers(&response, "the card");
    let html = response.text();
    assert!(
        html.starts_with("<!DOCTYPE html>"),
        "a full HTML document: {html}"
    );
    assert!(html.contains("Alice Example"), "the display name: {html}");
    assert!(html.contains("@embedalice"), "the handle: {html}");
    assert!(
        html.contains("href=\"https://test.example/@embedalice\""),
        "the author line links to the stored profile url: {html}"
    );
    assert!(
        html.contains(&posted.content),
        "the sanitized content rides through verbatim: {html}"
    );
    assert!(html.contains("<p>"), "as markup, not escaped text: {html}");
    assert!(
        !html.contains("&lt;p&gt;"),
        "the content is not escaped: {html}"
    );
    assert!(
        html.contains("<time datetime=\"20"),
        "the date stamp is a mastodon_time: {html}"
    );
}

/// An unlisted status renders too: the ticket widens Pleroma's
/// public-only embed to it.
#[tokio::test]
async fn the_embed_page_renders_an_unlisted_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "embeduna").await;
    let posted =
        common::create_status(&pool, &author, "quietly posted", Visibility::Unlisted).await;
    let response = get_embed(&app, &posted.id.to_string()).await;
    assert_eq!(response.status, StatusCode::OK, "unlisted embeds");
    assert_embed_headers(&response, "the unlisted card");
    assert!(
        response.text().contains(&posted.content),
        "the card carries the post: {}",
        response.text()
    );
}

/// A private and a direct status are not embeddable: each answers the
/// shared 404 page, and the page tells nothing of the status it hides.
#[tokio::test]
async fn private_and_direct_statuses_answer_the_not_found_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "embedpriv").await;
    let private =
        common::create_status(&pool, &author, "followers only text", Visibility::Private).await;
    let direct =
        common::create_status(&pool, &author, "mentionees only text", Visibility::Direct).await;
    for (id, who) in [
        (&private.id.to_string(), "a private status"),
        (&direct.id.to_string(), "a direct status"),
    ] {
        let response = get_embed(&app, id).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{who} is refused");
        assert_embed_headers(&response, who);
        let html = response.text();
        assert!(
            html.contains("Not found"),
            "{who} says only Not found: {html}"
        );
        assert!(
            !html.contains("only text"),
            "{who} leaks nothing of the post: {html}"
        );
    }
}

/// An unknown id, a non-UUID id and a soft-deleted (tombstoned) status
/// all answer the byte-identical 404 page, so the route never says which
/// of them it was.
#[tokio::test]
async fn unknown_malformed_and_deleted_ids_answer_the_same_not_found_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "embedgone").await;
    let posted =
        common::create_status(&pool, &author, "was here, now gone", Visibility::Public).await;
    let _tombstone = status::soft_delete(&pool, posted.id)
        .await
        .expect("the soft delete works");
    let unknown = Uuid::now_v7().to_string();
    let deleted = posted.id.to_string();
    let mut pages = Vec::new();
    for (id, who) in [
        (unknown.as_str(), "an unknown id"),
        ("not-a-uuid", "a malformed id"),
        (deleted.as_str(), "a deleted status"),
    ] {
        let response = get_embed(&app, id).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{who} is 404");
        assert_embed_headers(&response, who);
        let html = response.text();
        assert!(
            html.starts_with("<!DOCTYPE html>"),
            "{who} gets a full HTML page: {html}"
        );
        assert!(
            html.contains("<title>Not found</title>") && html.contains("<h1>Not found</h1>"),
            "{who} gets the Not found page: {html}"
        );
        assert!(!html.contains("was here"), "{who} leaks nothing: {html}");
        pages.push(html);
    }
    pages.dedup();
    assert_eq!(
        pages.len(),
        1,
        "the three refusals are one and the same page"
    );
}

/// A remote author's card renders the stored full nickname as the handle
/// and falls back to the actor id for the profile link.
#[tokio::test]
async fn a_remote_authors_handle_renders_as_stored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_remote_user(&pool, "eve", "remote.example").await;
    let posted =
        common::create_status(&pool, &author, "posted elsewhere", Visibility::Public).await;
    let response = get_embed(&app, &posted.id.to_string()).await;
    assert_eq!(response.status, StatusCode::OK, "a remote post embeds");
    let html = response.text();
    assert!(
        html.contains("@eve@remote.example"),
        "the full stored nickname is the handle: {html}"
    );
    assert!(
        html.contains("href=\"https://remote.example/users/eve\""),
        "with no stored profile_url the link is the actor id: {html}"
    );
}

/// Image, video and audio attachments become `img`, `video controls` and
/// `audio controls` elements in `position` order; an attachment of
/// neither kind renders nothing.
#[tokio::test]
async fn attachments_render_as_media_elements_in_position_order() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "embedmedia").await;
    let posted =
        common::create_status(&pool, &author, "a post with media", Visibility::Public).await;
    let _image =
        common::create_status_attachment(&pool, &posted, "image/png", "/media/one.png", 0).await;
    let _video =
        common::create_status_attachment(&pool, &posted, "video/mp4", "/media/two.mp4", 1).await;
    let _audio =
        common::create_status_attachment(&pool, &posted, "audio/mpeg", "/media/three.mp3", 2).await;
    let _other =
        common::create_status_attachment(&pool, &posted, "application/pdf", "/media/four.pdf", 3)
            .await;
    let response = get_embed(&app, &posted.id.to_string()).await;
    assert_eq!(response.status, StatusCode::OK, "the media card");
    let html = response.text();
    assert!(
        html.contains("<img src=\"/media/one.png\" alt=\"\">"),
        "the image is an img element: {html}"
    );
    assert!(
        html.contains("<video controls src=\"/media/two.mp4\" title=\"\"></video>"),
        "the video is a controls video: {html}"
    );
    assert!(
        html.contains("<audio controls src=\"/media/three.mp3\" title=\"\"></audio>"),
        "the audio is a controls audio: {html}"
    );
    assert!(
        !html.contains("four.pdf"),
        "a non-media attachment renders nothing: {html}"
    );
    let image = html.find("<img").expect("the image rendered");
    let video = html.find("<video").expect("the video rendered");
    let audio = html.find("<audio").expect("the audio rendered");
    assert!(image < video && video < audio, "position order: {html}");
}

/// A display name carrying script tags and quotes cannot open a tag on
/// the card, while a stored, already-sanitized content passes through
/// unescaped.
#[tokio::test]
async fn a_hostile_author_field_is_escaped_and_content_passes_through() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user_with(&pool, "embedxss", |user| NewUser {
        name: r#"<script>alert("xss")</script>"#.to_owned(),
        ..user
    })
    .await;
    let new = NewStatus {
        content: "<p>a &amp; b <i>c</i></p>".to_owned(),
        ..NewStatus::local(&author, "scripted text")
    };
    let posted = status::insert(&pool, &common::test_config().base_url, &new)
        .await
        .expect("the crafted status inserts");
    let response = get_embed(&app, &posted.id.to_string()).await;
    assert_eq!(response.status, StatusCode::OK, "the hostile card renders");
    let html = response.text();
    assert!(
        !html.contains("<script"),
        "no tag opens anywhere on the page: {html}"
    );
    assert!(
        html.contains("&lt;script&gt;alert(&quot;xss&quot;)&lt;/script&gt;"),
        "the display name is escaped into text: {html}"
    );
    assert!(
        html.contains("<p>a &amp; b <i>c</i></p>"),
        "the sanitized content is verbatim: {html}"
    );
    assert!(
        !html.contains("&amp;amp;"),
        "and never escaped a second time: {html}"
    );
}
