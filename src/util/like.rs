//! The one escaper for SQL `LIKE`/`ILIKE` patterns.
//!
//! Every search that builds a pattern from user text goes through here, so
//! `%`, `_` and the escape character `\` in a query match themselves rather
//! than acting as wildcards, and every search treats them the same way.

/// Backslash-escapes `%`, `_` and `\` in `text`, so the result matches
/// `text` literally inside a `LIKE`/`ILIKE` pattern. The caller adds any
/// wildcard `%` it wants around it.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    push_escaped(&mut escaped, text);
    escaped
}

/// The prefix pattern of `text`: its [`escape`]d form followed by one `%`.
#[must_use]
pub fn prefix(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() + 1);
    push_escaped(&mut pattern, text);
    pattern.push('%');
    pattern
}

/// The substring pattern of `text`: `%`, its [`escape`]d form, then `%`.
#[must_use]
pub fn contains(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() + 2);
    pattern.push('%');
    push_escaped(&mut pattern, text);
    pattern.push('%');
    pattern
}

/// Appends `text` to `out`, backslash-escaping each pattern metacharacter.
fn push_escaped(out: &mut String, text: &str) {
    for symbol in text.chars() {
        if matches!(symbol, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(symbol);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_quotes_percent_underscore_and_backslash() {
        assert_eq!(
            escape(r"a%b_c\d"),
            r"a\%b\_c\\d",
            "every wildcard is quoted"
        );
        assert_eq!(escape("plain/text"), "plain/text", "nothing to escape");
        assert_eq!(escape(""), "", "an empty text stays empty");
    }

    #[test]
    fn prefix_appends_one_percent() {
        assert_eq!(prefix("bob"), "bob%", "the plain case");
        assert_eq!(prefix("50%"), r"50\%%", "a percent stays literal");
        assert_eq!(prefix("a_b"), r"a\_b%", "an underscore stays literal");
        assert_eq!(prefix("a\\b"), r"a\\b%", "a backslash is doubled");
        assert_eq!(prefix(""), "%", "an empty text matches everything");
    }

    #[test]
    fn contains_wraps_in_percents() {
        assert_eq!(contains("alice"), "%alice%", "an ordinary text");
        assert_eq!(
            contains("50%_off"),
            r"%50\%\_off%",
            "wildcards stay literal"
        );
        assert_eq!(contains(""), "%%", "an empty text matches all");
    }
}
