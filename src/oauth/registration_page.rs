//! The HTML of the external-OAuth registration.
//!
//! `GET /oauth/registration_details` renders the details form for an
//! unlinked provider account, `POST /oauth/register` re-renders it with an
//! error line when a submission is refused, and a registration whose
//! account first has to be approved or email-confirmed is shown the
//! message page instead of the login.
//!
//! The behavioural reference is Pleroma's `registration_details` action and
//! its `register.html.eex` template
//! (`lib/pleroma/web/o_auth/o_auth_controller.ex`,
//! `lib/pleroma/web/templates/o_auth/o_auth/`). Pleroma's second half —
//! "sign in to connect an existing account" — has no analogue here: an
//! existing account links itself through the provider callback, which is
//! the flow that sent the browser to this page in the first place. Like
//! [`crate::oauth::page`] the pages are plain `String` pushes
//! with every dynamic value HTML-escaped, and self-contained: inline CSS,
//! no scripts (decision 1).

use crate::oauth::page::{push_error, push_escaped, push_head, push_tail};

/// Where [`render_form`]'s form posts.
const FORM_ACTION: &str = "/oauth/register";
/// The heading and `<title>` of both pages (Pleroma's "oauth register page
/// title").
const PAGE_TITLE: &str = "Registration Details";
/// The prompt above the form (Pleroma's "oauth register page fill form
/// prompt").
const FORM_PROMPT: &str =
    "If you'd like to register a new account, please provide the details below.";
/// The submit button's text; this form only ever registers.
const SUBMIT_TEXT: &str = "Register";

/// One labelled field of the form: id, name, label and value, so the push
/// helpers stay inside the lint gate's parameter budget.
#[derive(Debug)]
struct Field<'a> {
    /// The element id and the posted field name alike: flat and short here,
    /// so the two coincide (`nickname`, `email`, `name`, `bio`).
    name: &'a str,
    /// The input's `type` attribute; a textarea ignores it.
    input_type: &'a str,
    /// The label text.
    label: &'a str,
    /// The prefilled value: an attribute for an input, contents for a
    /// textarea, escaped either way.
    value: &'a str,
}

/// Everything [`render_form`] puts on the registration page.
#[derive(Debug)]
pub struct RegistrationForm<'a> {
    /// Value of the `nickname` input.
    pub nickname: &'a str,
    /// Value of the `email` input.
    pub email: &'a str,
    /// Value of the `name` input.
    pub name: &'a str,
    /// Contents of the `bio` textarea.
    pub bio: &'a str,
    /// Value of the hidden `state` input: the login state this form
    /// finishes.
    pub state: &'a str,
    /// The error line shown above the form (a refused submission
    /// re-render); `None` renders none.
    pub error: Option<&'a str>,
}

/// Renders the registration form for `form`.
///
/// Pleroma's prompt, the four editable fields prefilled (the provider's
/// values on a first visit, the submission's on a re-render), the Register
/// button and the hidden `state` round-trip — posting to `/oauth/register`.
#[must_use]
pub fn render_form(form: &RegistrationForm<'_>) -> String {
    let inputs = [
        Field {
            name: "nickname",
            input_type: "text",
            label: "Nickname",
            value: form.nickname,
        },
        Field {
            name: "email",
            input_type: "text",
            label: "Email",
            value: form.email,
        },
        Field {
            name: "name",
            input_type: "text",
            label: "Name",
            value: form.name,
        },
    ];
    let bio = Field {
        name: "bio",
        input_type: "text",
        label: "Bio",
        value: form.bio,
    };
    let mut out = String::new();
    push_head(&mut out, PAGE_TITLE);
    push_opening(&mut out);
    push_error(&mut out, form.error);
    out.push_str("<p>");
    push_escaped(&mut out, FORM_PROMPT);
    out.push_str("</p>\n");
    out.push_str("<form method=\"post\" action=\"");
    out.push_str(FORM_ACTION);
    out.push_str("\">\n");
    for field in &inputs {
        push_input(&mut out, field);
    }
    push_textarea(&mut out, &bio);
    out.push_str(
        "<div class=\"actions\">\n<input type=\"submit\" class=\"button--approve\" value=\"",
    );
    out.push_str(SUBMIT_TEXT);
    out.push_str("\">\n</div>\n");
    out.push_str("<input type=\"hidden\" name=\"state\" value=\"");
    push_escaped(&mut out, form.state);
    out.push_str("\">\n</form>\n</main>\n");
    push_tail(&mut out);
    out
}

/// Renders the page a registration that cannot log in yet answers with.
///
/// The account exists, and `message` says what it first waits for — staff
/// approval or the confirmation click (Pleroma shows its registration
/// messages the same way, as a flash line on a page of its own).
#[must_use]
pub fn render_message(message: &str) -> String {
    let mut out = String::new();
    push_head(&mut out, PAGE_TITLE);
    push_opening(&mut out);
    out.push_str("<p class=\"alert\" role=\"status\">");
    push_escaped(&mut out, message);
    out.push_str("</p>\n</main>\n");
    push_tail(&mut out);
    out
}

/// Opens the shared `<main>` block with the page heading.
fn push_opening(out: &mut String) {
    out.push_str("<main>\n<h1>");
    out.push_str(PAGE_TITLE);
    out.push_str("</h1>\n");
}

/// Appends one labelled text input prefilled with its value.
fn push_input(out: &mut String, field: &Field<'_>) {
    out.push_str("<div class=\"input\">\n<label for=\"");
    push_escaped(out, field.name);
    out.push_str("\">");
    push_escaped(out, field.label);
    out.push_str("</label>\n<input type=\"");
    push_escaped(out, field.input_type);
    out.push_str("\" id=\"");
    push_escaped(out, field.name);
    out.push_str("\" name=\"");
    push_escaped(out, field.name);
    out.push_str("\" value=\"");
    push_escaped(out, field.value);
    out.push_str("\">\n</div>\n");
}

/// Appends one labelled textarea holding its value.
fn push_textarea(out: &mut String, field: &Field<'_>) {
    out.push_str("<div class=\"input\">\n<label for=\"");
    push_escaped(out, field.name);
    out.push_str("\">");
    push_escaped(out, field.label);
    out.push_str("</label>\n<textarea id=\"");
    push_escaped(out, field.name);
    out.push_str("\" name=\"");
    push_escaped(out, field.name);
    out.push_str("\" rows=\"4\">");
    push_escaped(out, field.value);
    out.push_str("</textarea>\n</div>\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The form the shape tests render: everything set, no error.
    fn form<'a>() -> RegistrationForm<'a> {
        RegistrationForm {
            nickname: "octocat",
            email: "octocat@example.org",
            name: "The Octocat",
            bio: "lives in a repo",
            state: "issued-state",
            error: None,
        }
    }

    #[test]
    fn the_form_posts_the_editable_fields_and_the_state() {
        let html = render_form(&form());
        assert!(
            html.contains("<form method=\"post\" action=\"/oauth/register\">"),
            "the form posts to /oauth/register"
        );
        assert!(
            html.contains("name=\"nickname\" value=\"octocat\""),
            "the nickname is prefilled and editable: {html}"
        );
        assert!(
            html.contains("name=\"email\" value=\"octocat@example.org\""),
            "so is the email"
        );
        assert!(
            html.contains("name=\"name\" value=\"The Octocat\""),
            "and the name"
        );
        assert!(
            html.contains("name=\"bio\" rows=\"4\">lives in a repo</textarea>"),
            "and the bio"
        );
        assert!(
            html.contains("<input type=\"hidden\" name=\"state\" value=\"issued-state\">"),
            "the state rides along hidden, single-use like a code"
        );
        assert!(html.contains("value=\"Register\""), "there is a submit");
    }

    #[test]
    fn every_prefilled_value_is_escaped() {
        let evil = RegistrationForm {
            nickname: "\"><script>alert('xss')</script>",
            email: "a&b@example.org",
            name: "<img src=x onerror=alert(1)>",
            bio: "</textarea><p>broken</p>",
            state: "s<>&\"",
            error: Some("<b>bad</b>"),
        };
        let html = render_form(&evil);
        assert!(!html.contains("<script>alert"), "a field cannot open a tag");
        assert!(!html.contains("<img src=x"), "nor an element of its own");
        assert!(
            !html.contains("</textarea><p"),
            "nor close its textarea early"
        );
        assert!(!html.contains("<b>bad</b>"), "nor can the error line");
        assert!(
            html.contains("value=\"&quot;&gt;&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;\""),
            "the nickname shows escaped inside its attribute"
        );
        assert!(
            html.contains("value=\"a&amp;b@example.org\""),
            "ampersands too"
        );
        assert!(
            html.contains("value=\"s&lt;&gt;&amp;&quot;\""),
            "and the state is escaped like anything else"
        );
    }

    #[test]
    fn the_error_line_appears_only_when_set() {
        assert!(
            !render_form(&form()).contains("role=\"alert\""),
            "a fresh form has no error line"
        );
        let refused = RegistrationForm {
            error: Some("Registrations are closed"),
            ..form()
        };
        let html = render_form(&refused);
        assert!(
            html.contains(
                "<p class=\"alert alert-danger\" role=\"alert\">Registrations are closed</p>"
            ),
            "the refusal stands above the form: {html}"
        );
    }

    #[test]
    fn the_message_page_shows_one_escaped_message() {
        let html = render_message(
            "You have been registered. You'll be able to log in once your account is approved.",
        );
        assert!(
            html.contains("<p class=\"alert\" role=\"status\">You have been registered. You&#39;ll be able to log in once your account is approved.</p>"),
            "the message is the page's whole content: {html}"
        );
        assert!(
            render_message("<script>no</script>").contains("&lt;script&gt;no&lt;/script&gt;"),
            "and it is escaped"
        );
    }

    #[test]
    fn both_pages_are_self_contained_documents() {
        for html in [render_form(&form()), render_message("x")] {
            assert!(html.starts_with("<!DOCTYPE html>"), "a doctype");
            assert!(html.contains("<meta charset=\"utf-8\">"), "its encoding");
            assert!(html.contains("<style>"), "inline CSS");
            assert!(!html.contains("<script"), "and no scripts at all");
            assert!(html.ends_with("</body>\n</html>\n"), "a closed document");
        }
    }
}
