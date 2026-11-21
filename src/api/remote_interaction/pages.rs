//! The pages this feature renders: the login form and its success page,
//! the remote-subscribe form, their bodies and the shared error line, and
//! the document and header plumbing every one of them rides on — plus
//! the parameter trimming the routes read their requests through.

use axum::{
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse as _, Response},
};

use crate::util::sanitize::escape_html;

use super::{
    CSP_HEADER, FOLLOW_TITLE, FOLLOWED_MESSAGE, FRAME_OPTIONS_HEADER, PAGE_CONTENT_TYPE, PAGE_CSP,
    PAGE_STYLE, REMOTE_SUBSCRIBE_PATH, REMOTE_SUBSCRIBE_TITLE, SUBSCRIBE_PATH,
};

/// The `GET/POST /ostatus_subscribe` page: [`subscribe_body`] under the
/// module headers.
pub(super) fn subscribe_page(acct: &str, error: Option<&str>) -> Response {
    page(FOLLOW_TITLE, &subscribe_body(acct, error))
}

/// The body of the login page: the message, the optional error line, and
/// the form posting the hidden `acct` plus the nested credentials to
/// [`SUBSCRIBE_PATH`]. Every request-sourced value goes through
/// [`escape_html`].
pub(super) fn subscribe_body(acct: &str, error: Option<&str>) -> String {
    let line = error_line(error);
    let escaped_acct = escape_html(acct);
    format!(
        "<h1>{FOLLOW_TITLE}</h1>\n{line}\
         <form method=\"post\" action=\"{SUBSCRIBE_PATH}\">\n\
         <input type=\"hidden\" name=\"acct\" value=\"{escaped_acct}\">\n\
         <label for=\"authorization_name\">Username</label>\n\
         <input type=\"text\" id=\"authorization_name\" name=\"authorization[name]\" \
         autocomplete=\"username\" required>\n\
         <label for=\"authorization_password\">Password</label>\n\
         <input type=\"password\" id=\"authorization_password\" \
         name=\"authorization[password]\" autocomplete=\"current-password\" required>\n\
         <input type=\"submit\" value=\"Follow\">\n\
         </form>\n",
    )
}

/// The page a completed follow answers with: [`success_body`] under the
/// module headers.
pub(super) fn success_page(acct: &str) -> Response {
    page(FOLLOW_TITLE, &success_body(acct))
}

/// The body of the success page: the followed handle, escaped.
pub(super) fn success_body(acct: &str) -> String {
    let escaped_acct = escape_html(acct);
    format!("<h1>{FOLLOW_TITLE}</h1>\n<p>{FOLLOWED_MESSAGE} <b>{escaped_acct}</b>.</p>\n")
}

/// The `GET/POST /main/ostatus` page: [`remote_subscribe_body`] under the
/// module headers.
pub(super) fn remote_subscribe_page(
    nickname: &str,
    profile: &str,
    error: Option<&str>,
) -> Response {
    page(
        REMOTE_SUBSCRIBE_TITLE,
        &remote_subscribe_body(nickname, profile, error),
    )
}

/// The body of the remote-subscribe form: the message, the optional
/// error line, and the form posting `user[nickname]` / `user[profile]`
/// to [`REMOTE_SUBSCRIBE_PATH`], both values escaped.
pub(super) fn remote_subscribe_body(nickname: &str, profile: &str, error: Option<&str>) -> String {
    let line = error_line(error);
    let escaped_nickname = escape_html(nickname);
    let escaped_profile = escape_html(profile);
    format!(
        "<h1>{REMOTE_SUBSCRIBE_TITLE}</h1>\n{line}\
         <form method=\"post\" action=\"{REMOTE_SUBSCRIBE_PATH}\">\n\
         <label for=\"user_nickname\">Your account</label>\n\
         <input type=\"text\" id=\"user_nickname\" name=\"user[nickname]\" \
         value=\"{escaped_nickname}\" required>\n\
         <label for=\"user_profile\">Remote account</label>\n\
         <input type=\"text\" id=\"user_profile\" name=\"user[profile]\" \
         value=\"{escaped_profile}\" required>\n\
         <input type=\"submit\" value=\"Subscribe\">\n\
         </form>\n",
    )
}

/// The error line of a re-rendered page, the empty string for a first
/// render: the escaped text inside `<p class="error">`.
fn error_line(error: Option<&str>) -> String {
    let Some(text) = error else {
        return String::new();
    };
    let escaped = escape_html(text);
    format!("<p class=\"error\">{escaped}</p>\n")
}

/// A page response: [`page_document`]'s HTML under the module headers —
/// the HTML media type, [`PAGE_CSP`] and `X-Frame-Options: DENY`.
fn page(title: &str, body: &str) -> Response {
    let mut response = (StatusCode::OK, page_document(title, body)).into_response();
    put_header(&mut response, header::CONTENT_TYPE, PAGE_CONTENT_TYPE);
    put_header(&mut response, CSP_HEADER.clone(), PAGE_CSP);
    put_header(&mut response, FRAME_OPTIONS_HEADER.clone(), "DENY");
    response
}

/// The full document of a page: the doctype, the escaped `title`, the
/// one inline [`PAGE_STYLE`] sheet and `body` inside `<main>`. The body
/// is markup this module's builders wrote, already escaped where it came
/// from a request.
pub(super) fn page_document(title: &str, body: &str) -> String {
    let escaped_title = escape_html(title);
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{escaped_title}</title>\n\
         <style>{PAGE_STYLE}</style>\n\
         </head>\n<body>\n<main>\n{body}\n</main>\n</body>\n</html>\n",
    )
}

/// Sets header `name` on `response` to the static value `value`,
/// replacing any earlier one (the old value may not be dropped silently).
fn put_header(response: &mut Response, name: HeaderName, value: &'static str) {
    let replaced = response
        .headers_mut()
        .insert(name, HeaderValue::from_static(value));
    drop(replaced);
}

/// `value` trimmed, or the empty string when the request carried none.
pub(super) fn trimmed(value: Option<&str>) -> &str {
    value.map_or("", str::trim)
}

/// The parameter `value` when the request carried it non-blank after
/// trimming — the presence rule of every required parameter here.
pub(super) fn required(value: Option<&str>) -> Option<&str> {
    let text = trimmed(value);
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::remote_interaction::SOMETHING_WENT_WRONG;

    #[test]
    fn the_subscribe_form_posts_the_documented_fields() {
        let body = subscribe_body(r#"<i>&"'x"#, Some("bad & worse"));
        assert!(
            body.contains("action=\"/ostatus_subscribe\""),
            "the form posts back to the subscribe route: {body}"
        );
        assert!(
            body.contains("name=\"acct\" value=\"&lt;i&gt;&amp;&quot;&#39;x\""),
            "the hidden acct is escaped: {body}"
        );
        assert!(
            body.contains("name=\"authorization[name]\""),
            "the username field is the nested authorization name: {body}"
        );
        assert!(
            body.contains("name=\"authorization[password]\""),
            "the password field is the nested authorization password: {body}"
        );
        assert!(
            body.contains("type=\"password\""),
            "the password input hides its text: {body}"
        );
        assert!(
            body.contains("<p class=\"error\">bad &amp; worse</p>"),
            "the error line is escaped: {body}"
        );
    }

    #[test]
    fn a_first_render_shows_no_error_line() {
        let body = subscribe_body("bob@remote.example", None);
        assert!(
            !body.contains("class=\"error\""),
            "no failure, no line: {body}"
        );
    }

    #[test]
    fn the_remote_subscribe_form_carries_both_nested_fields() {
        let body = remote_subscribe_body(
            "alice\" onload=\"pwn()\"",
            "<b>bob@remote.example</b>",
            Some(SOMETHING_WENT_WRONG),
        );
        assert!(
            body.contains("action=\"/main/ostatus\""),
            "the form posts to the subscribe route: {body}"
        );
        assert!(
            body.contains("name=\"user[nickname]\" value=\"alice&quot; onload=&quot;pwn()&quot;\""),
            "the local account is escaped in its value: {body}"
        );
        assert!(
            body.contains("name=\"user[profile]\" value=\"&lt;b&gt;bob@remote.example&lt;/b&gt;\""),
            "the remote handle is escaped in its value: {body}"
        );
        assert!(
            body.contains("<p class=\"error\">Something went wrong.</p>"),
            "the failure says so: {body}"
        );
        assert!(
            !body.contains("<b>bob"),
            "no tag from the request survives: {body}"
        );
    }

    #[test]
    fn the_success_page_names_the_followed_acct_escaped() {
        let body = success_body(r#"eve'"><script>x</script>"#);
        assert!(body.contains(FOLLOWED_MESSAGE), "the success line");
        assert!(
            body.contains("<b>eve&#39;&quot;&gt;&lt;script&gt;x&lt;/script&gt;</b>"),
            "the acct arrives escaped: {body}"
        );
        assert!(!body.contains("<script"), "no tag opens anywhere: {body}");
    }

    #[test]
    fn the_page_document_escapes_its_title() {
        let html = page_document("<bad> & title", "<p>body</p>");
        assert!(html.starts_with("<!DOCTYPE html>"), "starts at the doctype");
        assert!(
            html.contains("<title>&lt;bad&gt; &amp; title</title>"),
            "the title is escaped: {html}"
        );
        assert!(
            html.contains("<main>\n<p>body</p>\n</main>"),
            "the body rides inside <main>: {html}"
        );
        assert!(!html.contains("<script"), "the pages carry no scripts");
    }

    #[test]
    fn every_page_answers_html_with_the_form_safe_policy() {
        for response in [
            subscribe_page("bob@remote.example", None),
            success_page("bob@remote.example"),
            remote_subscribe_page("alice", "bob@remote.example", None),
        ] {
            assert_eq!(response.status(), StatusCode::OK, "pages render at 200");
            let headers = response.headers();
            assert_eq!(
                headers[header::CONTENT_TYPE],
                PAGE_CONTENT_TYPE,
                "answered as HTML"
            );
            assert_eq!(
                headers[CSP_HEADER.clone()],
                PAGE_CSP,
                "the CSP allows the form's own submit"
            );
            assert!(
                PAGE_CSP.contains("form-action 'self'"),
                "and the policy names it: {PAGE_CSP}"
            );
            assert_eq!(
                headers[FRAME_OPTIONS_HEADER.clone()],
                "DENY",
                "a login form is not framable"
            );
        }
    }

    #[test]
    fn required_parameters_are_present_and_non_blank() {
        assert_eq!(required(None), None, "an absent parameter");
        assert_eq!(required(Some("")), None, "an empty one");
        assert_eq!(required(Some("   ")), None, "a blank one");
        assert_eq!(
            required(Some(" https://x.example/a ")),
            Some("https://x.example/a"),
            "a value is trimmed"
        );
        assert_eq!(trimmed(None), "", "and an absent value reads as empty");
        assert_eq!(
            trimmed(Some(" https://x.example/a ")),
            "https://x.example/a"
        );
    }
}
