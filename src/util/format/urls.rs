//! The linear-time bare-URL scanner: where a URL starts and ends inside
//! one line of text.

use super::{HTTP_SCHEME, HTTPS_SCHEME, TRAILING_PUNCTUATION};

/// A run of a line, in source order: either text to escape or a URL to
/// linkify.
pub(super) enum Piece<'a> {
    /// Text with no URL in it; it only needs escaping.
    Text(&'a str),
    /// A bare `http(s)://` URL.
    Url(&'a str),
}

/// The linear-time URL scanner: one left-to-right pass over `line`'s bytes
/// cutting it into plain-text and bare-URL pieces. Every boundary it
/// produces falls between ASCII bytes, hence at a char boundary.
pub(super) fn split_pieces(line: &str) -> Vec<Piece<'_>> {
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
pub(super) fn scheme_len(text: &str, at: usize) -> Option<usize> {
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
    use super::super::{test_support::para_with_link, text_to_html};

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
}
