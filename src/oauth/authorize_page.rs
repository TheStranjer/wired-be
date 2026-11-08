//! The HTML of `/oauth/authorize`: the login-and-consent page and the
//! out-of-band code page.
//!
//! The `GET` renders the consent page, and the `POST` re-renders it with
//! an error line after a failed login; the out-of-band page shows the
//! authorization code to a `urn:ietf:wg:oauth:2.0:oob` client. The
//! behavioural reference is Pleroma's
//! `lib/pleroma/web/templates/o_auth/o_auth/show.html.eex` and
//! `oob_authorization_created.html.eex`; the pages are here, but with no
//! template engine — plain `String` pushes only.
//!
//! Every dynamic value goes through [`escape_html`], so a client
//! name, redirect URI, state or scope that holds markup cannot break out
//! of its attribute or element. The pages are self-contained: inline CSS,
//! no scripts, no external assets (this server has no frontend, decision
//! 1).

use crate::util::sanitize::escape_html;

/// Where the consent form posts to.
const FORM_ACTION: &str = "/oauth/authorize";
/// The page's `lang` attribute when the request asked for no language.
const DEFAULT_LANG: &str = "en";
/// The name every scope checkbox posts under; repeated keys are the
/// approved scopes (`extract::parse_form` collects them into a list).
const SCOPE_FIELD: &str = "authorization[scope][]";
/// The name the Deny button posts under; its presence denies.
const DENY_FIELD: &str = "authorization[deny]";

/// The minimal inline stylesheet both pages share.
const PAGE_STYLE: &str = "body{margin:0;min-height:100vh;display:flex;\
    align-items:center;justify-content:center;background:#f5f4ef;\
    font-family:system-ui,sans-serif;color:#28242d}\
    main{background:#fff;border-radius:8px;padding:2rem;max-width:30rem;\
    box-shadow:0 2px 10px rgba(40,36,45,0.12)}\
    h1{font-size:1.3rem;margin-top:0}\
    code{background:#efeee9;border-radius:3px;padding:0 0.25rem}\
    p.alert{border-radius:4px;padding:0.5rem 0.75rem}\
    p.alert-danger{background:#fdecea;color:#8a1c12}\
    div.input{margin:0.75rem 0}\
    div.input label{display:block;font-size:0.9rem;margin-bottom:0.2rem}\
    div.input input{width:100%;padding:0.4rem;box-sizing:border-box}\
    div.scopes{margin:0.75rem 0}\
    div.scope label{display:block}\
    div.actions{display:flex;gap:0.5rem;margin-top:1rem}\
    input[type=submit]{padding:0.5rem 1rem;border-radius:4px;\
    border:1px solid #7c8a52;background:#7c8a52;color:#fff;cursor:pointer}\
    input[type=submit].button--cancel{background:#fff;color:#8a1c12;\
    border-color:#8a1c12}";

/// Everything [`render_consent`] puts on the login-and-consent page.
#[derive(Debug)]
pub struct ConsentPage<'a> {
    /// The application's registered name, shown in the request text.
    pub app_name: &'a str,
    /// Value of the hidden `authorization[client_id]` input.
    pub client_id: &'a str,
    /// Value of the hidden `authorization[redirect_uri]` input.
    pub redirect_uri: &'a str,
    /// Value of the hidden `authorization[state]` input; absent renders
    /// an empty one.
    pub state: Option<&'a str>,
    /// The requested scope names: one checked checkbox per name.
    pub scopes: &'a [String],
    /// The requested page language; absent renders the page default.
    pub lang: Option<&'a str>,
    /// The error line shown above the form (a failed login re-render);
    /// `None` renders none.
    pub error: Option<&'a str>,
}

/// Renders the login-and-consent page for `page`.
///
/// The document carries Pleroma's request text, one checked
/// `authorization[scope][]` checkbox per requested scope, the
/// `authorization[name]`/`authorization[password]` login fields, a Deny
/// and an Authorize button, and the hidden
/// `client_id`/`redirect_uri`/`state` round-trip fields — the form
/// Pleroma's `show.html.eex` renders, posting to `/oauth/authorize`.
#[must_use]
pub fn render_consent(page: &ConsentPage<'_>) -> String {
    let mut out = String::new();
    push_head(&mut out, page.lang, "Authorization Required");
    out.push_str("<main>\n<h1>Authorization Required</h1>\n");
    push_error(&mut out, page.error);
    push_app_request(&mut out, page.app_name);
    out.push_str("<form method=\"post\" action=\"");
    out.push_str(FORM_ACTION);
    out.push_str("\">\n");
    push_scopes(&mut out, page.scopes);
    push_field(
        &mut out,
        "authorization_name",
        "authorization[name]",
        "text",
        "Username",
    );
    push_field(
        &mut out,
        "authorization_password",
        "authorization[password]",
        "password",
        "Password",
    );
    push_actions(&mut out);
    push_hidden(&mut out, "authorization[client_id]", page.client_id);
    push_hidden(&mut out, "authorization[redirect_uri]", page.redirect_uri);
    push_hidden(
        &mut out,
        "authorization[state]",
        page.state.unwrap_or_default(),
    );
    out.push_str("</form>\n</main>\n");
    push_tail(&mut out);
    out
}

/// Renders the out-of-band page for `code` (Pleroma's
/// `oob_authorization_created.html.eex`): the client cannot be
/// redirected, so its user reads the code here and types it into the
/// application.
#[must_use]
pub fn render_oob(code: &str) -> String {
    let mut out = String::new();
    push_head(&mut out, None, "Successfully authorized");
    out.push_str("<main>\n<h1>Successfully authorized</h1>\n");
    out.push_str("<p>Token code is:<br><code id=\"authorization-code\">");
    push_escaped(&mut out, code);
    out.push_str("</code></p>\n</main>\n");
    push_tail(&mut out);
    out
}

/// Opens the document: doctype, `lang`, the given `title`, the inline
/// style and the `<body>` tag.
pub(crate) fn push_head(out: &mut String, lang: Option<&str>, title: &str) {
    out.push_str("<!DOCTYPE html>\n<html lang=\"");
    push_escaped(
        out,
        lang.filter(|text| !text.trim().is_empty())
            .unwrap_or(DEFAULT_LANG),
    );
    out.push_str("\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str("<title>");
    push_escaped(out, title);
    out.push_str("</title>\n<style>");
    out.push_str(PAGE_STYLE);
    out.push_str("</style>\n</head>\n<body>\n");
}

/// Closes the document.
pub(crate) fn push_tail(out: &mut String) {
    out.push_str("</body>\n</html>\n");
}

/// Appends the `alert-danger` error line, when there is a `message`.
pub(crate) fn push_error(out: &mut String, message: Option<&str>) {
    let Some(message) = message else {
        return;
    };
    out.push_str("<p class=\"alert alert-danger\" role=\"alert\">");
    push_escaped(out, message);
    out.push_str("</p>\n");
}

/// Appends Pleroma's request text naming `app_name`.
fn push_app_request(out: &mut String, app_name: &str) {
    out.push_str("<p>Application <strong>");
    push_escaped(out, app_name);
    out.push_str("</strong> is requesting access to your account.</p>\n");
}

/// Appends the scope list: a caption and one checked checkbox per name.
fn push_scopes(out: &mut String, scopes: &[String]) {
    out.push_str("<p>The following permissions will be granted:</p>\n<div class=\"scopes\">\n");
    for scope in scopes {
        push_scope_checkbox(out, scope);
    }
    out.push_str("</div>\n");
}

/// Appends one checked `authorization[scope][]` checkbox for `scope`,
/// labelled with its capitalised name and literal value (Pleroma's
/// `_scopes.html.eex`).
fn push_scope_checkbox(out: &mut String, scope: &str) {
    out.push_str("<div class=\"scope\">\n<label><input type=\"checkbox\" name=\"");
    out.push_str(SCOPE_FIELD);
    out.push_str("\" value=\"");
    push_escaped(out, scope);
    out.push_str("\" checked>");
    push_escaped(out, &capitalize(scope));
    out.push_str(" <code>");
    push_escaped(out, scope);
    out.push_str("</code></label>\n</div>\n");
}

/// Appends a labelled text or password input.
pub(crate) fn push_field(out: &mut String, id: &str, name: &str, input_type: &str, label: &str) {
    out.push_str("<div class=\"input\">\n<label for=\"");
    push_escaped(out, id);
    out.push_str("\">");
    push_escaped(out, label);
    out.push_str("</label>\n<input type=\"");
    push_escaped(out, input_type);
    out.push_str("\" id=\"");
    push_escaped(out, id);
    out.push_str("\" name=\"");
    push_escaped(out, name);
    out.push_str("\">\n</div>\n");
}

/// Appends the Deny and Authorize buttons. Pressing Deny submits
/// `authorization[deny]`, which is all the endpoint checks for a denial.
fn push_actions(out: &mut String) {
    out.push_str("<div class=\"actions\">\n");
    out.push_str("<input type=\"submit\" class=\"button--cancel\" name=\"");
    out.push_str(DENY_FIELD);
    out.push_str("\" value=\"Deny\">\n");
    out.push_str("<input type=\"submit\" class=\"button--approve\" value=\"Authorize\">\n");
    out.push_str("</div>\n");
}

/// Appends a hidden input carrying `value`.
pub(crate) fn push_hidden(out: &mut String, name: &str, value: &str) {
    out.push_str("<input type=\"hidden\" name=\"");
    push_escaped(out, name);
    out.push_str("\" value=\"");
    push_escaped(out, value);
    out.push_str("\">\n");
}

/// Appends `value` HTML-escaped.
pub(crate) fn push_escaped(out: &mut String, value: &str) {
    out.push_str(&escape_html(value));
}

/// Upper-cases the first character, Pleroma's `String.capitalize` for
/// the checkbox labels.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`ConsentPage`] over the fixture values, no error, default lang.
    fn page(scopes: &[String]) -> ConsentPage<'_> {
        ConsentPage {
            app_name: "Test Client",
            client_id: "client-id",
            redirect_uri: "https://client.example/callback",
            state: Some("xyz"),
            scopes,
            lang: None,
            error: None,
        }
    }

    /// A [`ConsentPage`] for a single scope — most page-shape tests only
    /// need one checkbox.
    fn single_scope_page() -> Vec<String> {
        vec!["read".to_owned()]
    }

    /// How many times `needle` occurs in `haystack`.
    fn count(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    #[test]
    fn consent_page_carries_the_login_and_consent_form() {
        let scopes = single_scope_page();
        let html = render_consent(&page(&scopes));
        assert!(
            html.contains("<form method=\"post\" action=\"/oauth/authorize\">"),
            "the form posts to /oauth/authorize"
        );
        assert!(
            html.contains("<strong>Test Client</strong> is requesting access to your account."),
            "Pleroma's request text names the app"
        );
        for field in [
            "authorization[name]",
            "authorization[password]",
            "authorization[client_id]",
            "authorization[redirect_uri]",
            "authorization[state]",
        ] {
            assert!(html.contains(field), "the field `{field}` is on the form");
        }
        assert!(
            html.contains("type=\"password\""),
            "the password field is a password input"
        );
        assert!(
            html.contains("type=\"submit\" class=\"button--cancel\" name=\"authorization[deny]\""),
            "there is a Deny button submitting authorization[deny]"
        );
        assert!(html.contains("value=\"Authorize\""), "and an Authorize one");
    }

    #[test]
    fn hidden_fields_carry_the_round_trip_values() {
        let scopes = single_scope_page();
        let html = render_consent(&page(&scopes));
        assert!(
            html.contains("name=\"authorization[client_id]\" value=\"client-id\">"),
            "the client id comes back hidden"
        );
        assert!(
            html.contains(
                "name=\"authorization[redirect_uri]\" value=\"https://client.example/callback\">"
            ),
            "so does the redirect uri"
        );
        assert!(
            html.contains("name=\"authorization[state]\" value=\"xyz\">"),
            "and the state"
        );
        let no_state = ConsentPage {
            state: None,
            ..page(&scopes)
        };
        let stateless = render_consent(&no_state);
        assert!(
            stateless.contains("name=\"authorization[state]\" value=\"\">"),
            "an absent state renders an empty hidden input, like Pleroma's"
        );
    }

    #[test]
    fn every_requested_scope_gets_a_checked_checkbox() {
        let scopes: Vec<String> = ["read", "write:statuses"]
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect();
        let html = render_consent(&page(&scopes));
        assert_eq!(
            count(&html, "name=\"authorization[scope][]\""),
            2,
            "one checkbox per requested scope"
        );
        assert!(
            html.contains("value=\"read\" checked>Read <code>read</code>"),
            "checked, capitalised label, literal value: {html}"
        );
        assert!(
            html.contains("value=\"write:statuses\" checked>Write:statuses"),
            "sub-scopes render like anything else"
        );
    }

    #[test]
    fn dynamic_values_are_html_escaped() {
        let scopes = vec!["<script>scope</script>".to_owned()];
        let evil = ConsentPage {
            app_name: "<img src=x onerror=alert(1)>",
            client_id: "ci&dc",
            redirect_uri: "https://x.example/cb?a=1&b=2",
            state: Some("\"><script>alert('xss')</script>"),
            scopes: &scopes,
            lang: Some("de\"onload=\"alert(2)"),
            error: Some("<b>bad</b>"),
        };
        let html = render_consent(&evil);
        assert!(
            !html.contains("<script>alert"),
            "the state cannot open a tag"
        );
        assert!(!html.contains("<img src=x"), "nor can the app name");
        assert!(!html.contains("<b>bad</b>"), "nor the error line");
        assert!(
            html.contains("value=\"&quot;&gt;&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;\""),
            "the state shows escaped inside its attribute"
        );
        assert!(
            html.contains("&lt;img src=x onerror=alert(1)&gt;"),
            "the app name shows escaped in the text"
        );
        assert!(
            html.contains("value=\"ci&amp;dc\""),
            "ampersands escape too"
        );
        assert!(
            html.contains("value=\"https://x.example/cb?a=1&amp;b=2\""),
            "in the redirect uri as well"
        );
        assert!(
            html.contains("value=\"&lt;script&gt;scope&lt;/script&gt;\""),
            "and in the scope checkbox"
        );
        assert!(
            html.contains("<html lang=\"de&quot;onload=&quot;alert(2)\">"),
            "even the requested language cannot break out"
        );
        assert!(
            html.contains("role=\"alert\">&lt;b&gt;bad&lt;/b&gt;</p>"),
            "the error line renders escaped"
        );
    }

    #[test]
    fn the_error_line_appears_only_when_set() {
        let scopes = single_scope_page();
        let clean = render_consent(&page(&scopes));
        assert!(
            !clean.contains("role=\"alert\""),
            "a fresh consent page has no error line"
        );
        let failed = ConsentPage {
            error: Some("Invalid Username/Password"),
            ..page(&scopes)
        };
        let html = render_consent(&failed);
        assert!(
            html.contains("Invalid Username/Password"),
            "the re-rendered page shows the failure"
        );
    }

    #[test]
    fn the_language_defaults_and_trims() {
        let scopes = single_scope_page();
        for unset in [None, Some(""), Some("   ")] {
            let page = ConsentPage {
                lang: unset,
                ..page(&scopes)
            };
            let html = render_consent(&page);
            assert!(
                html.contains("<html lang=\"en\">"),
                "`{unset:?}` renders the default language"
            );
        }
        let german = ConsentPage {
            lang: Some("de"),
            ..page(&scopes)
        };
        assert!(render_consent(&german).contains("<html lang=\"de\">"));
    }

    #[test]
    fn the_oob_page_shows_the_code() {
        let html = render_oob("abc123_TOKEN-url");
        assert!(
            html.contains("<h1>Successfully authorized</h1>"),
            "Pleroma's title"
        );
        assert!(
            html.contains("<code id=\"authorization-code\">abc123_TOKEN-url</code>"),
            "the code stands where the user (and the tests) can find it"
        );
        assert!(
            render_oob("a<b&c").contains(">a&lt;b&amp;c<"),
            "and is escaped"
        );
    }

    #[test]
    fn both_pages_are_self_contained_documents() {
        let scopes = single_scope_page();
        for html in [render_consent(&page(&scopes)), render_oob("c0de")] {
            assert!(html.starts_with("<!DOCTYPE html>"), "a doctype");
            assert!(html.contains("<meta charset=\"utf-8\">"), "its encoding");
            assert!(html.contains("<style>"), "inline CSS");
            assert!(!html.contains("<script"), "and no scripts at all");
            assert!(html.ends_with("</body>\n</html>\n"), "a closed document");
        }
    }

    #[test]
    fn capitalize_upper_cases_only_the_first_character() {
        assert_eq!(capitalize("read"), "Read", "a plain scope");
        assert_eq!(
            capitalize("read:accounts"),
            "Read:accounts",
            "the rest stays"
        );
        assert_eq!(capitalize(""), "", "nothing to capitalise");
        assert_eq!(capitalize("äbc"), "Äbc", "beyond ASCII");
    }
}
