//! Expected-markup builders the formatter's unit tests share.

/// The `rel` as it appears in sanitized output.
pub(super) const REL: &str = r#"rel="ugc nofollow noopener noreferrer""#;

/// Builds the expected anchor for a URL whose display needs no escaping
/// beyond what the caller already put in `href` and `display`.
pub(super) fn anchor(href: &str, display: &str) -> String {
    format!(r#"<a href="{href}" {REL}>{display}</a>"#)
}

/// Builds the expected paragraph holding a linkified URL: `href` and
/// `display` with the scheme-less URL (or its truncation) in them.
pub(super) fn para_with_link(before: &str, url: &str, display: &str, after: &str) -> String {
    format!("<p>{before}{}{after}</p>", anchor(url, display))
}
