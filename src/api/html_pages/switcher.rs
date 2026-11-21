//! The frontend switcher: `GET /frontend_switcher` renders the pick-list
//! of the configured frontends and `POST /frontend_switcher` validates a
//! pick, sets the year-long `preferred_frontend` cookie and sends the
//! browser back to `/` (Pleroma's `FrontendSwitcherController`,
//! `lib/pleroma/web/frontend_switcher/frontend_switcher_controller.ex`).
//!
//! The page helpers the handlers answer with — [`protected_page`],
//! [`message_body`], [`put_header`] — and the path, title and message
//! constants they name live in [`super`].

use axum::{
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use serde::Deserialize;

use crate::{
    error::{AppError, AppResult},
    extract::ApiBody,
    state::AppState,
    util::sanitize::escape_html,
};

use super::{
    COOKIE_MAX_AGE, FRONTEND_COOKIE, FRONTEND_FIELD, INVALID_FRONTEND, NO_FRONTENDS_MESSAGE,
    REDIRECT_PATH, SWITCHER_PATH, SWITCHER_TITLE, SWITCHING_MESSAGE, message_body, protected_page,
    put_header,
};

/// The form fields of `POST /frontend_switcher`: Pleroma's
/// `%{"frontend" => name}`, with the name optional so a request without it
/// deserializes and is refused as a bad pick rather than as a parse error.
#[derive(Debug, Default, Deserialize)]
pub(super) struct SwitcherForm {
    /// The picked frontend's name; `None` when the form carries no
    /// `frontend` field.
    frontend: Option<String>,
}

/// `GET /frontend_switcher` (Pleroma's `FrontendSwitcherController.index`):
/// the pick-list of [`Config::available_frontends`] as a form of labelled
/// radios, or the empty-list message when nothing is configured.
///
/// [`Config::available_frontends`]: crate::config::Config::available_frontends
pub(super) async fn switcher_index(State(state): State<AppState>) -> Response {
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
pub(super) async fn switcher_update(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::html_pages::PAGE_CONTENT_TYPE;

    /// One available frontend named `name`, for the pick-list fixtures.
    fn named(name: &str) -> String {
        name.to_owned()
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
