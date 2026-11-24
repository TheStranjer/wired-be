//! Turning user-supplied content into the sanitized HTML a client renders.
//!
//! [`text_to_html`] is the plain-text formatter, modelled on Pleroma's
//! `format_input(text, "text/plain", …)` (`lib/pleroma/web/common_api/utils.ex`)
//! plus its `Formatter.linkify` step (`lib/pleroma/formatter.ex`): escape the
//! text, wrap bare `http(s)`-scheme URLs in anchors whose visible text is the
//! scheme-less URL cut to 30 characters, break blocks on
//! blank lines into `<p>` elements and single newlines into `<br/>` — then run
//! the result through [`sanitize_html`]. [`render_content`] picks the pipeline
//! for a MIME content type, [`render_markdown`] is the `text/markdown`
//! formatter, [`truncate_chars`] cuts text by Unicode scalar value and
//! [`status_length`] measures a status the way clients count it, charging each
//! bare URL a fixed reserved length.
//!
//! URL detection is the hand-written byte scanner in `split_pieces`: it
//! walks a line once, `O(n)`, so there is no regex with catastrophic
//! backtracking anywhere in this module. It serves the plain-text formatter
//! and, on each text run, the markdown one.
//!
//! Markdown is `pulldown-cmark`'s event stream rendered by the module's
//! `MarkdownRenderer`, the counterpart of Pleroma's `Earmark.as_html!` call
//! in `Formatter.markdown_to_html` (`lib/pleroma/formatter.ex`) followed by
//! `Formatter.linkify` and the tag scrubber. Where that leaves room for choice
//! the behaviour is fixed here: raw HTML in the source is escaped instead of
//! passed through, soft line breaks become `<br/>`, an inline image becomes a
//! plain link, and the finished markup is sanitized like every other format.

use markdown::markdown_to_html;
use plain::{normalize_newlines, plain_to_html, plain_to_inline_html};
use urls::{Piece, split_pieces};

use crate::util::sanitize::sanitize_html;

mod markdown;
mod plain;
mod renderer;
#[cfg(test)]
mod test_support;
pub mod urls;

/// The `rel` attribute every linkified URL carries, spelled exactly as
/// [`sanitize_html`] will force it so the sanitizer pass changes nothing.
const LINK_REL: &str = r#"rel="ugc nofollow noopener noreferrer""#;

/// Maximum number of scalar values in a link's display text.
const DISPLAY_MAX_CHARS: usize = 30;

/// Appended to display text that was cut at [`DISPLAY_MAX_CHARS`].
const ELLIPSIS: char = '…';

/// Bytes stripped from the end of a candidate URL as trailing punctuation.
const TRAILING_PUNCTUATION: [u8; 6] = *b".,;:!?";

/// The `http://` scheme text, lower-case.
const HTTP_SCHEME: &str = "http://";

/// The `https://` scheme text, lower-case.
const HTTPS_SCHEME: &str = "https://";

/// Formats plain text as sanitized paragraph HTML.
///
/// The text is escaped (so markup like `<script>` arrives as inert text),
/// bare `http(s)://` URLs become anchors, blank-line separated blocks become
/// `<p>` elements and single newlines inside a block become `<br/>`; the
/// result is sanitized with [`sanitize_html`], which serializes each `<br/>`
/// as `<br>`. Trailing `.,;:!?` and a `)` whose `(` is missing are not taken
/// into a URL.
#[must_use]
pub fn text_to_html(text: &str) -> String {
    sanitize_html(&plain_to_html(text))
}

/// Formats plain text as sanitized inline HTML, with no `<p>` wrapper.
///
/// Escaping, URL anchors and sanitizing are [`text_to_html`]'s; newlines
/// become `<br>` (a blank line, two) instead of paragraphs. It is the
/// shape Mastodon gives a profile field value.
#[must_use]
pub fn text_to_inline_html(text: &str) -> String {
    sanitize_html(&plain_to_inline_html(text))
}

/// Formats user content by MIME type into sanitized HTML.
///
/// `text/html` is sanitized as given, `text/markdown` goes through
/// [`render_markdown`], and everything else — `text/plain` or an unknown or
/// empty type — is treated as plain text with [`text_to_html`].
#[must_use]
pub fn render_content(text: &str, content_type: &str) -> String {
    match content_type {
        "text/html" => sanitize_html(text),
        "text/markdown" => render_markdown(text),
        _ => text_to_html(text),
    }
}

/// Renders `text/markdown` content to sanitized HTML.
///
/// The source is `CommonMark` with strikethrough enabled and tables and
/// footnotes disabled (the parser options are `MARKDOWN_OPTIONS`). On top of
/// the parser's structure: raw HTML in the source is escaped, so it can never
/// form an element; soft line breaks become `<br/>` the way Pleroma renders
/// them; an inline image becomes a plain link instead of an `<img>` element;
/// bare `http(s)` URLs in text are linkified exactly as [`text_to_html`] does;
/// and the result goes through [`sanitize_html`], which is what makes the
/// remaining escaping and the URL scheme policy final (it also flattens an
/// anchor nested inside another anchor). Empty or whitespace-only input
/// produces an empty string.
#[must_use]
pub fn render_markdown(text: &str) -> String {
    sanitize_html(&markdown_to_html(text))
}

/// Cuts `s` to at most `max` Unicode scalar values, never splitting a char.
#[must_use]
pub fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Measures a status the way a client counts it: each bare `http(s)` URL
/// found by the [`text_to_html`] scanner counts as `url_reserved`
/// characters, every other scalar value (newlines included) counts as one.
#[must_use]
pub fn status_length(text: &str, url_reserved: usize) -> usize {
    let normalized = normalize_newlines(text);
    let mut total = 0;
    for line in normalized.split('\n') {
        for piece in split_pieces(line) {
            match piece {
                Piece::Text(plain) => total += plain.chars().count(),
                Piece::Url(_) => total += url_reserved,
            }
        }
    }
    total + normalized.chars().filter(|ch| *ch == '\n').count()
}

#[cfg(test)]
mod tests {
    use super::test_support::REL;
    use super::*;

    #[test]
    fn html_content_type_is_sanitized_only() {
        assert_eq!(
            render_content("<p>hi</p><script>bad()</script>", "text/html"),
            "<p>hi</p>",
            "text/html keeps its markup and loses its script"
        );
        assert_eq!(
            render_content(r#"<a href="javascript:alert(1)">click</a>"#, "text/html"),
            format!("<a {REL}>click</a>"),
            "no linkify pass, but the scheme policy still applies"
        );
        assert_eq!(
            render_content("plain & simple", "text/html"),
            "plain &amp; simple",
            "a bare ampersand becomes inert"
        );
    }

    #[test]
    fn content_type_dispatch_with_fallbacks() {
        assert_eq!(
            render_content("a\n\nb", "text/plain"),
            "<p>a</p><p>b</p>",
            "text/plain is the full plain-text pipeline"
        );
        assert_eq!(
            render_content("a\nb", "application/octet-stream"),
            "<p>a<br>b</p>",
            "unknown types fall back to plain text"
        );
        assert_eq!(
            render_content("x", ""),
            "<p>x</p>",
            "an empty content type falls back too"
        );
        assert_eq!(
            render_content("a&b", "text/plain; charset=utf-8"),
            "<p>a&amp;b</p>",
            "a parameterised text/plain is not text/html, so it is formatted as plain text"
        );
        assert_eq!(
            render_content("# heading", "text/markdown"),
            "<h1>heading</h1>",
            "text/markdown goes to the markdown formatter, not the plain-text one"
        );
        assert_eq!(
            render_content("**hi**", "text/markdown"),
            render_markdown("**hi**"),
            "the dispatch is render_markdown itself"
        );
    }

    #[test]
    fn truncate_counts_scalar_values_and_never_splits_chars() {
        assert_eq!(
            truncate_chars("hello", 10),
            "hello",
            "short input is unchanged"
        );
        assert_eq!(
            truncate_chars("hello", 5),
            "hello",
            "exactly max is unchanged"
        );
        assert_eq!(truncate_chars("hello", 2), "he", "ascii cut");
        assert_eq!(
            truncate_chars("héllo", 2),
            "hé",
            "a combining-free é is one char"
        );
        assert_eq!(
            truncate_chars("e\u{301}x", 2),
            "e\u{301}",
            "e plus combining acute is two scalars, kept together by 2"
        );
        assert_eq!(
            truncate_chars("e\u{301}x", 1),
            "e",
            "cutting at 1 splits after the base char, never mid-scalar"
        );
        assert_eq!(
            truncate_chars("🇯🇵🌍x", 2),
            "🇯🇵",
            "a flag is its two regional indicators, four bytes"
        );
        assert_eq!(
            truncate_chars("👍ok", 1),
            "👍",
            "an astral char is one scalar"
        );
        assert_eq!(truncate_chars("", 3), "", "empty stays empty");
    }

    #[test]
    fn status_length_charges_urls_a_fixed_cost() {
        assert_eq!(
            status_length("hello", 23),
            5,
            "plain text counts characters"
        );
        assert_eq!(
            status_length("https://example.com/a/very/long/path?x=1&y=2", 23),
            23,
            "one URL costs exactly the reserved length"
        );
        assert_eq!(
            status_length("See https://example.com/x done", 23),
            4 + 23 + 5,
            "text around the URL counts normally"
        );
        assert_eq!(
            status_length("short https://a.io", 23),
            6 + 23,
            "even a URL shorter than the reserve counts as the reserve"
        );
        assert_eq!(status_length("a\nb", 23), 3, "a newline is one character");
        assert_eq!(
            status_length("a\r\nb", 23),
            3,
            "CRLF counts as the one newline it makes"
        );
        assert_eq!(
            status_length("日本🌍", 23),
            3,
            "multibyte text counts scalars"
        );
        assert_eq!(
            status_length("https://a.io https://b.io", 23),
            23 + 1 + 23,
            "every URL gets the reserve"
        );
        assert_eq!(
            status_length("Visit https://example.com/page.", 23),
            6 + 23 + 1,
            "trimmed trailing punctuation counts as text"
        );
        assert_eq!(status_length("", 23), 0, "empty has no length");
    }
}
