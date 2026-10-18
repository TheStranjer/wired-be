//! Top-level HTML helper pages: the small standalone documents this
//! server serves outside `/api`.
//!
//! This ticket ships the frontend switcher, built from the shared page
//! helpers here; the embed, player and unsubscribe pages arrive in later
//! tickets, on the same contract and out of the same helpers.
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

use axum::{
    Router,
    extract::State,
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use serde::Deserialize;

use crate::{
    error::{AppError, AppResult},
    extract::ApiBody,
    state::AppState,
    util::sanitize::escape_html,
};

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

/// Whether a page may be framed: [`Framing`] decides if [`secure`] adds
/// `X-Frame-Options: DENY` or leaves the header off.
#[derive(Debug, Clone, Copy)]
enum Framing {
    /// An embed page: no `X-Frame-Options`, so another site may iframe it.
    Allow,
    /// Every other page: `X-Frame-Options: DENY`, never framable.
    Deny,
}

/// The form fields of `POST /frontend_switcher`: Pleroma's
/// `%{"frontend" => name}`, with the name optional so a request without it
/// deserializes and is refused as a bad pick rather than as a parse error.
#[derive(Debug, Default, Deserialize)]
struct SwitcherForm {
    /// The picked frontend's name; `None` when the form carries no
    /// `frontend` field.
    frontend: Option<String>,
}

/// The HTML helper-pages router, merged at the root by [`crate::app`].
///
/// Mounts `/frontend_switcher` only; later tickets add the embed, player
/// and unsubscribe routes here. Do not call [`Router::fallback`] on it:
/// unmatched paths must reach the app-wide fallback in [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new().route(SWITCHER_PATH, get(switcher_index).post(switcher_update))
}

/// `GET /frontend_switcher` (Pleroma's `FrontendSwitcherController.index`):
/// the pick-list of [`Config::available_frontends`] as a form of labelled
/// radios, or the empty-list message when nothing is configured.
///
/// [`Config::available_frontends`]: crate::config::Config::available_frontends
async fn switcher_index(State(state): State<AppState>) -> Response {
    protected_page(
        StatusCode::OK,
        SWITCHER_TITLE,
        &switcher_body(&state.config.available_frontends),
    )
}

/// `POST /frontend_switcher` (Pleroma's `FrontendSwitcherController.update`):
/// a pick on the list sets the year-long `preferred_frontend` cookie and
/// answers 302 to `/`; a name off the list — or no name at all — answers
/// 422 with the [`INVALID_FRONTEND`] page.
///
/// # Errors
///
/// [`AppError::Internal`] when a validated configured name cannot become a
/// header value (a name the header grammar cannot express; nothing else
/// about the request matters to that outcome).
async fn switcher_update(
    State(state): State<AppState>,
    ApiBody(form): ApiBody<SwitcherForm>,
) -> AppResult<Response> {
    let picked = form
        .frontend
        .filter(|name| state.config.available_frontends.contains(name));
    let Some(name) = picked else {
        return Ok(protected_page(
            StatusCode::UNPROCESSABLE_ENTITY,
            INVALID_FRONTEND,
            &message_body(INVALID_FRONTEND),
        ));
    };
    frontend_redirect(&name)
}

/// The 302 answer to a valid pick: to [`REDIRECT_PATH`], carrying the
/// [`FRONTEND_COOKIE`] cookie and the module headers.
///
/// # Errors
///
/// [`AppError::Internal`] when the cookie value is not a legal header
/// value, as in [`switcher_update`].
fn frontend_redirect(name: &str) -> AppResult<Response> {
    let cookie = HeaderValue::from_str(&frontend_cookie(name)).map_err(|err| {
        AppError::Internal(format!(
            "the configured frontend name is not cookie-safe: {err}"
        ))
    })?;
    let mut response = protected_page(
        StatusCode::FOUND,
        SWITCHER_TITLE,
        &message_body(SWITCHING_MESSAGE),
    );
    put_header(&mut response, header::LOCATION, REDIRECT_PATH);
    let replaced = response.headers_mut().insert(header::SET_COOKIE, cookie);
    drop(replaced);
    Ok(response)
}

/// The `Set-Cookie` value of a valid pick: the year-long, lax
/// `preferred_frontend` cookie, the shape
/// [`crate::api::pleroma_settings`] sets plus the `SameSite` Pleroma's
/// switcher sends.
fn frontend_cookie(name: &str) -> String {
    format!("{FRONTEND_COOKIE}={name}; Path=/; Max-Age={COOKIE_MAX_AGE}; SameSite=Lax")
}

/// The body of `GET /frontend_switcher`: the pick-list form when any
/// frontend is configured — one labelled radio per entry, in
/// configuration order — the [`NO_FRONTENDS_MESSAGE`] page (no form, no
/// radios, nothing to pick) when none is.
fn switcher_body(frontends: &[String]) -> String {
    if frontends.is_empty() {
        return message_body(NO_FRONTENDS_MESSAGE);
    }
    let mut out = message_body(SWITCHER_TITLE);
    out.push_str("<form method=\"post\" action=\"");
    out.push_str(SWITCHER_PATH);
    out.push_str("\">\n");
    for frontend in frontends {
        push_radio(&mut out, frontend);
    }
    out.push_str("<input type=\"submit\" value=\"Switch\">\n</form>\n");
    out
}

/// Appends one labelled `frontend` radio for `name`: the value and the
/// label are the escaped name (Pleroma shows the configured name as its
/// own label).
fn push_radio(out: &mut String, name: &str) {
    let escaped = escape_html(name);
    out.push_str("<div class=\"frontend\">\n<label><input type=\"radio\" name=\"");
    out.push_str(FRONTEND_FIELD);
    out.push_str("\" value=\"");
    out.push_str(&escaped);
    out.push_str("\">");
    out.push_str(&escaped);
    out.push_str("</label>\n</div>\n");
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
/// This is the shorthand the embed and player routes of later tickets
/// answer with. It is public (where the rest of the shared helpers are
/// module-internal) because this ticket ships no embed route yet: a
/// never-called private item would not compile, and later tickets call it
/// from this module anyway.
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

    /// One available frontend named `name`, for the pick-list fixtures.
    fn named(name: &str) -> String {
        name.to_owned()
    }

    /// The headers of `response` as a debuggable map.
    fn headers_of(response: &Response) -> axum::http::HeaderMap {
        response.headers().clone()
    }

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

    #[test]
    fn the_switcher_lists_every_frontend_in_config_order() {
        let html = switcher_body(&[named("pleroma-fe"), named("mochi-pub")]);
        assert!(
            html.contains("<form method=\"post\" action=\"/frontend_switcher\">"),
            "the form posts back to the switcher: {html}"
        );
        assert_eq!(
            html.matches("type=\"radio\" name=\"frontend\"").count(),
            2,
            "one labelled radio per configured frontend"
        );
        let first = html
            .find("value=\"pleroma-fe\"")
            .expect("the first frontend is listed");
        let second = html
            .find("value=\"mochi-pub\"")
            .expect("the second frontend is listed");
        assert!(first < second, "the radios follow the configuration order");
        assert!(
            html.contains(">pleroma-fe</label>"),
            "the label shows the frontend's own name"
        );
        assert!(
            html.contains("type=\"submit\""),
            "the form can be submitted"
        );
    }

    #[test]
    fn an_empty_switcher_offers_nothing() {
        let html = switcher_body(&[]);
        assert!(
            html.contains(NO_FRONTENDS_MESSAGE),
            "the empty list says so: {html}"
        );
        assert!(!html.contains("<form"), "there is no form to fill");
        assert!(!html.contains("type=\"radio\""), "and no radio to pick");
    }

    #[test]
    fn a_frontend_name_is_escaped_in_the_radio_and_its_label() {
        let html = switcher_body(&[named(r#"<b>"evil"</b>&'x"#)]);
        assert!(!html.contains("<b>"), "the name cannot open a tag");
        assert!(
            html.contains("value=\"&lt;b&gt;&quot;evil&quot;&lt;/b&gt;&amp;&#39;x\""),
            "the attribute value is escaped: {html}"
        );
        assert!(
            html.contains(">&lt;b&gt;&quot;evil&quot;&lt;/b&gt;&amp;&#39;x</label>"),
            "the label text is escaped too"
        );
    }

    #[test]
    fn the_frontend_cookie_is_the_documented_year_long_pick() {
        assert_eq!(
            frontend_cookie("pleroma-fe"),
            "preferred_frontend=pleroma-fe; Path=/; Max-Age=31536000; SameSite=Lax",
            "name, path, one year and SameSite, exactly"
        );
    }

    #[test]
    fn the_switcher_form_reads_the_frontend_field_and_may_miss_it() {
        let picked: SwitcherForm =
            serde_json::from_value(serde_json::json!({ "frontend": "pleroma-fe" }))
                .expect("a form body deserializes");
        assert_eq!(picked.frontend.as_deref(), Some("pleroma-fe"), "the pick");
        let empty: SwitcherForm = serde_json::from_value(serde_json::json!({}))
            .expect("a body without the field deserializes");
        assert_eq!(empty.frontend, None, "and reads as no pick");
        let default = SwitcherForm::default();
        assert_eq!(default.frontend, None, "the default is no pick");
    }

    #[test]
    fn the_invalid_pick_is_an_html_error_page() {
        let response = protected_page(
            StatusCode::UNPROCESSABLE_ENTITY,
            INVALID_FRONTEND,
            &message_body(INVALID_FRONTEND),
        );
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "422 for a bad pick"
        );
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            PAGE_CONTENT_TYPE,
            "answered as HTML, not JSON"
        );
    }
}
