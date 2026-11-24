//! Top-level HTML helper pages: the small standalone documents this
//! server serves outside `/api`.
//!
//! This module ships one page: the mailer unsubscribe page
//! (`GET /mailer/unsubscribe/{token}`, Pleroma's `SubscriptionController`
//! in `lib/pleroma/web/mailer/subscription_controller.ex`), built out of
//! the shared page helpers here.
//!
//! The contract every response this module answers keeps:
//!
//! - `content-type: text/html; charset=utf-8`.
//! - Every dynamic value — a message, a title — goes through
//!   [`escape_html`], so nothing a request or a config carries can open a
//!   tag or an attribute.
//! - `content-security-policy: default-src 'none'; style-src
//!   'unsafe-inline'`, spelled by `PAGE_CSP`: the pages load no scripts, no
//!   external CSS, no fonts, no images and no media, only the one inline
//!   `PAGE_STYLE` sheet.
//! - `x-frame-options: DENY` on every page: no page here can be framed.
//!
//! Pleroma's `/frontend_switcher` pair, `/embed/{id}` card and
//! `/notice/{id}/embed_player` player are deliberately not served: Wired
//! is API-only, so those paths fall through to the app-wide fallback. A
//! client that wants an embed renders its own from
//! `GET /api/v1/statuses/{id}`.
//!
//! This module is a tree. `unsubscribe` holds the mailer link and its HMAC
//! tokens; this root keeps the router, the shared constants and header
//! statics and the page helpers every child answers through.
//! `test_support` holds the unit-test fixtures the children share.

use axum::{
    Router,
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use hmac::Hmac;
use sha2_011::Sha256;

use crate::{state::AppState, util::sanitize::escape_html};

use self::unsubscribe::unsubscribe;

pub use self::unsubscribe::{unsubscribe_token, unsubscribe_user_id};

#[cfg(test)]
mod test_support;
mod unsubscribe;

/// The one CSP every page in this module sends: no scripts, inline CSS
/// only, no images or media.
const PAGE_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'";
/// The media type of every page: [`page`] renders full HTML documents.
const PAGE_CONTENT_TYPE: &str = "text/html; charset=utf-8";
/// The one inline stylesheet [`page`] embeds: [`PAGE_CSP`] allows no other
/// CSS, so every page in the module shares exactly this sheet.
const PAGE_STYLE: &str = "body{margin:0;min-height:100vh;display:flex;\
    align-items:center;justify-content:center;background:#f5f4ef;\
    font-family:system-ui,sans-serif;color:#28242d}\
    main{background:#fff;border-radius:8px;padding:2rem;max-width:30rem;\
    box-shadow:0 2px 10px rgba(40,36,45,0.12)}\
    h1{font-size:1.3rem;margin-top:0}";
/// The `X-Frame-Options` header [`secure`] puts up.
static FRAME_OPTIONS_HEADER: HeaderName = HeaderName::from_static("x-frame-options");
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

/// The HTML helper-pages router, merged at the root by [`crate::app`].
///
/// Mounts `/mailer/unsubscribe/{token}`. Do not call
/// [`Router::fallback`] on it: unmatched paths must reach the app-wide
/// fallback in [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new().route(UNSUBSCRIBE_PATH, get(unsubscribe))
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
/// `body`, secured by [`secure`]: the one place a page becomes an answer.
fn protected_page(status: StatusCode, title: &str, body: &str) -> Response {
    let mut response = (status, page(title, body)).into_response();
    secure(&mut response);
    response
}

/// Puts the module headers on `response`: the HTML media type, [`PAGE_CSP`]
/// and `X-Frame-Options: DENY`, so no page can be framed.
fn secure(response: &mut Response) {
    put_header(response, header::CONTENT_TYPE, PAGE_CONTENT_TYPE);
    put_header(response, header::CONTENT_SECURITY_POLICY, PAGE_CSP);
    put_header(response, FRAME_OPTIONS_HEADER.clone(), "DENY");
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
        let denied = protected_page(StatusCode::OK, "t", "b");
        let headers = headers_of(&denied);
        assert_eq!(
            headers[header::CONTENT_TYPE],
            PAGE_CONTENT_TYPE,
            "every page answers as HTML"
        );
        assert_eq!(
            headers[header::CONTENT_SECURITY_POLICY],
            PAGE_CSP,
            "every page sends the CSP"
        );
        assert_eq!(
            headers[FRAME_OPTIONS_HEADER.clone()],
            "DENY",
            "a protected page cannot be framed"
        );
        assert_eq!(denied.status(), StatusCode::OK, "secure keeps the status");
    }

    #[test]
    fn protected_page_wraps_the_page_and_secures_it() {
        let response = protected_page(StatusCode::IM_A_TEAPOT, "Teapot", "<p>short</p>");
        assert_eq!(
            response.status(),
            StatusCode::IM_A_TEAPOT,
            "the given status"
        );
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            PAGE_CSP,
            "the module headers"
        );
    }
}
