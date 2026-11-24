//! HTML sanitizing for user-supplied content, following Pleroma's markup
//! policy (`lib/pleroma/html.ex`, `priv/scrubbers/default.ex`).
//!
//! [`sanitize_html`] runs input through an `ammonia` allowlist built from
//! the ticket policy: a fixed tag set, per-tag attribute sets, value-
//! filtered `class` attributes (on `a`, `span`, `img` and `p`; links keep only
//! `hashtag`, `u-url` and `mention`, `p` only `quote-inline`; an element with
//! no allowed class token left loses the attribute), a fixed URL scheme set and a forced `rel`
//! on every link. Everything else — comments, `style` and `script`
//! including their contents, `iframe` and `form` tags, all event handler
//! attributes — is removed; text of stripped elements survives, "Links in
//! the text keep their text".
//!
//! The plain-text helpers build on the same clean pass: [`strip_html`]
//! walks sanitizer output, which is well-formed, lower-cased markup whose
//! text and attribute values have their `<`, `>`, `"` and `&` characters
//! re-escaped by the serializer (so the only character references that can
//! appear are `&amp;`, `&lt;`, `&gt;` and `&nbsp;`), turns `br` elements
//! and `p` element ends into newlines and drops the remaining tags.
//! [`escape_html`] is the reference escaper for text that has no markup at
//! all, and [`sanitize_display_name`] reduces arbitrary content to the
//! single line of plain text a name may be.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use ammonia::Builder;

/// The tags [`sanitize_html`] keeps; every other element is replaced by its
/// children.
const ALLOWED_TAGS: [&str; 23] = [
    "p",
    "br",
    "a",
    "span",
    "strong",
    "b",
    "em",
    "i",
    "u",
    "s",
    "del",
    "code",
    "pre",
    "blockquote",
    "ul",
    "ol",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "img",
];

/// Attributes kept on `a`. `rel` is deliberately absent: it is not user-set,
/// it is forced to [`FORCED_LINK_REL`] by the builder. `class` is also
/// absent: it is added through [`LINK_CLASSES`], which filters its values.
const LINK_ATTRIBUTES: [&str; 2] = ["href", "title"];

/// The only `class` values `a` may keep, as in Pleroma's scrubber
/// (`hashtag`, `u-url`, `mention` and their combinations); ammonia filters
/// per token, so `mention u-url` survives.
const LINK_CLASSES: [&str; 3] = ["hashtag", "u-url", "mention"];

/// Attributes kept on `img`, besides the value-filtered `class` from
/// [`IMAGE_CLASSES`]. `img` exists for custom emoji only, hence the tight
/// set.
const IMAGE_ATTRIBUTES: [&str; 3] = ["src", "alt", "title"];

/// The only `class` value `img` may keep, as in Pleroma's
/// `allow_tag_with_this_attribute_values(:img, "class", ["emoji"])`.
const IMAGE_CLASSES: [&str; 1] = ["emoji"];

/// The only `class` values `span` may keep; other classes are dropped from
/// the attribute (Pleroma's `h-card`/`invisible`/`ellipsis`/... list).
const SPAN_CLASSES: [&str; 8] = [
    "h-card",
    "mention",
    "hashtag",
    "invisible",
    "ellipsis",
    "emoji",
    "recipients-inline",
    QUOTE_INLINE_CLASS,
];

/// The class Mastodon, Misskey and Pleroma put on the fallback link to a
/// quoted post, so quote-aware clients can hide it.
const QUOTE_INLINE_CLASS: &str = "quote-inline";

/// The only `class` value `p` may keep (Mastodon wraps its `RE:` fallback in
/// `<p class="quote-inline">`).
const PARAGRAPH_CLASSES: [&str; 1] = [QUOTE_INLINE_CLASS];

/// The `class` values `tag` may keep; empty for tags without a class list.
fn allowed_classes_for(tag: &str) -> &'static [&'static str] {
    match tag {
        "a" => &LINK_CLASSES,
        "img" => &IMAGE_CLASSES,
        "span" => &SPAN_CLASSES,
        "p" => &PARAGRAPH_CLASSES,
        _ => &[],
    }
}

/// Drops a `class` attribute none of whose tokens is allowed on its tag.
///
/// Runs before ammonia's own per-token class filter, so an element left with
/// no allowed class serializes without the attribute instead of `class=""`.
fn drop_empty_class<'v>(tag: &str, attribute: &str, value: &'v str) -> Option<Cow<'v, str>> {
    if attribute == "class" {
        let allowed = allowed_classes_for(tag);
        if !value
            .split_ascii_whitespace()
            .any(|class| allowed.contains(&class))
        {
            return None;
        }
    }
    Some(Cow::Borrowed(value))
}

/// URL schemes links and images may use; anything else (`javascript:`,
/// `data:`, ...) loses its `href`/`src`.
const URL_SCHEMES: [&str; 5] = ["http", "https", "mailto", "xmpp", "magnet"];

/// The value `rel` gets forced to on every link; a user-supplied `rel` is
/// dropped first because `rel` is not on the attribute allowlist.
const FORCED_LINK_REL: &str = "ugc nofollow noopener noreferrer";

/// The ammonia builder implementing the policy every function here shares:
/// fresh per call, like [`sanitize_html`]'s parse.
fn builder() -> Builder<'static> {
    let mut builder = Builder::default();
    // The chain evaluates to `&mut Builder`; the binding consumes that
    // result (the lint gate denies dropping one), the builder itself is
    // what gets returned.
    let _configured = builder
        .tags(ALLOWED_TAGS.into_iter().collect())
        .generic_attributes(HashSet::new())
        .tag_attributes(HashMap::from([
            ("a", LINK_ATTRIBUTES.into_iter().collect()),
            ("img", IMAGE_ATTRIBUTES.into_iter().collect()),
        ]))
        .url_schemes(URL_SCHEMES.into_iter().collect())
        .link_rel(Some(FORCED_LINK_REL))
        .add_allowed_classes("a", LINK_CLASSES)
        .add_allowed_classes("img", IMAGE_CLASSES)
        .add_allowed_classes("span", SPAN_CLASSES)
        .add_allowed_classes("p", PARAGRAPH_CLASSES)
        .attribute_filter(drop_empty_class);
    builder
}

/// Sanitizes a fragment of user HTML against the Pleroma-style allowlist.
///
/// Kept verbatim: the `ALLOWED_TAGS` elements, `href`/`title` and the
/// `LINK_CLASSES` classes (`hashtag`, `u-url`, `mention`) on links,
/// `src`/`alt`/`title` and the `emoji` class on images (images exist for
/// custom emoji only), the `SPAN_CLASSES` classes on spans and the
/// `quote-inline` class on paragraphs. Other class tokens are dropped from
/// the attribute, and the attribute itself when none is left.
/// `rel` on every link is forced to `FORCED_LINK_REL`; no `target` is
/// added. Everything else goes: comments, event handler attributes,
/// `style`, and `script`, `iframe` and `form` tags (text of stripped
/// elements is kept, contents of `script` and `style` are not), as are
/// links and images whose URL scheme is not in `URL_SCHEMES`. Relative
/// URLs pass.
#[must_use]
pub fn sanitize_html(input: &str) -> String {
    builder().clean(input).to_string()
}

/// Reduces user HTML to plain text.
///
/// All tags are removed (first [`sanitize_html`], so `script`/`style`
/// contents and comments go too), character references are decoded, and
/// every `br` element and `p` element end becomes a newline:
/// `"<p>a</p><p>b</p>"` strips to `"a\nb\n"`.
#[must_use]
pub fn strip_html(input: &str) -> String {
    to_text(&sanitize_html(input))
}

/// Escapes the five characters that are unsafe in HTML text and quoted
/// attribute values: `&`, `<`, `>`, `"` and `'`.
///
/// The result is inert markup: nothing in `s` can open a tag or a
/// reference. The apostrophe becomes `&#39;`, a numeric character
/// reference that is equally valid in XML, so the XRD documents of
/// [`crate::api::well_known`] use this escaper too, in element text and
/// quoted attribute values alike.
#[must_use]
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Appends `text` to `out` with the unsafe HTML characters escaped, as
/// [`escape_html`] would.
pub fn push_escaped(out: &mut String, text: &str) {
    out.push_str(&escape_html(text));
}

/// Reduces arbitrary user-supplied display-name markup to one line of
/// plain text.
///
/// First [`strip_html`], then every run of control characters (including
/// the newlines a `br`/`/p` produced) collapses to a single space, then
/// the ends are trimmed.
#[must_use]
pub fn sanitize_display_name(s: &str) -> String {
    collapse_control_chars(&strip_html(s)).trim().to_owned()
}

/// Turns serialized HTML (the shape [`sanitize_html`] outputs, see the
/// module docs) into text, with `\n` for `br` elements and `p` element
/// ends; any other tag is dropped and text has its character references
/// decoded.
fn to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some((text, tail)) = rest.split_once('<') {
        decode_text(&mut out, text);
        if let Some((tag, after)) = tail.split_once('>') {
            if tag == "br" || tag == "/p" {
                out.push('\n');
            }
            rest = after;
        } else {
            // Serialized output never has a `<` without its `>`; if one
            // shows up anyway, treat the remainder as plain text.
            out.push('<');
            decode_text(&mut out, tail);
            return out;
        }
    }
    decode_text(&mut out, rest);
    out
}

/// Appends `text` with the character references a serializer emits
/// decoded: `&amp;`, `&lt;`, `&gt;` and `&nbsp;`. A `&` that starts none
/// of them (possible when input held a bare ampersand, e.g. `a&c;b`) stays
/// a literal ampersand.
fn decode_text(out: &mut String, text: &str) {
    let mut rest = text;
    while let Some((before, tail)) = rest.split_once('&') {
        out.push_str(before);
        if let Some(after) = tail.strip_prefix("amp;") {
            out.push('&');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("lt;") {
            out.push('<');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("gt;") {
            out.push('>');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("nbsp;") {
            out.push('\u{a0}');
            rest = after;
        } else {
            out.push('&');
            rest = tail;
        }
    }
    out.push_str(rest);
}

/// Replaces every maximal run of [`char::is_control`] characters with a
/// single space.
fn collapse_control_chars(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for ch in text.chars() {
        if ch.is_control() {
            in_run = true;
        } else {
            if in_run {
                out.push(' ');
                in_run = false;
            }
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::entities::{hashtag_html, mention_html};

    /// What every link's forced `rel` attribute looks like in output.
    const REL: &str = r#"rel="ugc nofollow noopener noreferrer""#;

    #[test]
    fn removes_script_including_its_content() {
        assert_eq!(
            sanitize_html("<script>alert('xss')</script>"),
            "",
            "the script body goes too"
        );
        assert_eq!(
            sanitize_html("<p>hi</p><SCRIPT>alert(1)</SCRIPT>"),
            "<p>hi</p>",
            "uppercase script too, sibling text stays"
        );
    }

    #[test]
    fn removes_style_including_its_content() {
        assert_eq!(
            sanitize_html("<style>@import url(evil.css);</style>hello"),
            "hello",
            "the stylesheet is not text"
        );
        assert_eq!(
            sanitize_html(r#"<p style="background:url(javascript:alert(1))">x</p>"#),
            "<p>x</p>",
            "style attributes go as well"
        );
    }

    #[test]
    fn drops_event_handler_attributes() {
        assert_eq!(
            sanitize_html(r#"<img src="https://x.example/e.png" onerror="alert(1)">"#),
            r#"<img src="https://x.example/e.png">"#,
            "onerror removed, img kept"
        );
        assert_eq!(
            sanitize_html(r#"<p onclick="steal()" class="evil">text</p>"#),
            "<p>text</p>",
            "event attributes and a class on p go (p has no attributes)"
        );
    }

    #[test]
    fn drops_javascript_and_data_urls_keeps_link_text() {
        assert_eq!(
            sanitize_html(r#"<a href="javascript:alert(1)">click</a>"#),
            format!("<a {REL}>click</a>"),
            "the link survives, its javascript: target does not"
        );
        assert_eq!(
            sanitize_html(r#"<a href="data:text/html;base64,PHNjcmlwdD4=">bad</a>"#),
            format!("<a {REL}>bad</a>"),
            "data: is not an allowed scheme"
        );
    }

    #[test]
    fn allows_the_listed_schemes() {
        for href in [
            "https://example.org/",
            "http://example.org/",
            "mailto:me@example.org",
            "xmpp:me@example.org",
            "magnet:?xt=urn:btih:0123456789",
        ] {
            assert_eq!(
                sanitize_html(&format!(r#"<a href="{href}">t</a>"#)),
                format!(r#"<a href="{href}" {REL}>t</a>"#),
                "{href} is allowed"
            );
        }
        assert_eq!(
            sanitize_html(r#"<a href="/objects/123">t</a>"#),
            r#"<a href="/objects/123" rel="ugc nofollow noopener noreferrer">t</a>"#,
            "relative urls pass through"
        );
    }

    #[test]
    fn strips_iframe_and_form_tags_and_keeps_text() {
        assert_eq!(
            sanitize_html(r#"<iframe src="https://evil.example"></iframe>fallback"#),
            "fallback",
            "iframe tag gone, its text kept"
        );
        assert_eq!(
            sanitize_html(r#"<form action="/x"><input name="a"><p>body</p></form>"#),
            "<p>body</p>",
            "form and input gone, inner allowed markup kept"
        );
    }

    #[test]
    fn preserves_allowed_inline_markup() {
        let input = r#"<p>Hi<br>there <a href="https://example.org">link</a> <span class="mention">@m</span> <strong>s</strong> <b>b</b> <em>e</em> <i>i</i> <u>u</u> <s>st</s> <del>dl</del> <code>co</code> <img src="https://x.example/e.png" class="emoji" alt=":e:" title=":e:"></p>"#;
        let expected = format!(
            r#"<p>Hi<br>there <a href="https://example.org" {REL}>link</a> <span class="mention">@m</span> <strong>s</strong> <b>b</b> <em>e</em> <i>i</i> <u>u</u> <s>st</s> <del>dl</del> <code>co</code> <img src="https://x.example/e.png" class="emoji" alt=":e:" title=":e:"></p>"#
        );
        assert_eq!(
            sanitize_html(input),
            expected,
            "the inline allowlist round-trips"
        );
    }

    #[test]
    fn preserves_allowed_block_markup() {
        let input = "<pre>a &lt; b</pre><blockquote>q</blockquote><ul><li>1</li></ul><ol><li>2</li></ol><h1>1</h1><h2>2</h2><h3>3</h3><h4>4</h4><h5>5</h5>";
        assert_eq!(
            sanitize_html(input),
            input,
            "the block allowlist round-trips"
        );
        assert_eq!(sanitize_html("<h6>x</h6>"), "x", "h6 is not on the list");
    }

    #[test]
    fn filters_class_values_to_the_allowlists() {
        assert_eq!(
            sanitize_html(r#"<span class="mention evil">x</span>"#),
            r#"<span class="mention">x</span>"#,
            "the evil class is dropped, the good one kept"
        );
        assert_eq!(
            sanitize_html(r#"<span class="evil">x</span>"#),
            "<span>x</span>",
            "an all-evil class list removes the attribute instead of leaving class=\"\""
        );
        assert_eq!(
            sanitize_html(
                r#"<span class="h-card invisible ellipsis emoji recipients-inline hashtag mention">x</span>"#
            ),
            r#"<span class="h-card invisible ellipsis emoji recipients-inline hashtag mention">x</span>"#,
            "every listed span class survives"
        );
        assert_eq!(
            sanitize_html(r#"<span class="p-name">Bob</span>"#),
            "<span>Bob</span>",
            "microformat classes on span go (they are not on this list)"
        );
        assert_eq!(
            sanitize_html(r#"<img src="https://x.example/i.png" class="emoji">"#),
            r#"<img src="https://x.example/i.png" class="emoji">"#,
            "the img emoji class is kept"
        );
        assert_eq!(
            sanitize_html(r#"<img src="https://x.example/i.png" class="avatar">"#),
            r#"<img src="https://x.example/i.png">"#,
            "img is for emoji only"
        );
        assert_eq!(
            sanitize_html(r#"<p class="evil">x</p>"#),
            "<p>x</p>",
            "p keeps only quote-inline"
        );
        assert_eq!(
            sanitize_html(r#"<p class="evil quote-inline">x</p>"#),
            r#"<p class="quote-inline">x</p>"#,
            "quote-inline survives next to a dropped class"
        );
    }

    #[test]
    fn keeps_the_quote_inline_class_of_remote_quote_fallbacks() {
        let mastodon = r#"<p class="quote-inline">RE: <a href="https://x.example/p/1">x.example/p/1</a></p><p>text</p>"#;
        assert_eq!(
            sanitize_html(mastodon),
            format!(
                r#"<p class="quote-inline">RE: <a href="https://x.example/p/1" {REL}>x.example/p/1</a></p><p>text</p>"#
            ),
            "Mastodon's paragraph fallback keeps its class"
        );
        let misskey = r#"text<br><br><span class="quote-inline">RE: <a href="https://x.example/p/1">x</a></span>"#;
        assert_eq!(
            sanitize_html(misskey),
            format!(
                r#"text<br><br><span class="quote-inline">RE: <a href="https://x.example/p/1" {REL}>x</a></span>"#
            ),
            "Misskey's span fallback keeps its class"
        );
        let pleroma = r#"<span class="quote-inline"><br/><br/><bdi>RT:</bdi> <a href="https://x.example/p/1">x</a></span>"#;
        assert!(
            sanitize_html(pleroma).starts_with(r#"<span class="quote-inline"><br><br>"#),
            "Pleroma's span fallback keeps its class"
        );
    }

    #[test]
    fn link_classes_are_value_filtered() {
        assert_eq!(
            sanitize_html(r#"<a href="https://example.org" class="mention u-url">@b</a>"#),
            format!(r#"<a href="https://example.org" class="mention u-url" {REL}>@b</a>"#),
            "listed link classes are kept"
        );
        assert_eq!(
            sanitize_html(r#"<a href="https://example.org" class="anything">x</a>"#),
            format!(r#"<a href="https://example.org" {REL}>x</a>"#),
            "an unlisted link class is dropped with its attribute"
        );
        assert_eq!(
            sanitize_html(r#"<a href="https://example.org" class="mention evil">x</a>"#),
            format!(r#"<a href="https://example.org" class="mention" {REL}>x</a>"#),
            "only the unlisted token goes"
        );
    }

    #[test]
    fn mention_and_hashtag_html_keep_their_classes() {
        let mention = sanitize_html(&mention_html("https://x.example/u", "bob"));
        assert!(
            mention.contains(r#"class="u-url mention""#),
            "mention classes survive byte for byte: {mention}"
        );
        let hashtag = sanitize_html(&hashtag_html("https://x.example", "tag"));
        assert!(
            hashtag.contains(r#"class="mention hashtag""#),
            "hashtag classes survive byte for byte: {hashtag}"
        );
    }

    #[test]
    fn forces_rel_on_links_and_adds_no_target() {
        assert_eq!(
            sanitize_html(r#"<a href="https://example.org" rel="me" target="_blank">x</a>"#),
            format!(r#"<a href="https://example.org" {REL}>x</a>"#),
            "user rel is replaced, target is dropped and never added"
        );
        assert_eq!(
            sanitize_html("<a>x</a>"),
            format!("<a {REL}>x</a>"),
            "even a link with no href gets the rel"
        );
    }

    #[test]
    fn strips_tags_to_text_with_newlines() {
        assert_eq!(
            strip_html("<p>a</p><p>b</p>"),
            "a\nb\n",
            "every </p> becomes a newline"
        );
        assert_eq!(
            strip_html("line1<br>line2"),
            "line1\nline2",
            "<br> becomes a newline"
        );
        assert_eq!(
            strip_html("x<BR/>y"),
            "x\ny",
            "br spellings normalize through the parser"
        );
        assert_eq!(
            strip_html(r#"<span class="mention"><span class="invisible">@</span>user</span>"#),
            "@user",
            "nested spans collapse to their text"
        );
        assert_eq!(
            strip_html("<script>gone()</script><p>keep <b>this</b></p>"),
            "keep this\n",
            "script content is not text"
        );
    }

    #[test]
    fn strips_html_decodes_entities() {
        assert_eq!(
            strip_html("a &amp; b &lt; c &gt; d"),
            "a & b < c > d",
            "the named ones"
        );
        assert_eq!(
            strip_html("&#39;q&#39;"),
            "'q'",
            "numeric references decode at parse time"
        );
        assert_eq!(
            strip_html("a&nbsp;b"),
            "a\u{a0}b",
            "a kept nbsp stays a nbsp"
        );
        assert_eq!(
            strip_html("&amp;lt;"),
            "&lt;",
            "double-escaped text decodes exactly once"
        );
    }

    #[test]
    fn escapes_the_five_unsafe_characters() {
        assert_eq!(
            escape_html(r#"<a href="x?a=1&b=2">'quote'</a>"#),
            "&lt;a href=&quot;x?a=1&amp;b=2&quot;&gt;&#39;quote&#39;&lt;/a&gt;",
            "every unsafe character becomes a reference"
        );
        assert_eq!(
            escape_html("plain text 100% ✓"),
            "plain text 100% ✓",
            "safe text is untouched"
        );
        assert_eq!(escape_html(""), "", "empty stays empty");
    }

    #[test]
    fn plain_text_and_unicode_pass_through_unescaped() {
        for value in [
            "",
            "alice",
            "acct:alice@test.example",
            "https://test.example/users/alice",
            "héllo wörld — ✓",
        ] {
            assert_eq!(escape_html(value), value, "{value:?} needs no escaping");
        }
    }

    #[test]
    fn every_special_character_becomes_its_reference() {
        assert_eq!(escape_html("&"), "&amp;", "ampersand");
        assert_eq!(escape_html("<"), "&lt;", "less-than");
        assert_eq!(escape_html(">"), "&gt;", "greater-than");
        assert_eq!(escape_html("\""), "&quot;", "double quote");
        assert_eq!(escape_html("'"), "&#39;", "single quote");
    }

    #[test]
    fn text_that_looks_like_references_is_escaped_once() {
        assert_eq!(
            escape_html("a<b>&c\"d'e"),
            "a&lt;b&gt;&amp;c&quot;d&#39;e",
            "each character is replaced once"
        );
        assert_eq!(
            escape_html("<a href=\"x\">&amp;</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;amp;&lt;/a&gt;",
            "text that already looks like entities is escaped as literal text"
        );
    }

    #[test]
    fn push_escaped_appends_the_escaped_text() {
        let mut out = String::from("<p>");
        push_escaped(&mut out, "a<b & 'c'");
        push_escaped(&mut out, "");
        assert_eq!(out, "<p>a&lt;b &amp; &#39;c&#39;", "appended, not replaced");
    }

    #[test]
    fn display_names_are_single_line_plain_text() {
        assert_eq!(
            sanitize_display_name("  <script>alert(1)</script>Bob <b>the</b> Builder\u{0} "),
            "Bob the Builder",
            "markup, padding and control characters all go"
        );
        assert_eq!(
            sanitize_display_name("John<br>Doe"),
            "John Doe",
            "a br becomes a space"
        );
        assert_eq!(
            sanitize_display_name("A\t\u{1}\u{1}B\r\nC"),
            "A B C",
            "each control run collapses to one space"
        );
        assert_eq!(
            sanitize_display_name("Bob\u{7f}\u{9c}"),
            "Bob",
            "trailing controls go"
        );
        assert_eq!(
            sanitize_display_name("<p></p>"),
            "",
            "markup-only names end up empty"
        );
    }

    #[test]
    fn sanitizing_is_idempotent() {
        let input = r#"<p onclick="x()">Hi <a href="javascript:1" class="mention evil">y</a><script>z</script><span class="h-card"><3</span></p>"#;
        let once = sanitize_html(input);
        assert_eq!(sanitize_html(&once), once, "a second pass changes nothing");
    }

    #[test]
    fn to_text_handles_a_broken_tag_as_text() {
        assert_eq!(to_text("a<b"), "a<b", "an unclosed tag stays plain text");
        assert_eq!(
            to_text("<b &amp;c>x"),
            "x",
            "a tag is dropped, its text kept"
        );
    }

    #[test]
    fn decode_text_passes_unknown_references_through() {
        let mut out = String::new();
        decode_text(&mut out, "x &amp; y &larr; z");
        assert_eq!(
            out, "x & y &larr; z",
            "an unknown reference is not a serialized entity"
        );
    }

    #[test]
    fn collapse_control_chars_merges_runs() {
        assert_eq!(
            collapse_control_chars("a\u{0}\u{1}b"),
            "a b",
            "a run becomes one space"
        );
        assert_eq!(
            collapse_control_chars("\u{7f}a\u{9f}"),
            " a",
            "runs still join at the edges"
        );
        assert_eq!(
            collapse_control_chars("keep\nthis"),
            "keep this",
            "newlines count too"
        );
    }
}
