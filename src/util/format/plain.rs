//! The plain-text formatter: newline normalization, paragraph and line
//! grouping, and the anchors a bare URL becomes.

use super::{
    DISPLAY_MAX_CHARS, ELLIPSIS, LINK_REL, truncate_chars,
    urls::{Piece, scheme_len, split_pieces},
};
use crate::util::sanitize::escape_html;

/// [`text_to_html`] without the sanitizer pass: escape, linkify and
/// paragraph, on newline-normalized text.
pub(super) fn plain_to_html(text: &str) -> String {
    let normalized = normalize_newlines(text);
    let mut out = String::new();
    for block in split_blocks(&normalized) {
        out.push_str("<p>");
        for (index, line) in block.iter().enumerate() {
            if index > 0 {
                out.push_str("<br/>");
            }
            push_line_html(line, &mut out);
        }
        out.push_str("</p>");
    }
    out
}

/// Rewrites `\r\n` and lone `\r` as `\n` so every split below only has to
/// consider LF.
pub(super) fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Groups newline-normalized text into blocks of consecutive non-blank
/// lines; a blank (whitespace-only) line closes the current block, and no
/// empty block is produced.
fn split_blocks(text: &str) -> Vec<Vec<&str>> {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        if line.trim().is_empty() {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(line);
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

/// Escapes the plain runs of one line and replaces its bare URLs with
/// anchors.
pub(super) fn push_line_html(line: &str, out: &mut String) {
    for piece in split_pieces(line) {
        match piece {
            Piece::Text(text) => out.push_str(&escape_html(text)),
            Piece::Url(url) => push_link(url, out),
        }
    }
}

/// Appends the anchor for one bare URL: the escaped URL as `href`, the
/// forced `rel`, and the escaped [`link_display`] as visible text.
fn push_link(url: &str, out: &mut String) {
    push_open_link(url, "", out);
    push_url_display(url, out);
    out.push_str("</a>");
}

/// Appends an opening `<a>` for `href`: the escaped URL as `href`, the
/// escaped `title` when there is one, and the forced [`LINK_REL`]. The link's
/// content and its `</a>` are the caller's job.
pub(super) fn push_open_link(href: &str, title: &str, out: &mut String) {
    out.push_str(r#"<a href=""#);
    out.push_str(&escape_html(href));
    out.push('"');
    if !title.is_empty() {
        out.push_str(r#" title=""#);
        out.push_str(&escape_html(title));
        out.push('"');
    }
    out.push(' ');
    out.push_str(LINK_REL);
    out.push('>');
}

/// Appends the visible text a bare URL gets: [`link_display`], escaped.
fn push_url_display(url: &str, out: &mut String) {
    out.push_str(&escape_html(&link_display(url)));
}

/// The visible text of a link: the URL without its scheme, cut to
/// [`DISPLAY_MAX_CHARS`] scalar values plus [`ELLIPSIS`] when longer.
pub(super) fn link_display(url: &str) -> String {
    let naked = without_scheme(url);
    let mut out = truncate_chars(naked, DISPLAY_MAX_CHARS);
    if naked.chars().count() > DISPLAY_MAX_CHARS {
        out.push(ELLIPSIS);
    }
    out
}

/// The URL text after its case-insensitive `http(s)://` scheme (a URL from
/// the scanner always has one).
fn without_scheme(url: &str) -> &str {
    let scheme = scheme_len(url, 0).unwrap_or_default();
    url.get(scheme..).unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::super::{
        test_support::{anchor, para_with_link},
        text_to_html,
    };

    #[test]
    fn escapes_markup_so_script_stays_inert() {
        assert_eq!(
            text_to_html("<script>alert('xss')</script>"),
            "<p>&lt;script&gt;alert('xss')&lt;/script&gt;</p>",
            "tag syntax survives as escaped text; no element is ever formed"
        );
        assert_eq!(
            text_to_html("a < b & c > d \"q\""),
            "<p>a &lt; b &amp; c &gt; d \"q\"</p>",
            "the sanitizer re-serializes text with < > & escaped and quotes literal"
        );
    }

    #[test]
    fn linkify_keeps_surrounding_text() {
        assert_eq!(
            para_with_link("See ", "https://example.com/a", "example.com/a", " now"),
            text_to_html("See https://example.com/a now"),
            "the scheme is cut from the display text, href keeps it"
        );
        assert_eq!(
            para_with_link("", "http://example.org", "example.org", ""),
            text_to_html("http://example.org"),
            "http works too"
        );
        assert_eq!(
            para_with_link("", "HTTPS://Example.COM/Path", "Example.COM/Path", ""),
            text_to_html("HTTPS://Example.COM/Path"),
            "scheme matching is case-insensitive and the rest keeps its case"
        );
    }

    #[test]
    fn linkify_supports_query_strings_and_fragments() {
        assert_eq!(
            text_to_html("https://example.com/s?q=a%20b&r=2#frag"),
            para_with_link(
                "",
                "https://example.com/s?q=a%20b&amp;r=2#frag",
                "example.com/s?q=a%20b&amp;r=2#frag",
                ""
            ),
            "query, percent-escapes and fragment stay inside the URL, & is escaped"
        );
        assert_eq!(
            text_to_html("a https://example.com/x?y=1&z=2 b"),
            para_with_link(
                "a ",
                "https://example.com/x?y=1&amp;z=2",
                "example.com/x?y=1&amp;z=2",
                " b"
            ),
            "a bare ampersand in a query round-trips as &amp; even mid-line"
        );
    }

    #[test]
    fn paragraphs_and_line_breaks() {
        assert_eq!(
            text_to_html("one\ntwo\n\nthree"),
            "<p>one<br>two</p><p>three</p>",
            "a single newline becomes <br>, a blank line splits <p>s"
        );
        assert_eq!(
            text_to_html("\n\nlead in\r\nwindows break\rtrailing\n\n\n"),
            "<p>lead in<br>windows break<br>trailing</p>",
            "CRLF and CR normalize, blank margins are dropped"
        );
        assert_eq!(
            text_to_html("a\n \nb"),
            "<p>a</p><p>b</p>",
            "a whitespace-only line still separates blocks"
        );
        assert_eq!(text_to_html(""), "", "empty text makes no paragraphs");
        assert_eq!(
            text_to_html("   \n\n  "),
            "",
            "nor does whitespace-only text"
        );
    }

    #[test]
    fn links_cross_paragraphs_and_breaks() {
        assert_eq!(
            text_to_html("a\nhttps://example.com/x\n\nb https://example.com/y"),
            format!(
                "<p>a<br>{}</p><p>b {}</p>",
                anchor("https://example.com/x", "example.com/x"),
                anchor("https://example.com/y", "example.com/y")
            ),
            "links render inside paragraphs and across breaks"
        );
    }

    #[test]
    fn unicode_survives_untouched() {
        assert_eq!(
            text_to_html("日本語です 🇯🇵"),
            "<p>日本語です 🇯🇵</p>",
            "non-ASCII text passes through as-is and never breaks the scanner"
        );
        assert_eq!(
            text_to_html("café ☕ https://例え.jp/あ"),
            "<p>café ☕ https://例え.jp/あ</p>",
            "non-ASCII right after the scheme means no URL body, so no link"
        );
        assert_eq!(
            text_to_html("über\n\n🎉 done https://emoji.example/🎉"),
            format!(
                "<p>über</p><p>🎉 done {}🎉</p>",
                anchor("https://emoji.example/", "emoji.example/")
            ),
            "the URL stops at the astral character, which stays plain text"
        );
    }
}
