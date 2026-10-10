//! Turning user-supplied content into the sanitized HTML a client renders.
//!
//! [`text_to_html`] is the plain-text formatter, modelled on Pleroma's
//! `format_input(text, "text/plain", …)` (`lib/pleroma/web/common_api/utils.ex`)
//! plus its `Formatter.linkify` step (`lib/pleroma/formatter.ex`): escape the
//! text, wrap bare `http(s)`-scheme URLs in anchors whose visible text is the
//! scheme-less URL cut to 30 characters, break blocks on
//! blank lines into `<p>` elements and single newlines into `<br/>` — then run
//! the result through [`sanitize_html`]. [`render_content`] picks the pipeline
//! for a MIME content type, [`render_markdown`] is the not-yet-real markdown
//! hook, [`truncate_chars`] cuts text by Unicode scalar value and
//! [`status_length`] measures a status the way clients count it, charging each
//! bare URL a fixed reserved length.
//!
//! URL detection is the hand-written byte scanner in `split_pieces`: it
//! walks a line once, `O(n)`, so there is no regex with catastrophic
//! backtracking anywhere in this module.

use crate::util::sanitize::{escape_html, sanitize_html};

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
/// A stub: the real markdown pipeline arrives in a later ticket, and until
/// then this delegates to [`text_to_html`].
#[must_use]
pub fn render_markdown(text: &str) -> String {
    text_to_html(text)
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

/// [`text_to_html`] without the sanitizer pass: escape, linkify and
/// paragraph, on newline-normalized text.
fn plain_to_html(text: &str) -> String {
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
fn normalize_newlines(text: &str) -> String {
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
fn push_line_html(line: &str, out: &mut String) {
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
    out.push_str(r#"<a href=""#);
    out.push_str(&escape_html(url));
    out.push_str("\" ");
    out.push_str(LINK_REL);
    out.push('>');
    out.push_str(&escape_html(&link_display(url)));
    out.push_str("</a>");
}

/// The visible text of a link: the URL without its scheme, cut to
/// [`DISPLAY_MAX_CHARS`] scalar values plus [`ELLIPSIS`] when longer.
fn link_display(url: &str) -> String {
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

/// A run of a line, in source order: either text to escape or a URL to
/// linkify.
enum Piece<'a> {
    /// Text with no URL in it; it only needs escaping.
    Text(&'a str),
    /// A bare `http(s)://` URL.
    Url(&'a str),
}

/// The linear-time URL scanner: one left-to-right pass over `line`'s bytes
/// cutting it into plain-text and bare-URL pieces. Every boundary it
/// produces falls between ASCII bytes, hence at a char boundary.
fn split_pieces(line: &str) -> Vec<Piece<'_>> {
    let bytes = line.as_bytes();
    let mut pieces: Vec<Piece<'_>> = Vec::new();
    let mut plain_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if let Some(end) = url_candidate(line, i) {
            if i > plain_start {
                pieces.push(Piece::Text(span(line, plain_start, i)));
            }
            pieces.push(Piece::Url(span(line, i, end)));
            plain_start = end;
            i = end;
            continue;
        }
        i += 1;
    }
    if plain_start < bytes.len() {
        pieces.push(Piece::Text(span(line, plain_start, bytes.len())));
    }
    pieces
}

/// The byte index just past a bare URL starting at `at`, or `None` if the
/// text there is not a URL. A candidate needs a scheme and at least one
/// body character; [`trim_url_end`] then pulls the end back over trailing
/// punctuation and unbalanced `)`, and a URL trimmed away entirely yields
/// `None`.
fn url_candidate(line: &str, at: usize) -> Option<usize> {
    let scheme = scheme_len(line, at)?;
    let bytes = line.as_bytes();
    let body_start = at + scheme;
    let mut end = body_start;
    while bytes.get(end).copied().is_some_and(is_url_byte) {
        end += 1;
    }
    if end == body_start {
        return None;
    }
    let trimmed = trim_url_end(line, body_start, end);
    (trimmed > body_start).then_some(trimmed)
}

/// The length of the case-insensitive `http(s)://` scheme starting at
/// `at`, or `None` if no scheme starts there. Bytes past the end of the
/// string are simply missing, so the match fails.
fn scheme_len(text: &str, at: usize) -> Option<usize> {
    let rest = text.get(at..)?;
    if rest
        .get(..HTTPS_SCHEME.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(HTTPS_SCHEME))
    {
        return Some(HTTPS_SCHEME.len());
    }
    if rest
        .get(..HTTP_SCHEME.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(HTTP_SCHEME))
    {
        return Some(HTTP_SCHEME.len());
    }
    None
}

/// Whether byte `b` may continue a bare URL: the RFC 3986 alphanumerics,
/// the `-._~%` characters and `:/?#[]@!$&'()*+,;=`. Everything else —
/// whitespace, `<`, `>`, `"`, backtick, `\`, `{|}` and all non-ASCII —
/// ends a URL.
const fn is_url_byte(b: u8) -> bool {
    matches!(
        b,
        b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'%'
            | b':'
            | b'/'
            | b'?'
            | b'#'
            | b'['
            | b']'
            | b'@'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
    )
}

/// Pulls the end of a candidate URL back over trailing characters that are
/// prose, not part of the link: any run of [`TRAILING_PUNCTUATION`], and a
/// `)` for which the body holds no unmatched `(`.
fn trim_url_end(line: &str, body_start: usize, mut end: usize) -> usize {
    let bytes = line.as_bytes();
    // The scanner only accepts ASCII URL bytes, so the body is ASCII and
    // counting its parentheses by character is exact.
    let body = span(line, body_start, end);
    let opens = body.matches('(').count();
    let mut closes = body.matches(')').count();
    while let Some(last) = end.checked_sub(1).and_then(|i| bytes.get(i)) {
        if TRAILING_PUNCTUATION.contains(last) {
            end -= 1;
        } else if *last == b')' && closes > opens {
            end -= 1;
            closes -= 1;
        } else {
            break;
        }
    }
    end
}

/// The substring between two byte positions that are always char
/// boundaries at the call sites (each falls between an ASCII URL byte and
/// any byte); `get` keeps an impossible range inert instead of panicking.
fn span(text: &str, from: usize, to: usize) -> &str {
    text.get(from..to).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `rel` as it appears in sanitized output.
    const REL: &str = r#"rel="ugc nofollow noopener noreferrer""#;

    /// Builds the expected anchor for a URL whose display needs no escaping
    /// beyond what the caller already put in `href` and `display`.
    fn anchor(href: &str, display: &str) -> String {
        format!(r#"<a href="{href}" {REL}>{display}</a>"#)
    }

    /// Builds the expected paragraph holding a linkified URL: `href` and
    /// `display` with the scheme-less URL (or its truncation) in them.
    fn para_with_link(before: &str, url: &str, display: &str, after: &str) -> String {
        format!("<p>{before}{}{after}</p>", anchor(url, display))
    }

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
    fn linkify_excludes_trailing_punctuation() {
        assert_eq!(
            text_to_html("Visit https://example.com/page."),
            para_with_link(
                "Visit ",
                "https://example.com/page",
                "example.com/page",
                "."
            ),
            "the sentence period is not part of the URL"
        );
        for punct in [',', ';', ':', '!', '?'] {
            assert_eq!(
                text_to_html(&format!("https://example.com/a{punct}")),
                para_with_link(
                    "",
                    "https://example.com/a",
                    "example.com/a",
                    &format!("{punct}")
                ),
                "trailing {punct} is prose"
            );
        }
    }

    #[test]
    fn linkify_handles_parentheses() {
        assert_eq!(
            text_to_html("https://en.wikipedia.org/wiki/Rust_(language)"),
            para_with_link(
                "",
                "https://en.wikipedia.org/wiki/Rust_(language)",
                "en.wikipedia.org/wiki/Rust_(la…",
                ""
            ),
            "a URL ending in a balanced ) keeps it (display is cut to 30 chars plus …)"
        );
        assert_eq!(
            text_to_html("see https://example.com/a)"),
            para_with_link("see ", "https://example.com/a", "example.com/a", ")"),
            "an unmatched ) is trimmed off the end"
        );
        assert_eq!(
            text_to_html("https://example.com/a(b)) end"),
            para_with_link("", "https://example.com/a(b)", "example.com/a(b)", ") end"),
            "one ) goes with the (, the extra one is prose"
        );
        assert_eq!(
            text_to_html("check https://x.example/a_(b). done"),
            para_with_link(
                "check ",
                "https://x.example/a_(b)",
                "x.example/a_(b)",
                ". done"
            ),
            "the period is trimmed, then the now-unmatched )"
        );
    }

    #[test]
    fn long_url_display_is_cut_with_ellipsis() {
        assert_eq!(
            text_to_html("see https://example.com/way-too/long/path-that-keeps-going-and-going"),
            para_with_link(
                "see ",
                "https://example.com/way-too/long/path-that-keeps-going-and-going",
                "example.com/way-too/long/path-…",
                ""
            ),
            "the display is 30 scalar values plus an ellipsis"
        );
        assert_eq!(
            text_to_html("https://example.com/way-too/long/paths"),
            para_with_link(
                "",
                "https://example.com/way-too/long/paths",
                "example.com/way-too/long/paths",
                ""
            ),
            "exactly 30 display characters gets no ellipsis"
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

    #[test]
    fn bare_scheme_or_other_text_is_not_a_url() {
        assert_eq!(
            text_to_html("https://"),
            "<p>https://</p>",
            "a scheme alone is not a link"
        );
        assert_eq!(
            text_to_html("https://."),
            "<p>https://.</p>",
            "a body that is only trimmed punctuation is no link either"
        );
        assert_eq!(
            text_to_html("example.com/path"),
            "<p>example.com/path</p>",
            "only http(s) URLs are linkified"
        );
        assert_eq!(
            text_to_html("ftp://example.com/f"),
            "<p>ftp://example.com/f</p>",
            "other schemes stay bare text"
        );
    }

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
            render_content("# not a heading", "text/markdown"),
            "<p># not a heading</p>",
            "markdown is the text_to_html stub until its own ticket"
        );
        assert_eq!(
            render_markdown("a\n\nb"),
            text_to_html("a\n\nb"),
            "the stub and the plain-text formatter agree"
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
