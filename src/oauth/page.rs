//! The HTML scaffolding the server-rendered OAuth pages share:
//! [`crate::oauth::mfa_page`] (the second-factor code entry) and
//! [`crate::oauth::registration_page`] (the external-login registration
//! form).
//!
//! The consent screen is not among them: it lives at
//! `{FRONTEND_BASE_URL}/oauth/authorize`, and `GET /oauth/authorize` only
//! redirects there. What stays here is plain `String` pushes with no
//! template engine, and every dynamic value goes through
//! [`escape_html`], so a state, token or URI that holds markup cannot
//! break out of its attribute or element. The pages are self-contained:
//! inline CSS, no scripts, no external assets.

use crate::util::sanitize::escape_html;

/// The page's `lang` attribute.
const DEFAULT_LANG: &str = "en";

/// The minimal inline stylesheet every page shares.
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
    div.actions{display:flex;gap:0.5rem;margin-top:1rem}\
    input[type=submit]{padding:0.5rem 1rem;border-radius:4px;\
    border:1px solid #7c8a52;background:#7c8a52;color:#fff;cursor:pointer}";

/// Opens the document: doctype, `lang`, the given `title`, the inline
/// style and the `<body>` tag.
pub(crate) fn push_head(out: &mut String, title: &str) {
    out.push_str("<!DOCTYPE html>\n<html lang=\"");
    push_escaped(out, DEFAULT_LANG);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_is_self_contained() {
        let mut out = String::new();
        push_head(&mut out, "A <title>");
        push_tail(&mut out);
        assert!(out.starts_with("<!DOCTYPE html>"), "a doctype");
        assert!(out.contains("<html lang=\"en\">"), "the default language");
        assert!(out.contains("<meta charset=\"utf-8\">"), "its encoding");
        assert!(out.contains("<style>"), "inline CSS");
        assert!(!out.contains("<script"), "and no scripts at all");
        assert!(
            out.contains("<title>A &lt;title&gt;</title>"),
            "the title is escaped"
        );
        assert!(out.ends_with("</body>\n</html>\n"), "a closed document");
    }

    #[test]
    fn the_error_line_appears_only_when_set() {
        let mut clean = String::new();
        push_error(&mut clean, None);
        assert!(clean.is_empty(), "no message, no line");

        let mut failed = String::new();
        push_error(&mut failed, Some("<b>bad</b>"));
        assert_eq!(
            failed, "<p class=\"alert alert-danger\" role=\"alert\">&lt;b&gt;bad&lt;/b&gt;</p>\n",
            "the line shows, escaped"
        );
    }

    #[test]
    fn fields_escape_every_dynamic_value() {
        let mut out = String::new();
        push_field(&mut out, "i\"d", "n&me", "text", "<Label>");
        assert_eq!(
            out,
            "<div class=\"input\">\n<label for=\"i&quot;d\">&lt;Label&gt;</label>\n\
             <input type=\"text\" id=\"i&quot;d\" name=\"n&amp;me\">\n</div>\n",
            "id, name and label are escaped"
        );

        let mut hidden = String::new();
        push_hidden(&mut hidden, "name", "\"><script>");
        assert_eq!(
            hidden, "<input type=\"hidden\" name=\"name\" value=\"&quot;&gt;&lt;script&gt;\">\n",
            "a hidden value cannot open a tag"
        );
    }
}
