//! Tests for the HTML helper pages of `src/api/html_pages.rs` (WBE-220):
//! the retired `GET`/`POST` pair on `/frontend_switcher` (now answered by
//! the fallback alone), the
//! `GET /embed/{id}` status card (public and unlisted statuses, remote
//! handles, media attachments, escaping, and the one 404 page every
//! refusal shares), the `GET /notice/{id}/embed_player` bare media player
//! (its verbatim url match, the refusals that all share the 404 page, and
//! its framable headers), the `GET /mailer/unsubscribe/{token}` page
//! (HMAC-verified tokens, the one 400 `Invalid token` refusal, and the
//! flag it clears in the database) and the module headers every answer
//! carries.
//!
//! The embed card and the player read statuses, authors and
//! attachments; the unsubscribe page signs against
//! [`Config::secret_key_base`] and writes the `email_notifications` flag.
//! Every test leases a slot so the full router can be mounted.
//!
//! [`Config::secret_key_base`]: wired_be::config::Config::secret_key_base

#![cfg(test)]

use axum::{Router, http::StatusCode};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    api::html_pages::unsubscribe_token,
    models::{
        status::{self, NewStatus, Status, Visibility},
        user::{self, NewUser},
    },
};

use crate::common;

/// The exact CSP every page in the module must carry.
const PAGE_CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; img-src * data:; media-src *";
/// The media type every page in the module must answer with.
const PAGE_CONTENT_TYPE: &str = "text/html; charset=utf-8";

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

/// `GET /frontend_switcher` is not routed: the app-wide fallback answers
/// 404 and marks it with `x-wired-fallback: 1`.
#[tokio::test]
async fn get_frontend_switcher_is_answered_by_the_fallback() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/frontend_switcher")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "the switcher page is gone"
    );
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "only the fallback answers the path"
    );
}

/// `POST /frontend_switcher` is not routed either: 404 from the fallback.
#[tokio::test]
async fn post_frontend_switcher_is_answered_by_the_fallback() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::post("/frontend_switcher")
        .form(&[("frontend", "pleroma-fe")])
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "the switcher pick is gone"
    );
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "only the fallback answers the path"
    );
    assert_eq!(
        response.header("set-cookie"),
        None,
        "no frontend cookie is set"
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

/// `GET /notice/{id}/embed_player{query}` over the mounted router.
async fn get_player(app: &Router, id: &str, query: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/notice/{id}/embed_player{query}"))
        .send(app)
        .await
}

/// A public status authored by a fresh local user named `nickname`,
/// carrying one image, one video, one audio and one non-media
/// attachment — everything the player must refuse or accept between.
async fn media_status(pool: &PgPool, nickname: &str) -> Status {
    let author = common::create_local_user(pool, nickname).await;
    let posted =
        common::create_status(pool, &author, "a post with media", Visibility::Public).await;
    let _image =
        common::create_status_attachment(pool, &posted, "image/png", "/media/one.png", 0).await;
    let _video =
        common::create_status_attachment(pool, &posted, "video/mp4", "/media/two.mp4", 1).await;
    let _audio =
        common::create_status_attachment(pool, &posted, "audio/mpeg", "/media/three.mp3", 2).await;
    let _other =
        common::create_status_attachment(pool, &posted, "application/pdf", "/media/four.pdf", 3)
            .await;
    posted
}

/// The player of a video attachment's url is just the `video controls`
/// element inside `<main class="player">`, framed by the embed headers:
/// no `X-Frame-Options`, the CSP and HTML.
#[tokio::test]
async fn the_player_page_streams_the_matching_video_attachment() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let posted = media_status(&pool, "playeralice").await;
    let response = get_player(&app, &posted.id.to_string(), "?url=/media/two.mp4").await;
    assert_eq!(response.status, StatusCode::OK, "the video plays");
    assert_embed_headers(&response, "the player");
    let html = response.text();
    assert!(
        html.starts_with("<!DOCTYPE html>"),
        "a full HTML document: {html}"
    );
    assert!(
        html.contains(
            "<main class=\"player\"><video controls src=\"/media/two.mp4\" title=\"\"></video></main>"
        ),
        "just the video element inside <main class=\"player\">: {html}"
    );
    assert!(!html.contains("<img"), "the page carries no card: {html}");
    assert!(
        !html.contains("a post with media"),
        "and none of the status content: {html}"
    );
}

/// The player of an audio attachment's url renders the `audio controls`
/// element.
#[tokio::test]
async fn the_player_page_streams_the_matching_audio_attachment() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let posted = media_status(&pool, "playeraudio").await;
    let response = get_player(&app, &posted.id.to_string(), "?url=/media/three.mp3").await;
    assert_eq!(response.status, StatusCode::OK, "the audio plays");
    assert_embed_headers(&response, "the audio player");
    assert!(
        response.text().contains(
            "<main class=\"player\"><audio controls src=\"/media/three.mp3\" title=\"\"></audio></main>"
        ),
        "just the audio element inside <main class=\"player\">: {}",
        response.text()
    );
}

/// An image match, a non-media content type, a url no attachment of the
/// status carries, a url belonging to another status, no `url` at all and
/// an empty `url` all answer the shared 404 `Not found` page.
#[tokio::test]
async fn the_player_refuses_every_url_that_is_no_video_or_audio() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let posted = media_status(&pool, "playerrefuse").await;
    let other_author = common::create_local_user(&pool, "playerother").await;
    let other = common::create_status(&pool, &other_author, "elsewhere", Visibility::Public).await;
    let _video =
        common::create_status_attachment(&pool, &other, "video/mp4", "/media/five.mp4", 0).await;
    let id = posted.id.to_string();
    for (query, who) in [
        ("?url=/media/one.png", "an image match"),
        ("?url=/media/four.pdf", "a non-media content type"),
        ("?url=/media/nowhere.mp4", "a url no attachment carries"),
        ("?url=/media/five.mp4", "a url belonging to another status"),
        ("", "no url parameter at all"),
        ("?url=", "an empty url parameter"),
    ] {
        let response = get_player(&app, &id, query).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{who} is 404");
        assert_embed_headers(&response, who);
        let html = response.text();
        assert!(
            html.contains("<title>Not found</title>") && html.contains("<h1>Not found</h1>"),
            "{who} gets the shared Not found page: {html}"
        );
    }
}

/// A private status, an unknown id and a malformed id all answer the
/// same 404 page even when the `url` names one of the status's videos.
#[tokio::test]
async fn the_player_refuses_statuses_that_are_not_embeddable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "playerpriv").await;
    let private =
        common::create_status(&pool, &author, "followers only film", Visibility::Private).await;
    let _video =
        common::create_status_attachment(&pool, &private, "video/mp4", "/media/secret.mp4", 0)
            .await;
    let unknown = Uuid::now_v7().to_string();
    for (id, who) in [
        (private.id.to_string(), "a private status"),
        (unknown, "an unknown id"),
        ("not-a-uuid".to_owned(), "a malformed id"),
    ] {
        let response = get_player(&app, &id, "?url=/media/secret.mp4").await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{who} is 404");
        assert_embed_headers(&response, who);
        let html = response.text();
        assert!(
            html.contains("<h1>Not found</h1>"),
            "{who} gets the shared Not found page: {html}"
        );
        assert!(
            !html.contains("secret.mp4"),
            "{who} leaks nothing of the film: {html}"
        );
    }
}

/// A url containing quotes is matched only verbatim — the double-encoded
/// spelling is a different string and gets the 404 — and is escaped on
/// output, where a raw quote could close the `src` attribute.
#[tokio::test]
async fn a_url_with_quotes_matches_verbatim_and_is_escaped_on_output() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "playerquote").await;
    let posted = common::create_status(&pool, &author, "quoted film", Visibility::Public).await;
    let stored = r#"/media/a"b&c.mp4"#;
    let _video = common::create_status_attachment(&pool, &posted, "video/mp4", stored, 0).await;
    let id = posted.id.to_string();
    let response = get_player(&app, &id, "?url=/media/a%22b%26c.mp4").await;
    assert_eq!(response.status, StatusCode::OK, "the verbatim match plays");
    let html = response.text();
    assert!(
        html.contains("src=\"/media/a&quot;b&amp;c.mp4\""),
        "the url arrives escaped: {html}"
    );
    assert!(
        !html.contains("src=\"/media/a\""),
        "a raw quote cannot close the attribute: {html}"
    );
    let refused = get_player(&app, &id, "?url=/media/a%2522b%2526c.mp4").await;
    assert_eq!(
        refused.status,
        StatusCode::NOT_FOUND,
        "the double-encoded spelling is a different url"
    );
}

/// The `SECRET_KEY_BASE` value the mailer tests sign with; never a real
/// secret.
const MAILER_SECRET: &str = "a-mailer-signing-secret-for-tests";

/// The real router with `Config::secret_key_base` set to `secret` (or
/// left unset for `None`), over `pool`.
fn mailer_app(pool: PgPool, secret: Option<&str>) -> Router {
    let mut config = common::test_config();
    config.secret_key_base = secret.map(str::to_owned);
    common::test_app_with_config(pool, config)
}

/// `GET /mailer/unsubscribe/{token}` over the mounted router.
async fn get_unsubscribe(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/mailer/unsubscribe/{token}"))
        .send(app)
        .await
}

/// `user`'s stored `email_notifications` flag.
async fn notifications_on(pool: &PgPool, user_id: Uuid) -> bool {
    let found = user::get_by_id(pool, user_id)
        .await
        .expect("the user lookup runs");
    found
        .expect("the user row is there")
        .email_notifications
        .get()
}

/// `token` with its first base64url character swapped for another (both
/// fully significant, so the decoded signature must change).
fn flipped_signature(token: &str) -> String {
    let mut chars = token.chars();
    let first = chars.next().expect("a minted token is not empty");
    let swap = if first == 'A' { 'B' } else { 'A' };
    let mut flipped = String::new();
    flipped.push(swap);
    flipped.extend(chars);
    flipped
}

/// A token minted with the configured secret answers 200
/// `You have been unsubscribed` as a protected page (`X-Frame-Options:
/// DENY`, unlike the embed pages) and leaves `email_notifications` false
/// in the database.
#[tokio::test]
async fn a_valid_unsubscribe_token_clears_the_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mailer_app(pool.clone(), Some(MAILER_SECRET));
    let subscriber = common::create_local_user(&pool, "unsubalice").await;
    assert!(
        notifications_on(&pool, subscriber.id).await,
        "the fixture starts subscribed"
    );
    let token = unsubscribe_token(MAILER_SECRET, subscriber.id);
    let response = get_unsubscribe(&app, &token).await;
    assert_eq!(response.status, StatusCode::OK, "a good token unsubscribes");
    assert_module_headers(&response, "the unsubscribe page");
    let html = response.text();
    assert!(
        html.contains("<title>You have been unsubscribed</title>")
            && html.contains("<h1>You have been unsubscribed</h1>"),
        "the page says so: {html}"
    );
    assert!(
        !notifications_on(&pool, subscriber.id).await,
        "the flag is stored cleared"
    );
}

/// A flipped signature byte, a token whose id segment names another
/// user, one signed with a different secret, one with no dot and one
/// whose signature segment is no base64url all answer the 400
/// `Invalid token` page and change no row.
#[tokio::test]
async fn every_bad_unsubscribe_token_answers_the_invalid_page_and_changes_no_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mailer_app(pool.clone(), Some(MAILER_SECRET));
    let alice = common::create_local_user(&pool, "unsubbob").await;
    let bob = common::create_local_user(&pool, "unsubcarol").await;
    let token = unsubscribe_token(MAILER_SECRET, alice.id);
    let (signature, id_text) = token.split_once('.').expect("a minted token has a dot");
    for (bad_token, who) in [
        (flipped_signature(&token), "a flipped signature byte"),
        (
            format!("{signature}.{}", bob.id),
            "an id segment swapped for another user",
        ),
        (
            unsubscribe_token("a-different-secret", alice.id),
            "a token signed with a different secret",
        ),
        (signature.to_owned(), "a token with no dot"),
        (
            format!("!!!.{id_text}"),
            "a signature segment that is no base64url",
        ),
    ] {
        let response = get_unsubscribe(&app, &bad_token).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{who} is 400");
        assert_module_headers(&response, who);
        assert!(
            response.text().contains("Invalid token"),
            "{who} says Invalid token: {}",
            response.text()
        );
    }
    assert!(
        notifications_on(&pool, alice.id).await,
        "no refusal touched alice's row"
    );
    assert!(
        notifications_on(&pool, bob.id).await,
        "and none touched bob's"
    );
}

/// With `secret_key_base` unset no token can verify: even one freshly
/// minted with the secret the tests otherwise use answers 400
/// `Invalid token` and changes no row.
#[tokio::test]
async fn without_a_signing_secret_every_unsubscribe_token_is_invalid() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mailer_app(pool.clone(), None);
    let subscriber = common::create_local_user(&pool, "unsubdave").await;
    let token = unsubscribe_token(MAILER_SECRET, subscriber.id);
    let response = get_unsubscribe(&app, &token).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "an un-mintable config is 400"
    );
    assert_module_headers(&response, "the unconfigured token");
    assert!(
        response.text().contains("Invalid token"),
        "the page says so: {}",
        response.text()
    );
    assert!(
        notifications_on(&pool, subscriber.id).await,
        "the row is untouched"
    );
}
