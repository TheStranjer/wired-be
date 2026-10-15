//! Atom and RSS feeds over HTTP (`src/api/feeds.rs`).
//!
//! The account feeds serve a local user's newest public root posts and the
//! tag feeds the newest public tagged posts, as `Atom` or `RSS` documents,
//! negotiated by the route suffix or the `Accept` header. These tests own
//! the routes: media types, the parsed-back documents (well-formedness,
//! escaping, entry sets and order, enclosures), pagination, the 404/410
//! answers of the account routes, the empty-valid-feed answer of an
//! unknown tag, and the tag-name normalization. The XML builders are
//! covered where they live, in `src/feeds.rs`, and the pure request
//! handling (negotiation, cursor parsing, the restriction gate) in
//! `src/api/feeds.rs`.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use quick_xml::{Reader, XmlVersion, events::BytesStart, events::Event};
use sqlx::PgPool;
use wired_be::models::{
    media_attachment::{self, MediaAttachment, NewMediaAttachment},
    status::{self, NewStatus, Status, Visibility},
    user::{Flag, NewUser, User},
};

use crate::common;

/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";
/// The media type of an Atom answer, charset included.
const ATOM_TYPE: &str = "application/atom+xml; charset=utf-8";
/// The media type of an RSS answer, charset included.
const RSS_TYPE: &str = "application/rss+xml; charset=utf-8";
/// The XML version the reader decodes attribute values against.
const XML_VERSION: XmlVersion = XmlVersion::Explicit1_0;

/// One parsed XML element: its name, attributes, direct text (entities
/// resolved) and child elements. Parsing with `quick-xml`'s reader is the
/// well-formedness check: an unmatched tag, an unclosed element or a bare
/// ampersand panics in [`parse_xml`] before any assertion runs.
#[derive(Debug)]
struct Node {
    /// The element's local name.
    name: String,
    /// Its attributes, in document order, names exactly as written.
    attributes: Vec<(String, String)>,
    /// Its direct text, with the entity references resolved.
    text: String,
    /// Its child elements, in document order.
    children: Vec<Self>,
}

impl Node {
    /// The node an opening (or empty) tag starts.
    fn opening(start: &BytesStart<'_>) -> Self {
        let attributes = start
            .attributes()
            .map(|attribute| {
                let attribute = attribute.expect("the attributes parse");
                (
                    attribute.key.as_ref().to_owned(),
                    attribute
                        .normalized_value(XML_VERSION)
                        .expect("the values decode")
                        .into_owned(),
                )
            })
            .collect();
        Self {
            name: start.local_name().as_ref().to_owned(),
            attributes,
            text: String::new(),
            children: Vec::new(),
        }
    }

    /// The child elements named `name`, in document order.
    fn all(&self, name: &str) -> Vec<&Self> {
        self.children
            .iter()
            .filter(|child| child.name == name)
            .collect()
    }

    /// The first child element named `name`, if any.
    fn first(&self, name: &str) -> Option<&Self> {
        self.all(name).into_iter().next()
    }

    /// The first child element named `name`.
    ///
    /// # Panics
    ///
    /// If no such child exists.
    fn one(&self, name: &str) -> &Self {
        self.first(name)
            .unwrap_or_else(|| panic!("{} has no {name} child", self.name))
    }

    /// The value of attribute `name`, if present.
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The first child element with attribute `key` set to `value`.
    ///
    /// # Panics
    ///
    /// If no child carries the attribute value.
    fn with_attribute(&self, name: &str, key: &str, value: &str) -> &Self {
        self.all(name)
            .into_iter()
            .find(|child| child.attribute(key) == Some(value))
            .unwrap_or_else(|| panic!("{} has a {name} with {key}={value}", self.name))
    }
}

/// Parses `document` into its root [`Node`].
///
/// # Panics
///
/// If the document is not well-formed XML or carries an undefined entity.
fn parse_xml(document: &str) -> Node {
    let mut reader = Reader::from_str(document);
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut pending = String::new();
    loop {
        match reader.read_event().expect("the feed is well-formed XML") {
            Event::Start(start) => {
                drain_text(&mut stack, &mut pending);
                stack.push(Node::opening(&start));
            }
            Event::Empty(empty) => {
                drain_text(&mut stack, &mut pending);
                attach(&mut stack, &mut root, Node::opening(&empty));
            }
            Event::End(_) => {
                drain_text(&mut stack, &mut pending);
                let closing = stack.pop().expect("end tags match start tags");
                attach(&mut stack, &mut root, closing);
            }
            Event::Text(text) => pending.push_str(&text.xml10_content()),
            Event::GeneralRef(reference) => {
                pending.push('&');
                pending.push_str(reference.into_inner().as_ref());
                pending.push(';');
            }
            Event::Eof => {
                assert!(stack.is_empty(), "every element is closed");
                pending.clear();
                return root.expect("the document has a root element");
            }
            other @ (Event::Decl(_)
            | Event::CData(_)
            | Event::Comment(_)
            | Event::PI(_)
            | Event::DocType(_)) => drop(other),
        }
    }
}

/// Resolves the text run collected so far into the innermost open element.
/// The reader splits entity references out of the text, so the run is
/// rebuilt as the `&name;` spellings it arrived in and decoded once.
///
/// # Panics
///
/// If the run carries an undefined entity.
fn drain_text(stack: &mut [Node], pending: &mut String) {
    if pending.is_empty() {
        return;
    }
    let decoded = quick_xml::escape::unescape(pending)
        .expect("the entities are defined")
        .into_owned();
    pending.clear();
    if let Some(top) = stack.last_mut() {
        top.text.push_str(&decoded);
    }
}

/// Places a finished node in its parent, or into `root` when it closed the
/// outermost element.
fn attach(stack: &mut [Node], root: &mut Option<Node>, node: Node) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else {
        assert!(root.is_none(), "one root element");
        *root = Some(node);
    }
}

/// Requests `path` and asserts its status and media type, handing back the
/// parsed document root.
async fn fetch_feed(app: &Router, path: &str, expect_type: &str) -> Node {
    let response = common::TestRequest::get(path).send(app).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{path}: {}",
        response.text()
    );
    assert_eq!(
        response.header("content-type"),
        Some(expect_type),
        "{path} media type"
    );
    parse_xml(&response.text())
}

/// The ids of the Atom entries of `feed`, in document order.
fn entry_ids(feed: &Node) -> Vec<String> {
    feed.all("entry")
        .into_iter()
        .map(|entry| entry.one("id").text.clone())
        .collect()
}

/// The `guid` texts of the RSS items of `channel`, in document order.
fn item_guids(channel: &Node) -> Vec<String> {
    channel
        .all("item")
        .into_iter()
        .map(|item| item.one("guid").text.clone())
        .collect()
}

/// The uri of a stored status, as its feed entry carries it.
fn uri_of(post: &Status) -> String {
    post.uri.clone()
}

/// The uris of `posts`, newest first — the order every feed page must
/// answer them in.
fn uris_newest_first(posts: &[Status]) -> Vec<String> {
    posts.iter().rev().map(uri_of).collect()
}

/// A local active user named `nickname` with display name `name`.
async fn named_user(pool: &PgPool, nickname: &str, name: &str) -> User {
    common::create_local_user_with(pool, nickname, |new| NewUser {
        name: name.to_owned(),
        ..new
    })
    .await
}

/// Inserts `text` as `author`'s status at `visibility`.
async fn visible_post(pool: &PgPool, author: &User, text: &str, visibility: Visibility) -> Status {
    let new = NewStatus {
        visibility,
        ..NewStatus::local(author, text)
    };
    status::insert(pool, BASE, &new)
        .await
        .expect("fixture status inserts")
}

/// Inserts a public answer to `parent` by `author`.
async fn reply_to(pool: &PgPool, author: &User, parent: &Status, text: &str) -> Status {
    let new = NewStatus {
        in_reply_to_id: Some(parent.id),
        ..NewStatus::local(author, text)
    };
    status::insert(pool, BASE, &new)
        .await
        .expect("fixture reply inserts")
}

/// Inserts a public reblog of `original` by `author`.
async fn reblog_of(pool: &PgPool, author: &User, original: &Status) -> Status {
    let new = NewStatus {
        reblog_of_id: Some(original.id),
        ..NewStatus::local(author, "")
    };
    status::insert(pool, BASE, &new)
        .await
        .expect("fixture reblog inserts")
}

/// Attaches one sized image to `post` and returns its row.
async fn attach_media(pool: &PgPool, author: &User, post: &Status) -> MediaAttachment {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            status_id: Some(post.id),
            url: format!("{BASE}/media/{}.png", post.id),
            content_type: "image/png".to_owned(),
            file_size: Some(1234),
            ..Default::default()
        },
    )
    .await
    .expect("fixture media attaches")
}

/// `alice` with a timeline of three public roots plus everything the feed
/// must not show: an unlisted, a private, a direct and a local post, a
/// reply, a reblog and a tombstone. The roots come back oldest first.
async fn alice_with_a_timeline(pool: &PgPool) -> (User, Vec<Status>) {
    let alice = named_user(pool, "alice", "Alice").await;
    let one = common::create_status(pool, &alice, "First post", Visibility::Public).await;
    let _unlisted = visible_post(pool, &alice, "Quiet", Visibility::Unlisted).await;
    let _private = visible_post(pool, &alice, "Friends only", Visibility::Private).await;
    let _direct = visible_post(pool, &alice, "psst", Visibility::Direct).await;
    let _localonly = visible_post(pool, &alice, "local talk", Visibility::Local).await;
    let _reply = reply_to(pool, &alice, &one, "in answer").await;
    let _reblog = reblog_of(pool, &alice, &one).await;
    let two = common::create_status(pool, &alice, "Second post", Visibility::Public).await;
    let doomed = common::create_status(pool, &alice, "Deleted soon", Visibility::Public).await;
    let _tombstone = status::soft_delete(pool, doomed.id)
        .await
        .expect("the tombstone lands");
    let three = common::create_status(pool, &alice, "Third post", Visibility::Public).await;
    (alice, vec![one, two, three])
}

/// The Atom user feed lists exactly the public root posts, newest first,
/// under the ticket's feed metadata: `{name}'s timeline`, the author, the
/// `id`, `updated`, and the `self` and `alternate` links.
#[tokio::test]
async fn atom_user_feed_lists_public_roots_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, posts) = alice_with_a_timeline(&pool).await;
    let app = common::test_app(pool.clone());

    let feed = fetch_feed(&app, "/users/alice/feed.atom", ATOM_TYPE).await;
    assert_eq!(feed.name, "feed", "the Atom root");
    assert_eq!(
        feed.attribute("xmlns"),
        Some("http://www.w3.org/2005/Atom"),
        "the Atom namespace"
    );
    assert_eq!(
        feed.one("title").text,
        "Alice's timeline",
        "the display name titles the feed"
    );
    assert_eq!(
        feed.one("id").text,
        format!("{BASE}/users/alice/feed.atom"),
        "the feed id is its own URL"
    );
    assert_eq!(
        feed.one("author").one("name").text,
        "Alice",
        "the author is the account"
    );
    assert_eq!(
        feed.with_attribute("link", "rel", "self").attribute("href"),
        Some(format!("{BASE}/users/alice/feed.atom").as_str()),
        "the self link points at this document"
    );
    assert_eq!(
        feed.with_attribute("link", "rel", "self").attribute("type"),
        Some("application/atom+xml"),
        "and names the format"
    );
    assert_eq!(
        feed.with_attribute("link", "rel", "alternate")
            .attribute("href"),
        Some(format!("{BASE}/alice").as_str()),
        "the alternate is the frontend profile URL"
    );
    assert_eq!(
        entry_ids(&feed),
        uris_newest_first(&posts),
        "exactly the three public roots, newest first, by uri"
    );
    let newest = feed.one("entry");
    assert_eq!(newest.one("title").text, "Third post", "newest first");
    assert_eq!(
        newest.all("link").len(),
        1,
        "the root feed entry carries only its alternate link"
    );
    for name in ["updated", "published"] {
        let stamp = newest.one(name).text.clone();
        let _published: chrono::DateTime<chrono::FixedOffset> =
            chrono::DateTime::parse_from_rfc3339(&stamp)
                .unwrap_or_else(|err| panic!("{stamp} is RFC 3339: {err}"));
    }
}

/// The account that never set a display name is titled by its nickname.
#[tokio::test]
async fn a_bare_account_titles_its_feed_by_nickname() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let feed = fetch_feed(&app, "/users/alice/feed.atom", ATOM_TYPE).await;
    assert_eq!(
        feed.one("title").text,
        "alice's timeline",
        "empty name falls back to the nickname"
    );
    assert_eq!(feed.one("author").one("name").text, "alice");
}

/// An entry carries the status uri as its `id`, the first 100 characters
/// of the text as its `title`, the stored HTML as `content type="html"`,
/// and an `alternate` link to the notice.
#[tokio::test]
async fn atom_entries_carry_id_title_and_content() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let long = format!("{} tail", "x".repeat(120));
    let post = common::create_status(&pool, &alice, &long, Visibility::Public).await;
    let app = common::test_app(pool.clone());

    let feed = fetch_feed(&app, "/users/alice/feed.atom", ATOM_TYPE).await;
    let entry = feed.one("entry");
    assert_eq!(entry.one("id").text, post.uri, "the entry id is the uri");
    assert_eq!(
        entry.one("title").text,
        "x".repeat(100),
        "the title is the first 100 characters of the text"
    );
    assert_eq!(
        entry.one("content").text,
        post.content,
        "the content is the stored HTML, round-tripped through the escaping"
    );
    assert_eq!(entry.one("content").attribute("type"), Some("html"));
    assert_eq!(
        entry
            .with_attribute("link", "rel", "alternate")
            .attribute("href"),
        post.url.as_deref(),
        "the entry links to the notice"
    );
}

/// The ticket's escaping case: `<` and `&` in a post never break out of
/// the document — the raw body carries only entities there, and the
/// parsed-back text is the original.
#[tokio::test]
async fn markup_and_ampersands_are_escaped_not_interpreted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let _post = common::create_status(
        &pool,
        &alice,
        r#"Tom & Jerry say <hi> & "bye""#,
        Visibility::Public,
    )
    .await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::get("/users/alice/feed.atom")
        .send(&app)
        .await;
    let body = response.text();
    assert!(body.contains("Tom &amp; Jerry"), "{body}");
    assert!(body.contains("&lt;hi&gt;"), "{body}");
    assert!(!body.contains("<hi>"), "no live element from the text");
    let feed = parse_xml(&body);
    assert_eq!(
        feed.one("entry").one("title").text,
        r#"Tom & Jerry say <hi> & "bye""#,
        "the title decodes back to exactly what was posted"
    );
}

/// The RSS feed of the same account is the same page in the other shape:
/// the channel metadata, and items with `guid`, `pubDate` and
/// `description`, plus the ticket's `enclosure` for attached media.
#[tokio::test]
async fn rss_user_feed_is_the_channel_equivalent() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, posts) = alice_with_a_timeline(&pool).await;
    let _attachment = attach_media(&pool, &alice, &posts[2]).await;
    let app = common::test_app(pool.clone());

    let rss = fetch_feed(&app, "/users/alice/feed.rss", RSS_TYPE).await;
    assert_eq!(rss.name, "rss", "the RSS root");
    assert_eq!(rss.attribute("version"), Some("2.0"), "RSS 2.0");
    let channel = rss.one("channel");
    assert_eq!(channel.one("title").text, "Alice's timeline", "the title");
    assert_eq!(
        channel.one("link").text,
        format!("{BASE}/alice"),
        "the channel links to the profile"
    );
    assert_eq!(
        channel.one("description").text,
        alice.bio,
        "the channel description is the account's bio"
    );
    assert_eq!(
        item_guids(channel),
        uris_newest_first(&posts),
        "the same public roots, newest first, by uri"
    );
    let item = channel.one("item");
    assert_eq!(
        item.one("guid").attribute("isPermaLink"),
        Some("false"),
        "the guid is a stable id, not a permalink"
    );
    let pub_date = item.one("pubDate").text.clone();
    let _parsed: chrono::DateTime<chrono::FixedOffset> =
        chrono::DateTime::parse_from_rfc2822(&pub_date)
            .unwrap_or_else(|err| panic!("{pub_date} is an RFC 2822 date: {err}"));
    assert_eq!(
        item.one("description").text,
        posts[2].content,
        "the description is the post's HTML"
    );
    assert_eq!(
        item.one("link").text,
        posts[2].url.clone().unwrap_or_default(),
        "the item links to the notice"
    );
    let enclosure = item.one("enclosure");
    assert_eq!(
        enclosure.attribute("url"),
        Some(format!("{BASE}/media/{}.png", posts[2].id).as_str()),
        "the attachment's URL"
    );
    assert_eq!(enclosure.attribute("type"), Some("image/png"));
    assert_eq!(enclosure.attribute("length"), Some("1234"));
}

/// An Atom entry renders each attachment as a `link rel="enclosure"` with
/// the media type and the stored size.
#[tokio::test]
async fn atom_entries_render_enclosure_links() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "look", Visibility::Public).await;
    let _attachment = attach_media(&pool, &alice, &post).await;
    let app = common::test_app(pool.clone());

    let feed = fetch_feed(&app, "/users/alice/feed.atom", ATOM_TYPE).await;
    let enclosure = feed.one("entry").with_attribute("link", "rel", "enclosure");
    assert_eq!(
        enclosure.attribute("href"),
        Some(format!("{BASE}/media/{}.png", post.id).as_str())
    );
    assert_eq!(enclosure.attribute("type"), Some("image/png"));
    assert_eq!(
        enclosure.attribute("length"),
        Some("1234"),
        "the recorded size travels as length"
    );
}

/// `?max_id=` keys the older page on the status id in both formats; an
/// unusable cursor is the standard 400.
#[tokio::test]
async fn user_feed_pages_older_statuses_by_max_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, posts) = alice_with_a_timeline(&pool).await;
    let app = common::test_app(pool.clone());

    let first = fetch_feed(&app, "/users/alice/feed.atom", ATOM_TYPE).await;
    assert_eq!(
        entry_ids(&first),
        uris_newest_first(&posts),
        "the first page holds all three"
    );
    let second = fetch_feed(
        &app,
        &format!("/users/alice/feed.atom?max_id={}", posts[1].id),
        ATOM_TYPE,
    )
    .await;
    assert_eq!(
        entry_ids(&second),
        vec![uri_of(&posts[0])],
        "the next page is the strictly older post"
    );
    let rss_page = fetch_feed(
        &app,
        &format!("/users/alice/feed.rss?max_id={}", posts[2].id),
        RSS_TYPE,
    )
    .await;
    assert_eq!(
        item_guids(rss_page.one("channel")),
        vec![uri_of(&posts[1]), uri_of(&posts[0])],
        "the cursor pages RSS the same way"
    );

    let bad = common::TestRequest::get("/users/alice/feed.atom?max_id=not-an-id")
        .send(&app)
        .await;
    common::assert_answer(&bad, StatusCode::BAD_REQUEST, "an unusable max_id");
    common::assert_error(&bad, "Invalid id", "an unusable max_id");
}

/// A feed page holds at most twenty statuses, and the cursor reaches the
/// rest.
#[tokio::test]
async fn user_feed_pages_at_twenty_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mut posts = Vec::new();
    for number in 0..21 {
        posts.push(
            common::create_status(
                &pool,
                &alice,
                &format!("status {number}"),
                Visibility::Public,
            )
            .await,
        );
    }
    let app = common::test_app(pool.clone());

    let first = fetch_feed(&app, "/users/alice/feed.atom", ATOM_TYPE).await;
    assert_eq!(feed_len(&first), 20, "the page holds 20 entries");
    assert_eq!(
        entry_ids(&first),
        uris_newest_first(&posts[1..]),
        "the newest 20, in order"
    );
    let oldest = &posts[0];
    let second = fetch_feed(
        &app,
        &format!("/users/alice/feed.atom?max_id={}", oldest.id),
        ATOM_TYPE,
    )
    .await;
    assert!(entry_ids(&second).is_empty(), "past the end is empty");
    let tail = fetch_feed(
        &app,
        &format!("/users/alice/feed.atom?max_id={}", posts[1].id),
        ATOM_TYPE,
    )
    .await;
    assert_eq!(
        entry_ids(&tail),
        vec![uri_of(oldest)],
        "and the page after the last shown one carries the oldest post"
    );
}

/// The number of entries in a parsed Atom feed.
fn feed_len(feed: &Node) -> usize {
    feed.all("entry").len()
}

/// On the extensionless route an `Accept` naming `application/rss+xml`
/// gets RSS and anything else gets Atom; the answer says it varies.
#[tokio::test]
async fn the_user_feed_negotiates_the_format_on_accept() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, posts) = alice_with_a_timeline(&pool).await;
    let app = common::test_app(pool.clone());

    let rss = common::TestRequest::get("/users/alice/feed")
        .header("accept", "application/rss+xml")
        .send(&app)
        .await;
    assert_eq!(rss.status, StatusCode::OK, "the RSS client is served");
    assert_eq!(rss.header("content-type"), Some(RSS_TYPE), "RSS answer");
    assert_eq!(rss.header("vary"), Some("Accept"), "the header varies");
    assert_eq!(
        item_guids(parse_xml(&rss.text()).one("channel")),
        uris_newest_first(&posts),
        "the same page as the .rss route"
    );

    for accept in [
        "application/atom+xml",
        "application/rss+xmlz",
        "text/html",
        "*/*",
        "application/xml",
    ] {
        let atom = common::TestRequest::get("/users/alice/feed")
            .header("accept", accept)
            .send(&app)
            .await;
        assert_eq!(atom.status, StatusCode::OK, "{accept} is served");
        assert_eq!(
            atom.header("content-type"),
            Some(ATOM_TYPE),
            "{accept} falls back to Atom"
        );
        assert_eq!(atom.header("vary"), Some("Accept"), "{accept} varies");
    }
    let bare = common::TestRequest::get("/users/alice/feed")
        .send(&app)
        .await;
    assert_eq!(
        bare.header("content-type"),
        Some(ATOM_TYPE),
        "no Accept is Atom"
    );
}

/// Unknown and remote nicknames are 404s and a deactivated account is 410,
/// as JSON, on every one of the account feed routes.
#[tokio::test]
async fn unknown_remote_and_deactivated_users_are_404_or_410() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let erin = named_user(&pool, "erin", "Erin").await;
    let _kept = common::create_status(&pool, &erin, "bye", Visibility::Public).await;
    let _gone = common::create_local_user_with(&pool, "gone", |new| NewUser {
        is_active: Flag::new(false),
        ..new
    })
    .await;
    let app = common::test_app(pool.clone());

    for path in [
        "/users/nobody/feed.atom",
        "/users/nobody/feed.rss",
        "/users/nobody/feed",
        &format!("/users/{}/feed.atom", bob.nickname),
        &format!("/users/{}/feed", bob.nickname),
    ] {
        let response = common::TestRequest::get(path).send(&app).await;
        common::assert_answer(&response, StatusCode::NOT_FOUND, path);
        common::assert_error(&response, "Not found", path);
    }
    for path in [
        "/users/gone/feed.atom",
        "/users/gone/feed.rss",
        "/users/gone/feed",
    ] {
        let response = common::TestRequest::get(path).send(&app).await;
        common::assert_answer(&response, StatusCode::GONE, path);
        common::assert_error(&response, "Gone", path);
    }
    let live = common::TestRequest::get("/users/erin/feed.atom")
        .send(&app)
        .await;
    assert_eq!(live.status, StatusCode::OK, "a live account is served");
}

/// The tag feed lists the newest public statuses carrying the tag, from
/// every author, and excludes the unlisted posts and the replies. The
/// path segment is normalized like the stored name: `Rust` finds `rust`.
#[tokio::test]
async fn tag_feed_lists_public_tagged_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let a = common::create_status(&pool, &alice, "Learning #Rust today", Visibility::Public).await;
    let b = common::create_status(&pool, &bob, "Rusty #RUST news", Visibility::Public).await;
    let _unlisted = visible_post(&pool, &alice, "Quiet #rust", Visibility::Unlisted).await;
    let _reply = reply_to(&pool, &bob, &a, "nice #rust").await;
    let app = common::test_app(pool.clone());

    let feed = fetch_feed(&app, "/tags/rust.atom", ATOM_TYPE).await;
    assert_eq!(feed.name, "feed", "the Atom root of the tag feed");
    assert_eq!(
        feed.one("title").text,
        "#rust",
        "the requested tag titles it"
    );
    assert!(feed.first("author").is_none(), "a tag has no feed author");
    assert_eq!(
        entry_ids(&feed),
        vec![uri_of(&b), uri_of(&a)],
        "newest first across authors, the unlisted post and the reply out"
    );
    let rss = fetch_feed(&app, "/tags/RUST.rss", RSS_TYPE).await;
    assert_eq!(
        item_guids(rss.one("channel")),
        vec![uri_of(&b), uri_of(&a)],
        "the name lookup normalizes, and the RSS page matches"
    );
}

/// An explicit `.rss`/`.atom` suffix fixes the format whatever the
/// `Accept` header says, while the extensionless tag route negotiates.
#[tokio::test]
async fn tag_feed_suffixes_beat_the_accept_header() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let _post = common::create_status(&pool, &alice, "tagged #rss", Visibility::Public).await;
    let app = common::test_app(pool.clone());

    let atom = common::TestRequest::get("/tags/rss.atom")
        .header("accept", "application/rss+xml")
        .send(&app)
        .await;
    assert_eq!(
        atom.header("content-type"),
        Some(ATOM_TYPE),
        "the .atom suffix wins over Accept"
    );
    assert_eq!(parse_xml(&atom.text()).one("title").text, "#rss");

    let rss = common::TestRequest::get("/tags/rss")
        .header("accept", "application/rss+xml")
        .send(&app)
        .await;
    assert_eq!(
        rss.header("content-type"),
        Some(RSS_TYPE),
        "without a suffix the header decides"
    );
    assert_eq!(rss.header("vary"), Some("Accept"), "and it varies");
    let atom_by_header = common::TestRequest::get("/tags/rss")
        .header("accept", "application/atom+xml")
        .send(&app)
        .await;
    assert_eq!(
        atom_by_header.header("content-type"),
        Some(ATOM_TYPE),
        "the bare tag route answers Atom"
    );
}

/// An unknown tag is not an error: it answers a valid, empty feed.
#[tokio::test]
async fn an_unknown_tag_answers_a_valid_empty_feed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());

    let atom = fetch_feed(&app, "/tags/nosuchtag.atom", ATOM_TYPE).await;
    assert!(atom.all("entry").is_empty(), "no entries");
    assert_eq!(atom.one("title").text, "#nosuchtag", "the title names it");
    assert_eq!(
        atom.with_attribute("link", "rel", "self").attribute("href"),
        Some(format!("{BASE}/tags/nosuchtag.atom").as_str()),
        "the self link is the URL it was served at"
    );
    let rss = fetch_feed(&app, "/tags/nosuchtag.rss", RSS_TYPE).await;
    assert_eq!(rss.one("channel").all("item").len(), 0, "no items");
    let negotiated = common::TestRequest::get("/tags/nosuchtag").send(&app).await;
    assert_eq!(
        negotiated.header("content-type"),
        Some(ATOM_TYPE),
        "and the negotiated route answers Atom"
    );
    let _empty = parse_xml(&negotiated.text());
}

/// An authenticated request is served exactly like an anonymous one — a
/// feed is open by nature on a public instance, and the restriction gate
/// (unit-tested in `src/api/feeds.rs`) never refuses a live user token.
#[tokio::test]
async fn an_authenticated_feed_request_is_served() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "public", Visibility::Public).await;
    let reader = common::create_local_user(&pool, "reader").await;
    let token = common::create_app_and_token(&pool, &reader, "read").await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::get("/users/alice/feed.atom")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the token reads the feed");
    assert_eq!(
        entry_ids(&parse_xml(&response.text())),
        vec![uri_of(&post)],
        "with its own query parameters intact"
    );
    let with_cursor = common::TestRequest::get(&format!(
        "/users/alice/feed.atom?max_id={}&access_token={token}",
        post.id
    ))
    .send(&app)
    .await;
    assert_eq!(
        with_cursor.status,
        StatusCode::OK,
        "the token also rides the query string"
    );
    assert!(
        entry_ids(&parse_xml(&with_cursor.text())).is_empty(),
        "and the cursor still pages"
    );
}
