//! Atom and RSS feed document builders (`docs/ARCHITECTURE.md` decision 3).
//!
//! The routes of [`crate::api::feeds`] fetch a feed's content; this module
//! owns the XML: one [`Feed`] description (metadata plus newest-first
//! [`FeedEntry`] rows) renders as either an Atom document
//! ([`atom_document`]) or an RSS 2.0 document ([`rss_document`]) through
//! `quick-xml`'s writer. Every database-sourced string goes out as a
//! [`quick_xml::events::BytesText`] or an attribute value, so the writer
//! escapes `<`, `>`, `&`, `"` and `'` — a post containing markup can
//! never break out of its element, and an entity in the text survives as
//! literal text.
//!
//! The builders are pure: no database, no request, no configuration.
//! Timestamps arrive as [`DateTime<Utc>`] and are formatted per syndication
//! ([`atom_time`] is `Atom`'s RFC 3339, [`rss_time`] is `RSS`'s RFC 2822);
//! [`clip_title`] cuts an entry title at a character count, never mid-`char`.

use chrono::{DateTime, SecondsFormat, Utc};
use quick_xml::{
    Writer,
    events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event},
};

use crate::error::{AppError, AppResult};

/// `chrono` format for [`rss_time`]: the RFC 2822 date with a fixed UTC
/// offset spelled out (`+0000`), which is what `pubDate` demands.
const RSS_TIME_FORMAT: &str = "%a, %d %b %Y %H:%M:%S +0000";

/// The `Atom` namespace, declared on the feed root as its default.
const ATOM_NAMESPACE: &str = "http://www.w3.org/2005/Atom";
/// The qualified name of the standards-track self link the `RSS` channel
/// borrows from `Atom` (the root declares the namespace under this
/// prefix).
const ATOM_LINK_NAME: &str = "atom:link";
/// The `rel` value of the link to this feed at this exact URL.
const REL_SELF: &str = "self";
/// The `rel` value of the link to the human-facing page.
const REL_ALTERNATE: &str = "alternate";
/// The `rel` value of the link to an attached file.
const REL_ENCLOSURE: &str = "enclosure";
/// The media type of this `Atom` feed, as its self link advertises it.
const ATOM_MEDIA_TYPE: &str = "application/atom+xml";
/// The media type of this `RSS` feed, as its self link advertises it.
const RSS_MEDIA_TYPE: &str = "application/rss+xml";
/// The media type of a human-facing page, as the alternate links say.
const HTML_MEDIA_TYPE: &str = "text/html";

/// `Content-Type` of an Atom feed document: `Atom` is XML, and the charset
/// is spelled out because the bodies carry user text.
pub const ATOM_CONTENT_TYPE: &str = "application/atom+xml; charset=utf-8";
/// `Content-Type` of an RSS feed document, charset included.
pub const RSS_CONTENT_TYPE: &str = "application/rss+xml; charset=utf-8";

/// One attached file of a status, rendered as the entry's enclosure.
#[derive(Clone, Debug)]
pub struct Enclosure {
    /// Public URL of the media file.
    pub url: String,
    /// `MIME` type of the media, e.g. `image/png`.
    pub mime_type: String,
    /// File size in bytes; `None` (or a non-positive size) renders no
    /// `length` on the Atom link, and RSS (which requires the attribute)
    /// renders zero.
    pub size: Option<i64>,
}

/// One status rendered as one feed entry.
#[derive(Clone, Debug)]
pub struct FeedEntry {
    /// Stable identity of the post: its `ActivityPub` uri, unique forever.
    pub id: String,
    /// Headline: the first characters of the status text
    /// (see [`clip_title`]).
    pub title: String,
    /// The post body as sanitized HTML; escaped into the document, never
    /// interpolated raw.
    pub content_html: String,
    /// Human-facing URL of the post; `None` renders no alternate link.
    pub url: Option<String>,
    /// When the post was published.
    pub published: DateTime<Utc>,
    /// When the post last changed.
    pub updated: DateTime<Utc>,
    /// The attached media, in position order.
    pub enclosures: Vec<Enclosure>,
}

/// A whole feed: the channel metadata and its entries, newest first.
#[derive(Clone, Debug)]
pub struct Feed {
    /// The URL this feed is served at, in the format being rendered.
    pub self_url: String,
    /// The human-facing page this feed describes (the profile, or the tag).
    pub alternate_url: String,
    /// The feed's title.
    pub title: String,
    /// The feed's description: the RSS channel's `description` element
    /// (`Atom` renders no subtitle).
    pub description: String,
    /// The feed author's name for an account feed; `None` (a tag feed)
    /// renders no `author` element.
    pub author_name: Option<String>,
    /// The feed-level updated timestamp: the newest entry's, or the
    /// underlying account's (or now, for a tag feed) when it has none.
    pub updated: DateTime<Utc>,
    /// The entries, newest first.
    pub entries: Vec<FeedEntry>,
}

/// `dt` as an `Atom` timestamp: RFC 3339, UTC, millisecond precision.
#[must_use]
pub fn atom_time(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `dt` as an `RSS` timestamp: the RFC 2822 date format `pubDate` and
/// `lastBuildDate` demand.
#[must_use]
pub fn rss_time(dt: &DateTime<Utc>) -> String {
    dt.format(RSS_TIME_FORMAT).to_string()
}

/// The first `max_chars` characters of `text`, for entry titles: the cut is
/// on `char` boundaries, so a multibyte title is never sliced mid-character,
/// and a shorter text comes back whole.
#[must_use]
pub fn clip_title(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Renders `feed` as an `Atom` document: `<feed xmlns="…Atom">` with its
/// `id`, `title`, `updated`, optional `author`, the `self` and `alternate`
/// links, and one `<entry>` per [`FeedEntry`].
///
/// # Errors
///
/// [`AppError::Internal`] if the document could not be written (the sink is
/// a `Vec`, so it cannot; the 500 stands between a caller and a `panic`).
pub fn atom_document(feed: &Feed) -> AppResult<String> {
    let mut writer = Writer::new(Vec::new());
    write_atom(&mut writer, feed).map_err(|err| xml_failure(&err))?;
    render_string(writer)
}

/// Renders `feed` as an `RSS` 2.0 document: `<rss version="2.0">` with one
/// `<channel>` (title, link, description, `lastBuildDate`, the self link)
/// and one `<item>` per [`FeedEntry`].
///
/// # Errors
///
/// [`AppError::Internal`] as in [`atom_document`].
pub fn rss_document(feed: &Feed) -> AppResult<String> {
    let mut writer = Writer::new(Vec::new());
    write_rss(&mut writer, feed).map_err(|err| xml_failure(&err))?;
    render_string(writer)
}

/// The XML writer's failure as the client-facing 500 (the detail is the
/// [`std::io::Error`]'s message; it never reaches the body, see
/// [`AppError::Internal`]).
fn xml_failure(err: &std::io::Error) -> AppError {
    AppError::Internal(format!("could not write the feed document: {err}"))
}

/// The bytes the writer produced as text: it can only be UTF-8 (every
/// string fed it was `str`), but a failed re-decode is the same 500.
fn render_string(writer: Writer<Vec<u8>>) -> AppResult<String> {
    String::from_utf8(writer.into_inner())
        .map_err(|err| AppError::Internal(format!("feed document is not UTF-8: {err}")))
}

/// Writes the XML declaration both formats start with.
fn write_decl(writer: &mut Writer<Vec<u8>>) -> std::io::Result<()> {
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
}

/// Writes one element with `name` holding `text` as its escaped content.
fn write_text_element(writer: &mut Writer<Vec<u8>>, name: &str, text: &str) -> std::io::Result<()> {
    writer.write_event(Event::Start(BytesStart::new(name)))?;
    writer.write_event(Event::Text(BytesText::new(text)))?;
    writer.write_event(Event::End(BytesEnd::new(name)))
}

/// Writes an empty `<link>` element with `rel` and `href`, plus the
/// optional `type` and `length` attributes.
fn write_link(
    writer: &mut Writer<Vec<u8>>,
    rel: &str,
    href: &str,
    mime_type: Option<&str>,
    length: Option<i64>,
) -> std::io::Result<()> {
    let mut link = BytesStart::new("link");
    link.push_attribute(("rel", rel));
    link.push_attribute(("href", href));
    if let Some(mime_type) = mime_type {
        link.push_attribute(("type", mime_type));
    }
    if let Some(length) = length {
        let length_text = length.to_string();
        link.push_attribute(("length", length_text.as_str()));
    }
    writer.write_event(Event::Empty(link))
}

/// Writes `feed` in full as `Atom`.
fn write_atom(writer: &mut Writer<Vec<u8>>, feed: &Feed) -> std::io::Result<()> {
    write_decl(writer)?;
    let mut root = BytesStart::new("feed");
    root.push_attribute(("xmlns", ATOM_NAMESPACE));
    writer.write_event(Event::Start(root))?;
    write_text_element(writer, "id", &feed.self_url)?;
    write_text_element(writer, "title", &feed.title)?;
    write_text_element(writer, "updated", &atom_time(&feed.updated))?;
    write_atom_author(writer, feed.author_name.as_deref())?;
    write_link(
        writer,
        REL_SELF,
        &feed.self_url,
        Some(ATOM_MEDIA_TYPE),
        None,
    )?;
    write_link(
        writer,
        REL_ALTERNATE,
        &feed.alternate_url,
        Some(HTML_MEDIA_TYPE),
        None,
    )?;
    for entry in &feed.entries {
        write_atom_entry(writer, entry)?;
    }
    writer.write_event(Event::End(BytesEnd::new("feed")))
}

/// Writes the optional feed-level `<author><name>…</name></author>`.
fn write_atom_author(writer: &mut Writer<Vec<u8>>, name: Option<&str>) -> std::io::Result<()> {
    let Some(name) = name else {
        return Ok(());
    };
    writer.write_event(Event::Start(BytesStart::new("author")))?;
    write_text_element(writer, "name", name)?;
    writer.write_event(Event::End(BytesEnd::new("author")))
}

/// Writes one `<entry>`: the ticket's element order — `id`, `title`,
/// escaped `content type="html"`, `published`, `updated`, the `alternate`
/// link, then one `enclosure` link per attachment.
fn write_atom_entry(writer: &mut Writer<Vec<u8>>, entry: &FeedEntry) -> std::io::Result<()> {
    writer.write_event(Event::Start(BytesStart::new("entry")))?;
    write_text_element(writer, "id", &entry.id)?;
    write_text_element(writer, "title", &entry.title)?;
    let mut content = BytesStart::new("content");
    content.push_attribute(("type", "html"));
    writer.write_event(Event::Start(content))?;
    writer.write_event(Event::Text(BytesText::new(&entry.content_html)))?;
    writer.write_event(Event::End(BytesEnd::new("content")))?;
    write_text_element(writer, "published", &atom_time(&entry.published))?;
    write_text_element(writer, "updated", &atom_time(&entry.updated))?;
    if let Some(url) = &entry.url {
        write_link(writer, REL_ALTERNATE, url, Some(HTML_MEDIA_TYPE), None)?;
    }
    for enclosure in &entry.enclosures {
        write_link(
            writer,
            REL_ENCLOSURE,
            &enclosure.url,
            Some(&enclosure.mime_type),
            enclosure.size.filter(|size| *size > 0),
        )?;
    }
    writer.write_event(Event::End(BytesEnd::new("entry")))
}

/// Writes `feed` in full as `RSS` 2.0.
fn write_rss(writer: &mut Writer<Vec<u8>>, feed: &Feed) -> std::io::Result<()> {
    write_decl(writer)?;
    let mut root = BytesStart::new("rss");
    root.push_attribute(("version", "2.0"));
    root.push_attribute(("xmlns:atom", ATOM_NAMESPACE));
    writer.write_event(Event::Start(root))?;
    writer.write_event(Event::Start(BytesStart::new("channel")))?;
    write_text_element(writer, "title", &feed.title)?;
    write_text_element(writer, "link", &feed.alternate_url)?;
    write_text_element(writer, "description", &feed.description)?;
    write_text_element(writer, "lastBuildDate", &rss_time(&feed.updated))?;
    let mut self_link = BytesStart::new(ATOM_LINK_NAME);
    self_link.push_attribute(("href", feed.self_url.as_str()));
    self_link.push_attribute(("rel", REL_SELF));
    self_link.push_attribute(("type", RSS_MEDIA_TYPE));
    writer.write_event(Event::Empty(self_link))?;
    for entry in &feed.entries {
        write_rss_item(writer, entry)?;
    }
    writer.write_event(Event::End(BytesEnd::new("channel")))?;
    writer.write_event(Event::End(BytesEnd::new("rss")))
}

/// Writes one `<item>`: `guid` (the status uri, not a permalink), `title`,
/// escaped `description`, the post `link`, `pubDate`, then one
/// `<enclosure>` per attachment (`length` is required by `RSS`, so an
/// unknown size renders zero).
fn write_rss_item(writer: &mut Writer<Vec<u8>>, entry: &FeedEntry) -> std::io::Result<()> {
    writer.write_event(Event::Start(BytesStart::new("item")))?;
    let mut guid = BytesStart::new("guid");
    guid.push_attribute(("isPermaLink", "false"));
    writer.write_event(Event::Start(guid))?;
    writer.write_event(Event::Text(BytesText::new(&entry.id)))?;
    writer.write_event(Event::End(BytesEnd::new("guid")))?;
    write_text_element(writer, "title", &entry.title)?;
    write_text_element(writer, "description", &entry.content_html)?;
    if let Some(url) = &entry.url {
        write_text_element(writer, "link", url)?;
    }
    write_text_element(writer, "pubDate", &rss_time(&entry.published))?;
    for enclosure in &entry.enclosures {
        let mut tag = BytesStart::new("enclosure");
        tag.push_attribute(("url", enclosure.url.as_str()));
        tag.push_attribute(("type", enclosure.mime_type.as_str()));
        let length = enclosure.size.filter(|size| *size > 0).unwrap_or(0);
        let length_text = length.to_string();
        tag.push_attribute(("length", length_text.as_str()));
        writer.write_event(Event::Empty(tag))?;
    }
    writer.write_event(Event::End(BytesEnd::new("item")))
}

#[cfg(test)]
mod tests {
    use quick_xml::{Reader, XmlVersion, escape::unescape};

    use super::*;

    /// The document version the reader decodes text against (the writers
    /// all emit `<?xml version="1.0" …?>`).
    const XML_VERSION: XmlVersion = XmlVersion::Explicit1_0;

    /// The timestamp the fixtures are stamped with (a whole millisecond).
    fn fixed_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-15T12:34:56.789Z")
            .expect("the fixture timestamp is RFC 3339")
            .with_timezone(&Utc)
    }

    /// An entry body with the given text as its content and title.
    fn entry(title_text: &str, content_html: &str) -> FeedEntry {
        let now = fixed_time();
        FeedEntry {
            id: format!("https://test.example/objects/{title_text}"),
            title: title_text.to_owned(),
            content_html: content_html.to_owned(),
            url: Some(format!("https://test.example/notice/{title_text}")),
            published: now,
            updated: now,
            enclosures: Vec::new(),
        }
    }

    /// A two-entry feed with an author, and a tag-style feed without one.
    fn sample_feed() -> Feed {
        let now = fixed_time();
        Feed {
            self_url: "https://test.example/users/alice/feed.atom".to_owned(),
            alternate_url: "https://test.example/alice".to_owned(),
            title: "Alice's timeline".to_owned(),
            description: "hello & goodbye".to_owned(),
            author_name: Some("alice".to_owned()),
            updated: now,
            entries: vec![entry("first", "<p>one</p>"), entry("second", "<p>two</p>")],
        }
    }

    /// One XML event of the document stream, flattened for assertions:
    /// start/empty tags as `open:<name>`, end tags as `close:<name>`, text
    /// as `text:<decoded content>` (entities already unescaped by the
    /// reader), and the declaration as `decl`.
    #[derive(Debug, PartialEq, Eq)]
    enum Step {
        /// The XML declaration.
        Decl,
        /// An opened tag (`start` or `empty`), with its attributes.
        Open(String, Vec<(String, String)>),
        /// A closed tag.
        Close(String),
        /// Element text, entities decoded.
        Text(String),
    }

    /// Parses `document` back with `quick-xml`'s reader — which fails on
    /// any ill-formed markup or unmatched end tag — and hands over the
    /// event steps. The reader splits entity references out of the text
    /// (into `GeneralRef` events), so a text run is rebuilt as the
    /// `&name;` spellings it arrived in and then decoded, exactly as the
    /// reader's other consumer in this crate does
    /// ([`crate::federation::webfinger_client`]).
    ///
    /// # Panics
    ///
    /// If the document is not well-formed XML, or carries an undefined
    /// entity.
    fn parse_back(document: &str) -> Vec<Step> {
        let mut reader = Reader::from_str(document);
        let mut steps: Vec<Step> = Vec::new();
        let mut pending = String::new();
        loop {
            match reader.read_event().expect("the document is well-formed") {
                Event::Eof => {
                    flush_text(&mut steps, &mut pending);
                    return steps;
                }
                Event::Decl(_) => {
                    flush_text(&mut steps, &mut pending);
                    steps.push(Step::Decl);
                }
                Event::Start(start) => {
                    flush_text(&mut steps, &mut pending);
                    steps.push(Step::Open(
                        start.local_name().as_ref().to_owned(),
                        attributes_of(&start),
                    ));
                }
                Event::Empty(empty) => {
                    flush_text(&mut steps, &mut pending);
                    steps.push(Step::Open(
                        empty.local_name().as_ref().to_owned(),
                        attributes_of(&empty),
                    ));
                }
                Event::End(end) => {
                    flush_text(&mut steps, &mut pending);
                    steps.push(Step::Close(end.local_name().as_ref().to_owned()));
                }
                Event::Text(text) => pending.push_str(&text.xml10_content()),
                Event::GeneralRef(reference) => {
                    pending.push('&');
                    pending.push_str(reference.into_inner().as_ref());
                    pending.push(';');
                }
                other
                @ (Event::CData(_) | Event::Comment(_) | Event::PI(_) | Event::DocType(_)) => {
                    drop(other);
                }
            }
        }
    }

    /// Ends the current text run: the accumulated raw fragments, with the
    /// entity references resolved, become one [`Step::Text`].
    ///
    /// # Panics
    ///
    /// If the run carries an undefined entity.
    fn flush_text(steps: &mut Vec<Step>, pending: &mut String) {
        if pending.is_empty() {
            return;
        }
        let decoded = unescape(pending).expect("the entities are defined");
        steps.push(Step::Text(decoded.into_owned()));
        pending.clear();
    }

    /// The attribute pairs of a start tag, names and unescaped values.
    ///
    /// # Panics
    ///
    /// If an attribute value holds no decodable text.
    fn attributes_of(start: &BytesStart<'_>) -> Vec<(String, String)> {
        start
            .attributes()
            .map(|attribute| {
                let attribute = attribute.expect("the attribute parses");
                (
                    attribute.key.as_ref().to_owned(),
                    attribute
                        .normalized_value(XML_VERSION)
                        .expect("the value decodes")
                        .into_owned(),
                )
            })
            .collect()
    }

    /// The text of the first element named `name`: the builder never nests
    /// markup inside a text element, so the first text after its opening
    /// tag is its content.
    ///
    /// # Panics
    ///
    /// If no such element carries text.
    fn text_of(steps: &[Step], name: &str) -> String {
        let position = steps
            .iter()
            .position(|step| matches!(step, Step::Open(element, _) if element == name))
            .unwrap_or_else(|| panic!("no element named {name}"));
        steps[position + 1..]
            .iter()
            .find_map(|step| match step {
                Step::Text(text) => Some(text.clone()),
                Step::Decl | Step::Open(..) | Step::Close(_) => None,
            })
            .unwrap_or_else(|| panic!("element {name} holds no text"))
    }

    #[test]
    fn an_atom_feed_parses_back_with_every_ticketed_element() {
        let document = atom_document(&sample_feed()).expect("the feed renders");
        let steps = parse_back(&document);
        let names: Vec<&str> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Open(name, _) => Some(name.as_str()),
                Step::Decl | Step::Text(_) | Step::Close(_) => None,
            })
            .collect();
        assert_eq!(steps.first(), Some(&Step::Decl), "the declaration is first");
        assert_eq!(
            &steps[1..3],
            [
                Step::Open(
                    "feed".to_owned(),
                    vec![("xmlns".to_owned(), ATOM_NAMESPACE.to_owned())]
                ),
                Step::Open("id".to_owned(), Vec::new()),
            ],
            "the root is a namespaced feed opening with its id"
        );
        for expected in [
            "title", "updated", "author", "name", "link", "link", "entry",
        ] {
            assert!(names.contains(&expected), "{expected} is part of the feed");
        }
        assert_eq!(
            names.iter().filter(|name| **name == "entry").count(),
            2,
            "one entry per status"
        );
    }

    #[test]
    fn atom_entries_keep_their_order_and_element_order() {
        let document = atom_document(&sample_feed()).expect("the feed renders");
        let steps = parse_back(&document);
        let texts: Vec<&str> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Text(text) => Some(text.as_str()),
                Step::Decl | Step::Open(..) | Step::Close(_) => None,
            })
            .collect();
        let first_id = "https://test.example/objects/first";
        let second_id = "https://test.example/objects/second";
        let first_at = texts
            .iter()
            .position(|text| *text == first_id)
            .expect("the first entry is present");
        let second_at = texts
            .iter()
            .position(|text| *text == second_id)
            .expect("the second entry is present");
        assert!(
            first_at < second_at,
            "entries keep the given (newest-first) order"
        );
        let entry_start = steps
            .iter()
            .position(|step| step == &Step::Open("entry".to_owned(), Vec::new()))
            .expect("an entry exists");
        let entry_body: Vec<String> = steps[entry_start + 1..entry_start + 14]
            .iter()
            .filter_map(|step| match step {
                Step::Open(name, _) => Some(name.clone()),
                Step::Decl | Step::Text(_) | Step::Close(_) => None,
            })
            .collect();
        assert_eq!(
            entry_body,
            vec!["id", "title", "content", "published", "updated"],
            "the ticket's element order inside an entry"
        );
    }

    #[test]
    fn markup_in_content_is_escaped_and_parses_back_as_text() {
        let mut feed = sample_feed();
        feed.entries = vec![entry("x", r#"<script>alert("a &lt; b & c > d")</script>"#)];
        let document = atom_document(&feed).expect("the feed renders");
        assert!(
            document.contains("&lt;script&gt;"),
            "the markup travels as escaped text: {document}"
        );
        assert!(
            !document.contains("<script>"),
            "no live script element exists in the document"
        );
        let steps = parse_back(&document);
        assert_eq!(
            text_of(&steps, "content"),
            r#"<script>alert("a &lt; b & c > d")</script>"#,
            "the decoded content is exactly what went in"
        );
    }

    #[test]
    fn ampersands_in_titles_and_feed_text_are_escaped_once() {
        let mut feed = sample_feed();
        feed.title = "Alice & Bob's <timeline>".to_owned();
        feed.entries = vec![entry("Tom & Jerry", "text")];
        let document = rss_document(&feed).expect("the feed renders");
        assert!(
            document.contains("Alice &amp; Bob&apos;s &lt;timeline&gt;"),
            "{document}"
        );
        assert!(document.contains("Tom &amp; Jerry"), "{document}");
        let steps = parse_back(&document);
        assert_eq!(text_of(&steps, "title"), "Alice & Bob's <timeline>");
    }

    #[test]
    fn an_rss_feed_parses_back_with_channel_and_items() {
        let document = rss_document(&sample_feed()).expect("the feed renders");
        let steps = parse_back(&document);
        assert_eq!(steps.first(), Some(&Step::Decl), "the declaration is first");
        assert_eq!(
            steps.get(1),
            Some(&Step::Open(
                "rss".to_owned(),
                vec![
                    ("version".to_owned(), "2.0".to_owned()),
                    ("xmlns:atom".to_owned(), ATOM_NAMESPACE.to_owned()),
                ]
            )),
            "the root announces RSS 2.0 and the Atom namespace for its self link"
        );
        let names: Vec<&str> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Open(name, _) => Some(name.as_str()),
                Step::Decl | Step::Text(_) | Step::Close(_) => None,
            })
            .collect();
        for expected in ["channel", "title", "link", "description", "lastBuildDate"] {
            assert!(names.contains(&expected), "{expected}");
        }
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::Open(name, attributes)
                    if name == "link"
                        && attributes.contains(&("rel".to_owned(), REL_SELF.to_owned()))
                        && attributes.contains(&("type".to_owned(), RSS_MEDIA_TYPE.to_owned())))
            }),
            "the channel carries the atom:link self reference"
        );
        assert_eq!(
            names.iter().filter(|name| **name == "item").count(),
            2,
            "one item per entry"
        );
        assert_eq!(
            text_of(&steps, "description"),
            sample_feed().description,
            "the channel description is the feed's, decoded whole"
        );
    }

    #[test]
    fn rss_items_carry_guid_link_pubdate_and_enclosures() {
        let mut feed = sample_feed();
        feed.entries = vec![FeedEntry {
            enclosures: vec![
                Enclosure {
                    url: "https://test.example/media/pic.png".to_owned(),
                    mime_type: "image/png".to_owned(),
                    size: Some(1234),
                },
                Enclosure {
                    url: "https://test.example/media/mystery".to_owned(),
                    mime_type: "application/octet-stream".to_owned(),
                    size: None,
                },
            ],
            ..entry("post", "<p>body</p>")
        }];
        let document = rss_document(&feed).expect("the feed renders");
        let steps = parse_back(&document);
        let guid_attributes = steps
            .iter()
            .find_map(|step| match step {
                Step::Open(name, attributes) if name == "guid" => Some(attributes.clone()),
                Step::Decl | Step::Open(..) | Step::Text(_) | Step::Close(_) => None,
            })
            .expect("an item carries its guid");
        assert_eq!(
            guid_attributes,
            vec![("isPermaLink".to_owned(), "false".to_owned())],
            "the guid is not a permalink"
        );
        let pub_date = text_of(&steps, "pubDate");
        let published =
            DateTime::parse_from_rfc2822(&pub_date).expect("pubDate is an RFC 2822 date");
        assert_eq!(
            published.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "2026-10-15T12:34:56",
            "the pubDate decodes to the fixture instant: {pub_date}"
        );
        let enclosures: Vec<&Vec<(String, String)>> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Open(name, attributes) if name == "enclosure" => Some(attributes),
                Step::Decl | Step::Open(..) | Step::Text(_) | Step::Close(_) => None,
            })
            .collect();
        assert_eq!(enclosures.len(), 2, "both attachments are enclosures");
        assert!(
            enclosures.first().is_some_and(
                |attributes| attributes.contains(&("length".to_owned(), "1234".to_owned()))
            ),
            "the known size travels as length"
        );
        assert!(
            enclosures.get(1).is_some_and(
                |attributes| attributes.contains(&("length".to_owned(), "0".to_owned()))
            ),
            "an unknown size falls back to the required zero"
        );
    }

    #[test]
    fn atom_enclosures_are_links_with_type_and_only_positive_length() {
        let mut feed = sample_feed();
        feed.entries = vec![FeedEntry {
            enclosures: vec![Enclosure {
                url: "https://test.example/media/pic.png".to_owned(),
                mime_type: "image/png".to_owned(),
                size: None,
            }],
            ..entry("post", "<p>body</p>")
        }];
        let document = atom_document(&feed).expect("the feed renders");
        let steps = parse_back(&document);
        let attributes = steps
            .iter()
            .find_map(|step| match step {
                Step::Open(name, attributes)
                    if name == "link"
                        && attributes.contains(&("rel".to_owned(), REL_ENCLOSURE.to_owned())) =>
                {
                    Some(attributes.clone())
                }
                Step::Decl | Step::Open(..) | Step::Text(_) | Step::Close(_) => None,
            })
            .expect("the attachment is an enclosure link");
        assert!(
            attributes.contains(&("type".to_owned(), "image/png".to_owned())),
            "the media type travels"
        );
        assert!(
            !attributes.iter().any(|(key, _)| key == "length"),
            "no length is guessed: {attributes:?}"
        );
    }

    #[test]
    fn timestamps_are_rfc_3339_in_atom_and_rfc_2822_in_rss() {
        let feed = sample_feed();
        let atom = atom_document(&feed).expect("the feed renders");
        let atom_steps = parse_back(&atom);
        let updated = text_of(&atom_steps, "updated");
        assert_eq!(updated, "2026-10-15T12:34:56.789Z", "millisecond Z form");
        assert!(
            DateTime::parse_from_rfc3339(&updated).is_ok(),
            "{updated} is RFC 3339"
        );
        let rss = rss_document(&feed).expect("the feed renders");
        let rss_steps = parse_back(&rss);
        let built = text_of(&rss_steps, "lastBuildDate");
        let parsed =
            DateTime::parse_from_rfc2822(&built).expect("lastBuildDate is an RFC 2822 date");
        assert_eq!(
            parsed.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "2026-10-15T12:34:56",
            "and it carries the same instant"
        );
    }

    #[test]
    fn an_empty_feed_is_still_a_valid_document() {
        let mut feed = sample_feed();
        feed.entries = Vec::new();
        feed.author_name = None;
        let atom = atom_document(&feed).expect("the atom renders");
        let steps = parse_back(&atom);
        assert!(
            !steps
                .iter()
                .any(|step| step == &Step::Open("entry".to_owned(), Vec::new())),
            "no entries"
        );
        assert!(
            !steps
                .iter()
                .any(|step| step == &Step::Open("author".to_owned(), Vec::new())),
            "no author element at all"
        );
        let rss = rss_document(&feed).expect("the rss renders");
        let _steps = parse_back(&rss);
    }

    #[test]
    fn links_carry_self_alternate_and_entry_alternates() {
        let document = atom_document(&sample_feed()).expect("the feed renders");
        let steps = parse_back(&document);
        let links: Vec<&Vec<(String, String)>> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Open(name, attributes) if name == "link" => Some(attributes),
                Step::Decl | Step::Open(..) | Step::Text(_) | Step::Close(_) => None,
            })
            .collect();
        assert_eq!(links.len(), 4, "self, alternate, one per entry");
        assert_eq!(
            links.first().copied().map(Vec::as_slice),
            Some(
                [
                    ("rel".to_owned(), REL_SELF.to_owned()),
                    (
                        "href".to_owned(),
                        "https://test.example/users/alice/feed.atom".to_owned()
                    ),
                    ("type".to_owned(), ATOM_MEDIA_TYPE.to_owned()),
                ]
                .as_slice()
            ),
            "the self link points at this document"
        );
        assert_eq!(
            links.get(1).copied().map(Vec::as_slice),
            Some(
                [
                    ("rel".to_owned(), REL_ALTERNATE.to_owned()),
                    ("href".to_owned(), "https://test.example/alice".to_owned()),
                    ("type".to_owned(), HTML_MEDIA_TYPE.to_owned()),
                ]
                .as_slice()
            ),
            "the alternate link points at the profile"
        );
    }

    #[test]
    fn titles_are_clipped_on_character_boundaries() {
        assert_eq!(clip_title("short", 100), "short", "a short text survives");
        assert_eq!(clip_title("abcdefgh", 3), "abc", "ascii cuts at the count");
        assert_eq!(
            clip_title("héllo wörld", 6),
            "héllo ",
            "multibyte text cuts on chars, not bytes"
        );
        assert_eq!(clip_title("", 100), "", "the empty text stays empty");
    }

    #[test]
    fn the_time_formats_agree_with_the_wire_conventions() {
        let now = fixed_time();
        assert_eq!(atom_time(&now), "2026-10-15T12:34:56.789Z");
        assert_eq!(rss_time(&now), "Thu, 15 Oct 2026 12:34:56 +0000");
    }
}
