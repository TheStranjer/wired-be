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

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

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

/// What [`render_markdown`] asks `pulldown-cmark` for: plain `CommonMark` plus
/// strikethrough, so tables, footnotes, task lists, math and heading
/// attributes are never parsed and their markup stays ordinary text.
const MARKDOWN_OPTIONS: Options = Options::ENABLE_STRIKETHROUGH;

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
    push_open_link(url, "", out);
    push_url_display(url, out);
    out.push_str("</a>");
}

/// Appends an opening `<a>` for `href`: the escaped URL as `href`, the
/// escaped `title` when there is one, and the forced [`LINK_REL`]. The link's
/// content and its `</a>` are the caller's job.
fn push_open_link(href: &str, title: &str, out: &mut String) {
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

/// [`render_markdown`] without the sanitizer pass: the `pulldown-cmark` event
/// stream for `text`, rendered to HTML by [`MarkdownRenderer`].
fn markdown_to_html(text: &str) -> String {
    let mut renderer = MarkdownRenderer::default();
    for event in Parser::new_ext(text, MARKDOWN_OPTIONS) {
        renderer.on_event(event);
    }
    // A document can end on a text run, with no further event to flush it.
    renderer.flush_text();
    renderer.out
}

/// The open tag of a heading level. [`sanitize_html`] keeps `h1` to `h5`, so a
/// sixth-level heading comes back out as plain text.
const fn heading_open(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "<h1>",
        HeadingLevel::H2 => "<h2>",
        HeadingLevel::H3 => "<h3>",
        HeadingLevel::H4 => "<h4>",
        HeadingLevel::H5 => "<h5>",
        HeadingLevel::H6 => "<h6>",
    }
}

/// The close tag matching [`heading_open`].
const fn heading_close(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "</h1>",
        HeadingLevel::H2 => "</h2>",
        HeadingLevel::H3 => "</h3>",
        HeadingLevel::H4 => "</h4>",
        HeadingLevel::H5 => "</h5>",
        HeadingLevel::H6 => "</h6>",
    }
}

/// Where [`markdown_to_html`] stands in one document: the output so far plus
/// the context an event's rendering depends on.
#[derive(Debug, Default)]
struct MarkdownRenderer {
    /// The HTML produced so far.
    out: String,
    /// Consecutive [`Event::Text`] runs, not yet rendered. The parser ends a
    /// text run at every entity reference and every possible emphasis
    /// delimiter, so merging them is what lets a bare URL be found here just
    /// as [`text_to_html`] finds it in the same line.
    pending_text: String,
    /// Whether the current event is inside a code block, whose text is
    /// escaped but never linkified.
    in_code_block: bool,
    /// How many links are open. Their text is not linkified: an anchor may
    /// not contain another one.
    link_depth: usize,
    /// One entry per open image, innermost last: the byte offset in `out`
    /// where its display text starts and the URL-derived display to fall back
    /// to when the image has no alt text at all.
    open_images: Vec<(usize, String)>,
}

impl MarkdownRenderer {
    /// Renders one parser event into [`Self::out`]. Text is only buffered
    /// (see [`Self::pending_text`]); every other event first flushes what was
    /// buffered, so a run is always rendered under the state it was parsed in
    /// and lands in the output where it was written.
    fn on_event(&mut self, event: Event<'_>) {
        match event {
            Event::Text(text) => self.pending_text.push_str(&text),
            Event::Start(tag) => {
                self.flush_text();
                self.on_start(&tag);
            }
            Event::End(end) => {
                self.flush_text();
                self.on_end(end);
            }
            Event::Code(code) => {
                self.flush_text();
                self.on_code(&code);
            }
            // Raw HTML is only ever text here.
            Event::Html(html) | Event::InlineHtml(html) => {
                self.flush_text();
                self.out.push_str(&escape_html(&html));
            }
            // Pleroma shows a soft break as a line break, not as whitespace.
            Event::SoftBreak | Event::HardBreak => {
                self.flush_text();
                self.out.push_str("<br/>");
            }
            // Footnote references, math and task markers belong to extensions
            // [`MARKDOWN_OPTIONS`] does not enable, and a thematic break has
            // no element in the markup policy: none of them renders anything.
            Event::FootnoteReference(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::TaskListMarker(_)
            | Event::Rule => self.flush_text(),
        }
    }

    /// Renders the opening of an element.
    fn on_start(&mut self, tag: &Tag<'_>) {
        match tag {
            Tag::Paragraph => self.out.push_str("<p>"),
            Tag::Heading { level, .. } => self.out.push_str(heading_open(*level)),
            Tag::BlockQuote(_) => self.out.push_str("<blockquote>"),
            Tag::CodeBlock(_) => {
                self.in_code_block = true;
                self.out.push_str("<pre><code>");
            }
            Tag::List(Some(_)) => self.out.push_str("<ol>"),
            Tag::List(None) => self.out.push_str("<ul>"),
            Tag::Item => self.out.push_str("<li>"),
            Tag::Emphasis => self.out.push_str("<em>"),
            Tag::Strong => self.out.push_str("<strong>"),
            Tag::Strikethrough => self.out.push_str("<del>"),
            Tag::Link {
                dest_url, title, ..
            } => {
                push_open_link(dest_url, title, &mut self.out);
                self.link_depth += 1;
            }
            Tag::Image {
                dest_url, title, ..
            } => {
                // An image is a link: same anchor, and its alt text is the
                // link's visible text.
                push_open_link(dest_url, title, &mut self.out);
                let display = link_display(dest_url);
                self.open_images.push((self.out.len(), display));
                self.link_depth += 1;
            }
            // An HTML block renders through the escaped [`Event::Html`] runs
            // it contains, and the remaining tags belong to extensions
            // [`MARKDOWN_OPTIONS`] does not enable, so they never arrive.
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::Subscript
            | Tag::Superscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    /// Renders the closing of an element, mirroring [`Self::on_start`].
    fn on_end(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph => self.out.push_str("</p>"),
            TagEnd::Heading(level) => self.out.push_str(heading_close(level)),
            TagEnd::BlockQuote(_) => self.out.push_str("</blockquote>"),
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.out.push_str("</code></pre>");
            }
            TagEnd::List(true) => self.out.push_str("</ol>"),
            TagEnd::List(false) => self.out.push_str("</ul>"),
            TagEnd::Item => self.out.push_str("</li>"),
            TagEnd::Emphasis => self.out.push_str("</em>"),
            TagEnd::Strong => self.out.push_str("</strong>"),
            TagEnd::Strikethrough => self.out.push_str("</del>"),
            TagEnd::Link => {
                self.link_depth -= 1;
                self.out.push_str("</a>");
            }
            TagEnd::Image => self.close_image(),
            // See the matching arm in [`Self::on_start`].
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::Subscript
            | TagEnd::Superscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    /// Emits the buffered text run: always escaped, and linkified only where a
    /// bare URL may become a link — outside code blocks and outside links. An
    /// empty run (the common case: the flush that opens or closes an element)
    /// adds nothing.
    fn flush_text(&mut self) {
        if self.in_code_block || self.link_depth > 0 {
            self.out.push_str(&escape_html(&self.pending_text));
        } else {
            push_line_html(&self.pending_text, &mut self.out);
        }
        self.pending_text.clear();
    }

    /// Appends an inline code span: escaped text in `<code>`, never linkified.
    fn on_code(&mut self, code: &str) {
        self.out.push_str("<code>");
        self.out.push_str(&escape_html(code));
        self.out.push_str("</code>");
    }

    /// Closes the anchor [`Self::on_start`] opened for an image. An image with
    /// no alt text has added nothing since its `<a>`; it shows its URL the way
    /// a bare URL does.
    fn close_image(&mut self) {
        self.link_depth -= 1;
        if let Some((start, display)) = self.open_images.pop()
            && self.out.len() == start
        {
            self.out.push_str(&escape_html(&display));
        }
        self.out.push_str("</a>");
    }
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
    fn markdown_renders_block_and_inline_markup() {
        assert_eq!(
            render_markdown("**bold** and *it* and `code`"),
            "<p><strong>bold</strong> and <em>it</em> and <code>code</code></p>",
            "strong, emphasis and a code span"
        );
        assert_eq!(
            render_markdown("a > b & c `x < y & z`"),
            "<p>a &gt; b &amp; c <code>x &lt; y &amp; z</code></p>",
            "text is escaped and so is code-span content"
        );
        assert_eq!(
            render_markdown("> quoted"),
            "<blockquote><p>quoted</p></blockquote>",
            "a block quote keeps its paragraph"
        );
        assert_eq!(
            render_markdown("- one\n- two"),
            "<ul><li>one</li><li>two</li></ul>",
            "a tight bullet list has no paragraphs in its items"
        );
        assert_eq!(
            render_markdown("1. one\n2. two"),
            "<ol><li>one</li><li>two</li></ol>",
            "an ordered list is an ol"
        );
        assert_eq!(
            render_markdown("- one\n\n- two"),
            "<ul><li><p>one</p></li><li><p>two</p></li></ul>",
            "a loose list wraps its items"
        );
        assert_eq!(render_markdown("# Title"), "<h1>Title</h1>", "atx heading");
        for (marker, tag) in [
            ("#", "h1"),
            ("##", "h2"),
            ("###", "h3"),
            ("####", "h4"),
            ("#####", "h5"),
        ] {
            assert_eq!(
                render_markdown(&format!("{marker} Title")),
                format!("<{tag}>Title</{tag}>"),
                "heading level {marker} keeps its level"
            );
        }
        assert_eq!(
            render_markdown("Setext\n======"),
            "<h1>Setext</h1>",
            "a setext heading is a heading too"
        );
        assert_eq!(
            render_markdown("###### Sixth"),
            "Sixth",
            "h6 is outside the markup policy, so only its text survives"
        );
        assert_eq!(
            render_markdown("first\n\nsecond"),
            "<p>first</p><p>second</p>",
            "blank lines split paragraphs, as in plain text"
        );
        assert_eq!(
            render_markdown("```\nlet x = 1;\n```"),
            "<pre><code>let x = 1;\n</code></pre>",
            "a fenced code block is pre and code, its newline kept"
        );
        assert_eq!(
            render_markdown("    indented & <b>code</b>\n"),
            "<pre><code>indented &amp; &lt;b&gt;code&lt;/b&gt;\n</code></pre>",
            "an indented code block too, escaped and not linkified"
        );
    }

    #[test]
    fn markdown_enables_strikethrough_only() {
        assert_eq!(
            render_markdown("~~gone~~"),
            "<p><del>gone</del></p>",
            "strikethrough is on, spelled del as Pleroma spells it"
        );
        assert_eq!(
            render_markdown("a | b\n---|---\n1 | 2"),
            "<p>a | b<br>---|---<br>1 | 2</p>",
            "tables are off: a pipe table is three lines of text"
        );
        assert_eq!(
            render_markdown("see [^1]"),
            "<p>see [^1]</p>",
            "footnotes are off: a footnote reference is literal text"
        );
        assert_eq!(
            render_markdown("- [ ] task"),
            "<ul><li>[ ] task</li></ul>",
            "task lists are off too, so the box is text"
        );
    }

    #[test]
    fn markdown_breaks_lines_with_br() {
        assert_eq!(
            render_markdown("one\ntwo"),
            "<p>one<br>two</p>",
            "a soft break renders as a line break, not whitespace"
        );
        assert_eq!(
            render_markdown("one  \ntwo"),
            "<p>one<br>two</p>",
            "a hard break too"
        );
        assert_eq!(
            render_markdown("one\\\ntwo"),
            "<p>one<br>two</p>",
            "and the backslash spelling"
        );
        assert_eq!(
            render_markdown("first\r\nsecond"),
            "<p>first<br>second</p>",
            "a CRLF is one soft break"
        );
    }

    #[test]
    fn markdown_linkifies_bare_urls_like_plain_text() {
        assert_eq!(
            render_markdown("see https://example.com/a now"),
            text_to_html("see https://example.com/a now"),
            "the same anchor the plain-text formatter builds, rel included"
        );
        assert_eq!(
            render_markdown("see https://example.com/a now"),
            para_with_link("see ", "https://example.com/a", "example.com/a", " now"),
            "spelled out: scheme cut from the display, rel forced"
        );
        assert_eq!(
            render_markdown("https://example.com/a_(b) tail"),
            text_to_html("https://example.com/a_(b) tail"),
            "underscores split the parser's text runs, not the URL it linkifies"
        );
        assert_eq!(
            render_markdown("https://example.com/x?a=1&amp;b=2"),
            text_to_html("https://example.com/x?a=1&b=2"),
            "an escaped ampersand in the source is one URL, as in plain text"
        );
        assert_eq!(
            render_markdown("see https://example.com/way-too/long/path-that-keeps-going-here"),
            text_to_html("see https://example.com/way-too/long/path-that-keeps-going-here"),
            "the 30-character display cut and ellipsis apply here too"
        );
        assert_eq!(
            render_markdown("`https://example.com/a`"),
            "<p><code>https://example.com/a</code></p>",
            "no link inside a code span"
        );
        assert_eq!(
            render_markdown("```\nhttps://example.com/a\n```"),
            "<pre><code>https://example.com/a\n</code></pre>",
            "nor inside a code block"
        );
        assert_eq!(
            render_markdown("[https://example.com/a](https://example.com/b)"),
            format!(
                "<p>{}</p>",
                anchor("https://example.com/b", "https://example.com/a")
            ),
            "a URL that is a link's text stays text: no anchor inside an anchor"
        );
    }

    #[test]
    fn markdown_links_keep_their_own_text() {
        assert_eq!(
            render_markdown("[text](https://example.com/a)"),
            format!("<p>{}</p>", anchor("https://example.com/a", "text")),
            "href from the target, rel forced, the label as text"
        );
        assert_eq!(
            render_markdown(r#"[text](https://example.com/a "a title")"#),
            format!(r#"<p><a href="https://example.com/a" title="a title" {REL}>text</a></p>"#),
            "a markdown title becomes the title attribute"
        );
        assert_eq!(
            render_markdown("<https://example.com/a>"),
            format!(
                "<p>{}</p>",
                anchor("https://example.com/a", "https://example.com/a")
            ),
            "a CommonMark autolink shows its full URL"
        );
        assert_eq!(
            render_markdown("[text](relative/path)"),
            r#"<p><a href="relative/path" rel="ugc nofollow noopener noreferrer">text</a></p>"#,
            "relative targets pass, like they do in the sanitizer"
        );
        assert_eq!(
            render_markdown("a [x] b\n\n[x]: https://example.com/a"),
            format!("<p>a {} b</p>", anchor("https://example.com/a", "x")),
            "a reference link resolves through the parser"
        );
    }

    #[test]
    fn markdown_images_become_links() {
        assert_eq!(
            render_markdown("![x](https://example.com/i.png)"),
            format!("<p>{}</p>", anchor("https://example.com/i.png", "x")),
            "alt text as the label, the URL as href, no img element"
        );
        assert_eq!(
            render_markdown("![*it*](https://example.com/i.png)"),
            "<p><a href=\"https://example.com/i.png\" rel=\"ugc nofollow noopener noreferrer\"><em>it</em></a></p>",
            "the alt text is rendered as inline markdown"
        );
        assert_eq!(
            render_markdown("![](https://example.com/i.png)"),
            format!(
                "<p>{}</p>",
                anchor("https://example.com/i.png", "example.com/i.png")
            ),
            "an image with no alt text shows its URL the way a bare URL does"
        );
        assert_eq!(
            render_markdown("![a ![b](https://example.com/in.png) c](https://example.com/out.png)"),
            format!(
                "<p><a href=\"https://example.com/out.png\" {REL}>a </a><a href=\"https://example.com/in.png\" {REL}>b</a> c</p>"
            ),
            "both images become links; the sanitizer flattens the nested anchor,\
             closing the outer link before the inner one opens"
        );
        assert!(
            !render_markdown("![x](https://example.com/i.png)").contains("<img"),
            "markdown never produces an img element"
        );
    }

    #[test]
    fn markdown_escaped_html_is_inert() {
        assert_eq!(
            render_markdown("<script>alert('xss')</script>"),
            "&lt;script&gt;alert('xss')&lt;/script&gt;",
            "an HTML block is text, never an element"
        );
        assert_eq!(
            render_markdown("hi <b>bold</b> <br> there"),
            "<p>hi &lt;b&gt;bold&lt;/b&gt; &lt;br&gt; there</p>",
            "inline HTML is escaped inside the paragraph too"
        );
        assert_eq!(
            render_markdown("a < b & c"),
            "<p>a &lt; b &amp; c</p>",
            "comparison and ampersand text stay inert"
        );
    }

    #[test]
    fn markdown_links_cannot_use_a_forbidden_scheme() {
        assert_eq!(
            render_markdown("[x](javascript:alert(1))"),
            format!("<p><a {REL}>x</a></p>"),
            "the sanitizer drops the javascript: href and the text survives"
        );
        assert_eq!(
            render_markdown("![x](javascript:alert(1))"),
            format!("<p><a {REL}>x</a></p>"),
            "same for an image whose URL is javascript:"
        );
        assert!(
            !render_markdown("[x](javascript:alert(1))").contains("javascript:"),
            "no javascript: href is ever produced"
        );
    }

    #[test]
    fn markdown_empty_input_is_empty() {
        assert_eq!(render_markdown(""), "", "nothing in, nothing out");
        assert_eq!(
            render_markdown("   "),
            "",
            "whitespace alone is no paragraph"
        );
        assert_eq!(
            render_markdown("\n\n   \n\t\n"),
            "",
            "not even blank lines make an element"
        );
        assert_eq!(
            render_markdown("---"),
            "",
            "a thematic break has no element in the markup policy"
        );
        assert_eq!(
            render_markdown("<!-- comment -->"),
            "&lt;!-- comment --&gt;",
            "an HTML comment is escaped text, not a comment"
        );
    }

    #[test]
    fn markdown_entity_references_cannot_smuggle_markup() {
        assert_eq!(
            render_markdown("AT&amp;T &copy; 100% &lt;b&gt;"),
            "<p>AT&amp;T © 100% &lt;b&gt;</p>",
            "a decoded reference is re-escaped: the source's &lt;b&gt; is text, not a tag"
        );
    }

    #[test]
    fn markdown_renders_a_whole_status() {
        let source = "# Release\n\nNotes for **v1**:\n\n- see https://example.com/a\n\
                      - [docs](https://example.com/docs \"Doc page\")\n\n\
                      > thanks to ~~everyone~~\n\n<script>bad()</script>\n";
        assert_eq!(
            render_markdown(source),
            "<h1>Release</h1>\
             <p>Notes for <strong>v1</strong>:</p>\
             <ul><li>see <a href=\"https://example.com/a\" rel=\"ugc nofollow noopener noreferrer\">example.com/a</a></li>\
             <li><a href=\"https://example.com/docs\" title=\"Doc page\" rel=\"ugc nofollow noopener noreferrer\">docs</a></li></ul>\
             <blockquote><p>thanks to <del>everyone</del></p></blockquote>\
             &lt;script&gt;bad()&lt;/script&gt;\n",
            "every construct composes: markup, lists, links, a bare URL and an inert script"
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
