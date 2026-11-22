//! The Markdown entry point: the parser options, the event loop that
//! feeds the renderer, and the heading tags.

use pulldown_cmark::{HeadingLevel, Options, Parser};

use super::renderer::MarkdownRenderer;

/// What [`render_markdown`] asks `pulldown-cmark` for: plain `CommonMark` plus
/// strikethrough, so tables, footnotes, task lists, math and heading
/// attributes are never parsed and their markup stays ordinary text.
const MARKDOWN_OPTIONS: Options = Options::ENABLE_STRIKETHROUGH;

/// [`render_markdown`] without the sanitizer pass: the `pulldown-cmark` event
/// stream for `text`, rendered to HTML by [`MarkdownRenderer`].
pub(super) fn markdown_to_html(text: &str) -> String {
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
pub(super) const fn heading_open(level: HeadingLevel) -> &'static str {
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
pub(super) const fn heading_close(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "</h1>",
        HeadingLevel::H2 => "</h2>",
        HeadingLevel::H3 => "</h3>",
        HeadingLevel::H4 => "</h4>",
        HeadingLevel::H5 => "</h5>",
        HeadingLevel::H6 => "</h6>",
    }
}

#[cfg(test)]
mod tests {
    use super::super::render_markdown;

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
}
