//! The event-driven Markdown renderer: where one document stands while
//! `pulldown-cmark`'s events are turned into escaped, linkified HTML.

use pulldown_cmark::{Event, Tag, TagEnd};

use super::{
    markdown::{heading_close, heading_open},
    plain::{link_display, push_line_html, push_open_link},
};
use crate::util::sanitize::escape_html;

/// Where [`markdown_to_html`] stands in one document: the output so far plus
/// the context an event's rendering depends on.
#[derive(Debug, Default)]
pub(super) struct MarkdownRenderer {
    /// The HTML produced so far.
    pub(super) out: String,
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
    pub(super) fn on_event(&mut self, event: Event<'_>) {
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
    pub(super) fn flush_text(&mut self) {
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

#[cfg(test)]
mod tests {
    use super::super::{
        render_markdown,
        test_support::{REL, anchor, para_with_link},
        text_to_html,
    };

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
}
