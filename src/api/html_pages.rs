//! Top-level HTML helper pages: the small standalone documents this
//! server serves outside `/api`.
//!
//! This module ships all four of its pages: the frontend switcher, the
//! embeddable status card, the media player for one attachment of a
//! status (`GET /notice/{id}/embed_player`, Pleroma's `notice_player` in
//! `lib/pleroma/web/o_status/o_status_controller.ex`) and the mailer
//! unsubscribe page (`GET /mailer/unsubscribe/{token}`, Pleroma's
//! `SubscriptionController` in
//! `lib/pleroma/web/mailer/subscription_controller.ex`), all built from
//! the same contract and out of the shared page helpers here.
//!
//! The contract every response this module answers keeps:
//!
//! - `content-type: text/html; charset=utf-8`.
//! - Every dynamic value — a frontend name, a message, a title — goes
//!   through [`escape_html`], so nothing a request or a config carries can
//!   open a tag or an attribute.
//! - `content-security-policy: default-src 'none'; style-src
//!   'unsafe-inline'; img-src * data:; media-src *` — Pleroma's switcher
//!   CSP, spelled by `PAGE_CSP`: the pages load no scripts, no external
//!   CSS and no fonts, only the one inline `PAGE_STYLE` sheet, images and
//!   media.
//! - `x-frame-options: DENY` on every page that is not made for embedding;
//!   embed pages omit the header so they can be iframed (see `Framing`).
//!
//! The reference for the endpoint pair is Pleroma's
//! `FrontendSwitcherController`
//! (`lib/pleroma/web/frontend_switcher/frontend_switcher_controller.ex`):
//! `index` renders the pick-list of the configured frontends, `update`
//! validates the pick, sets the year-long `preferred_frontend` cookie (the
//! shape [`crate::api::pleroma_settings`] mirrors) and sends the browser
//! back to `/`.
//!
//! This module is a tree. `switcher` holds the switcher form, handlers
//! and body builders, `embed` the status card and the media elements the
//! player shares, `player` the media player page and `unsubscribe` the
//! mailer link and its HMAC tokens; this root keeps the router, the
//! shared constants and header statics, `Framing` and the page helpers
//! every child answers through. `test_support` holds the unit-test
//! fixtures the children share.

use axum::{
    Router,
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use hmac::Hmac;
use sha2_011::Sha256;

use crate::{state::AppState, util::sanitize::escape_html};

use self::{
    embed::embed,
    player::embed_player,
    switcher::{switcher_index, switcher_update},
    unsubscribe::unsubscribe,
};

pub use self::unsubscribe::{unsubscribe_token, unsubscribe_user_id};

mod embed;
mod player;
mod switcher;
#[cfg(test)]
mod test_support;
mod unsubscribe;

/// The one CSP every page in this module sends, Pleroma's
/// `FrontendSwitcherController` `@csp`: no scripts, inline CSS only, any
/// image or media source.
const PAGE_CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; img-src * data:; media-src *";
/// The media type of every page: [`page`] renders full HTML documents.
const PAGE_CONTENT_TYPE: &str = "text/html; charset=utf-8";
/// The one inline stylesheet [`page`] embeds: [`PAGE_CSP`] allows no other
/// CSS, so every page in the module shares exactly this sheet.
const PAGE_STYLE: &str = "body{margin:0;min-height:100vh;display:flex;\
    align-items:center;justify-content:center;background:#f5f4ef;\
    font-family:system-ui,sans-serif;color:#28242d}\
    main{background:#fff;border-radius:8px;padding:2rem;max-width:30rem;\
    box-shadow:0 2px 10px rgba(40,36,45,0.12)}\
    h1{font-size:1.3rem;margin-top:0}\
    div.frontend{margin:0.75rem 0}\
    div.frontend label{display:block}\
    input[type=submit]{padding:0.5rem 1rem;border-radius:4px;\
    border:1px solid #7c8a52;background:#7c8a52;color:#fff;cursor:pointer}";
/// The `Content-Security-Policy` header [`PAGE_CSP`] rides on.
static CSP_HEADER: HeaderName = HeaderName::from_static("content-security-policy");
/// The `X-Frame-Options` header [`Framing::Deny`] puts up.
static FRAME_OPTIONS_HEADER: HeaderName = HeaderName::from_static("x-frame-options");
/// The path of the switcher, `GET` to list and `POST` to pick.
const SWITCHER_PATH: &str = "/frontend_switcher";
/// The title of the switcher page.
const SWITCHER_TITLE: &str = "Frontend switcher";
/// The name the radio inputs — and the POST — carry the pick under
/// (Pleroma's `%{"frontend" => name}`).
const FRONTEND_FIELD: &str = "frontend";
/// The body of the switcher page when [`Config::available_frontends`] is
/// empty: nothing to pick, so nothing is offered.
///
/// [`Config::available_frontends`]: crate::config::Config::available_frontends
const NO_FRONTENDS_MESSAGE: &str = "No alternative frontends are installed";
/// The 422 page title and body for a pick off the list (or no pick at all),
/// the message the ticket fixes.
const INVALID_FRONTEND: &str = "Invalid frontend";
/// The path a valid pick is sent to: the instance root, where the frontend
/// the cookie names takes over.
const REDIRECT_PATH: &str = "/";
/// The body of the redirect page; browsers follow the `Location` and never
/// show it.
const SWITCHING_MESSAGE: &str = "Switching frontend";
/// Name of the cookie a valid pick sets (the one
/// [`crate::api::pleroma_settings`] mirrors for stored choices).
const FRONTEND_COOKIE: &str = "preferred_frontend";
/// `Max-Age` of that cookie, in seconds: one year, the shape
/// [`crate::api::pleroma_settings`] uses.
const COOKIE_MAX_AGE: &str = "31536000";
/// The path of the embed card, `{id}` the status's primary key.
const EMBED_PATH: &str = "/embed/{id}";
/// The title of the embed card page.
const EMBED_TITLE: &str = "Status embed";
/// The title and body of the one 404 page the embed route answers every
/// refusal with: an unknown, deleted or unembeddable id is told no
/// differently than a malformed one, so the page never leaks which it was.
const NOT_FOUND_MESSAGE: &str = "Not found";
/// The path of the player, `{id}` the status's primary key, the
/// attachment to play named by the `url` query parameter.
const PLAYER_PATH: &str = "/notice/{id}/embed_player";
/// The title of the player page.
const PLAYER_TITLE: &str = "Media player";
/// The path of the mailer unsubscribe page, `{token}` the link token as
/// [`unsubscribe_token`] mints it.
const UNSUBSCRIBE_PATH: &str = "/mailer/unsubscribe/{token}";
/// The title and body of the page a verified token answers with.
const UNSUBSCRIBED_MESSAGE: &str = "You have been unsubscribed";
/// The title and body of the one 400 page the unsubscribe route answers
/// every refusal with: a forged, malformed or un-mintable token is told
/// the same, so the page never leaks which it was.
const INVALID_TOKEN_MESSAGE: &str = "Invalid token";

/// The HMAC flavor unsubscribe tokens carry: HMAC-SHA256 over the
/// user-id text, the [`crate::services::media_proxy`] signing recipe
/// with a different message.
type HmacSha256 = Hmac<Sha256>;

/// Whether a page may be framed: [`Framing`] decides if [`secure`] adds
/// `X-Frame-Options: DENY` or leaves the header off.
#[derive(Debug, Clone, Copy)]
enum Framing {
    /// An embed page: no `X-Frame-Options`, so another site may iframe it.
    Allow,
    /// Every other page: `X-Frame-Options: DENY`, never framable.
    Deny,
}

/// The HTML helper-pages router, merged at the root by [`crate::app`].
///
/// Mounts `/frontend_switcher`, `/embed/{id}`, `/notice/{id}/embed_player`
/// and `/mailer/unsubscribe/{token}`. Do not call [`Router::fallback`] on
/// it: unmatched paths must reach the app-wide fallback in [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route(SWITCHER_PATH, get(switcher_index).post(switcher_update))
        .route(EMBED_PATH, get(embed))
        .route(PLAYER_PATH, get(embed_player))
        .route(UNSUBSCRIBE_PATH, get(unsubscribe))
}

/// The full document of a page: the doctype, the escaped `title`, the one
/// inline [`PAGE_STYLE`] sheet and `body` inside `<main>`. The body is
/// markup the helpers here built (already escaped as it was appended), not
/// request data.
fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n\
         <style>{PAGE_STYLE}</style>\n\
         </head>\n<body>\n<main>\n{body}\n</main>\n</body>\n</html>\n",
        escape_html(title),
    )
}

/// The body of a page whose whole content is `message`: an `<h1>` carrying
/// the escaped text.
fn message_body(message: &str) -> String {
    format!("<h1>{}</h1>\n", escape_html(message))
}

/// A `status` response carrying [`page`]'s document for `title` and
/// `body`, secured for `framing`: the one place a page becomes an answer.
fn respond(status: StatusCode, title: &str, body: &str, framing: Framing) -> Response {
    let mut response = (status, page(title, body)).into_response();
    secure(&mut response, framing);
    response
}

/// [`respond`] for the pages that are not made for embedding: the
/// [`Framing::Deny`] flavour.
fn protected_page(status: StatusCode, title: &str, body: &str) -> Response {
    respond(status, title, body, Framing::Deny)
}

/// A `status` response for the embed pages: `respond` with the
/// `Framing::Allow` flavour.
///
/// The embed route and the player answer with it. It is public (where the
/// rest of the shared helpers are module-internal) so framable pages
/// built elsewhere can reuse it.
#[must_use]
pub fn embed_page(status: StatusCode, title: &str, body: &str) -> Response {
    respond(status, title, body, Framing::Allow)
}

/// Puts the module headers on `response`: the HTML media type and
/// [`PAGE_CSP`] always, `X-Frame-Options: DENY` only under
/// [`Framing::Deny`] — embed pages must stay framable, so the header is
/// left off entirely rather than sent with another value.
fn secure(response: &mut Response, framing: Framing) {
    put_header(response, header::CONTENT_TYPE, PAGE_CONTENT_TYPE);
    put_header(response, CSP_HEADER.clone(), PAGE_CSP);
    if matches!(framing, Framing::Deny) {
        put_header(response, FRAME_OPTIONS_HEADER.clone(), "DENY");
    }
}

/// Sets header `name` on `response` to the static value `value`, replacing
/// any earlier one: [`HeaderMap::insert`]'s returned old value may not be
/// dropped on the floor silently.
///
/// [`HeaderMap::insert`]: axum::http::HeaderMap::insert
fn put_header(response: &mut Response, name: HeaderName, value: &'static str) {
    let replaced = response
        .headers_mut()
        .insert(name, HeaderValue::from_static(value));
    drop(replaced);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::html_pages::test_support::headers_of;

    #[test]
    fn page_is_the_full_self_contained_document() {
        let html = page("Test <title>", "<p>body & markup</p>");
        assert!(html.starts_with("<!DOCTYPE html>"), "starts at the doctype");
        assert!(
            html.contains("<title>Test &lt;title&gt;</title>"),
            "the title is escaped: {html}"
        );
        assert!(
            html.contains(&format!("<style>{PAGE_STYLE}</style>")),
            "the one inline sheet is the only CSS"
        );
        assert!(
            html.contains("<main>\n<p>body & markup</p>\n</main>"),
            "the body rides inside <main>, untouched"
        );
        assert!(!html.contains("<script"), "the pages carry no scripts");
        assert!(html.ends_with("</body>\n</html>\n"), "the document closes");
    }

    #[test]
    fn message_body_escapes_its_message() {
        let body = message_body("<b>bad</b>&\"quote'");
        assert_eq!(
            body, "<h1>&lt;b&gt;bad&lt;/b&gt;&amp;&quot;quote&#39;</h1>\n",
            "every unsafe character becomes a reference"
        );
    }

    #[test]
    fn secure_sets_the_module_headers_on_every_page() {
        let mut denied = protected_page(StatusCode::OK, "t", "b");
        secure(&mut denied, Framing::Deny);
        let headers = headers_of(&denied);
        assert_eq!(
            headers[header::CONTENT_TYPE],
            PAGE_CONTENT_TYPE,
            "every page answers as HTML"
        );
        assert_eq!(
            headers[CSP_HEADER.clone()],
            PAGE_CSP,
            "every page sends the CSP"
        );
        assert_eq!(
            headers[FRAME_OPTIONS_HEADER.clone()],
            "DENY",
            "a protected page cannot be framed"
        );
        assert_eq!(denied.status(), StatusCode::OK, "secure keeps the status");

        let mut allowed = embed_page(StatusCode::OK, "t", "b");
        secure(&mut allowed, Framing::Allow);
        let embed_headers = headers_of(&allowed);
        assert_eq!(
            embed_headers[CSP_HEADER.clone()],
            PAGE_CSP,
            "embed pages get the CSP too"
        );
        assert_eq!(
            embed_headers[header::CONTENT_TYPE],
            PAGE_CONTENT_TYPE,
            "and the HTML media type"
        );
        assert!(
            !embed_headers.contains_key(FRAME_OPTIONS_HEADER.clone()),
            "embed pages omit X-Frame-Options so they can be iframed"
        );
    }

    #[test]
    fn respond_wraps_the_page_and_secures_it() {
        let response = respond(
            StatusCode::IM_A_TEAPOT,
            "Teapot",
            "<p>short</p>",
            Framing::Deny,
        );
        assert_eq!(
            response.status(),
            StatusCode::IM_A_TEAPOT,
            "the given status"
        );
        assert_eq!(
            response.headers()[CSP_HEADER.clone()],
            PAGE_CSP,
            "the module headers"
        );
    }
}
