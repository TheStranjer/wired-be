//! Rich media cards: the preview card a page's HTML describes.
//!
//! The reference is Pleroma's rich media pipeline,
//! `lib/pleroma/web/rich_media/parser.ex` (`Pleroma.Web.RichMedia.Parser`)
//! and `lib/pleroma/web/rich_media/card.ex` (`Pleroma.Web.RichMedia.Card`).
//! Pleroma fetches the page, runs the Open Graph and Twitter-card parsers
//! over it, and renders the result as the Mastodon `card` object; this
//! module is those parts of the pipeline — [`host_ignored`] gates on the
//! instance's ignore list, [`fetch_card`] fetches a page over the
//! SSRF-guarded client, [`parse_card`] and [`card_json`] parse and render
//! it, and [`stored_card_json`] answers a cached [`LinkCard`] row back as
//! the entity JSON the fetch job stored in it — while the storage and the
//! job that runs them live elsewhere.
//!
//! The field sources follow the parent ticket's priority: Open Graph meta
//! tags (`og:*`) first, then Twitter card tags (`twitter:*`), then the
//! plain `<title>` element with `meta[name=description]`. A title is
//! required — a page that names none yields no card, like Pleroma's
//! `check_parsed_data`. Images are resolved against the page URL and kept
//! only when the result is an `http(s)` URL: `data:` URIs and scheme-less
//! protocol-relative values that do not resolve to `http(s)` are dropped.
//! Title and description are cut to `MAX_CARD_TITLE` and
//! `MAX_CARD_DESCRIPTION` characters. Every raw `og:*`/`twitter:*` pair
//! found on the page rides along in [`Card::opengraph`], which
//! [`card_json`] exposes under `pleroma.opengraph` the way Pleroma does.

use std::{error::Error, fmt, time::Duration};

use scraper::{Html, Selector};
use serde::Serialize;
use serde_json::{Map, Value, json};
use url::Url;

use crate::{
    config::Config,
    http_client::{HttpClient, HttpError, HttpResponse},
    models::link_card::LinkCard,
    util::format::truncate_chars,
};

/// Maximum number of characters kept from a card title.
const MAX_CARD_TITLE: usize = 255;

/// Maximum number of characters kept from a card description.
const MAX_CARD_DESCRIPTION: usize = 500;

/// The prefixes of the raw Open Graph and Twitter-card pairs kept on a [`Card`].
const RAW_META_PREFIXES: [&str; 2] = ["og:", "twitter:"];

/// The most a [`fetch_card`] response body may grow to: 1 MiB.
const MAX_CARD_BODY_BYTES: usize = 1024 * 1024;

/// The overall budget of one [`fetch_card`]: connect, redirects, body read
/// and parse all included.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The `Accept` header value [`fetch_card`] asks the page with.
const HTML_ACCEPT: &str = "text/html";

/// The preview card one page describes: the fields [`card_json`] renders
/// into the Mastodon `card` entity, plus the raw meta pairs Pleroma exposes
/// under `pleroma.opengraph`.
#[derive(Debug, Clone, Serialize)]
pub struct Card {
    /// The URL of the page the card was parsed from.
    pub url: String,
    /// The card title, cut to `MAX_CARD_TITLE` characters.
    pub title: String,
    /// The card description, cut to `MAX_CARD_DESCRIPTION` characters.
    pub description: String,
    /// The card image: the first `og:image`/`twitter:image` that resolves
    /// against the page URL to an `http(s)` URL, if any.
    pub image: Option<String>,
    /// The page's `og:type` ("" when it set none).
    pub site_type: String,
    /// The page's `og:site_name` ("" when it set none).
    pub site_name: String,
    /// Every raw `og:*`/`twitter:*` meta pair found on the page, keyed by
    /// the full property name.
    pub opengraph: Map<String, Value>,
}

impl PartialEq for Card {
    /// Field-by-field comparison, the shape a derived `PartialEq` would
    /// give. It is written by hand because a derived one is what the
    /// `derive_partial_eq_without_eq` lint denies: [`Card`] holds a
    /// [`Map<String, Value>`], whose JSON numbers are floats, so `Eq` —
    /// the lint's alternative — is not a promise this type can make.
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
            && self.title == other.title
            && self.description == other.description
            && self.image == other.image
            && self.site_type == other.site_type
            && self.site_name == other.site_name
            && self.opengraph == other.opengraph
    }
}

/// Parses one HTML page into the rich media [`Card`] it describes.
///
/// `url` is the page's own URL: it anchors the resolution of relative image
/// URLs and is stored on the card. Returns `None` when the page yields no
/// title from Open Graph, Twitter card tags or the `<title>` element, the
/// one condition under which Pleroma's parser rejects the page.
#[must_use]
pub fn parse_card(html: &str, url: &str) -> Option<Card> {
    let document = Html::parse_document(html);
    let tags = meta_tags(&document);
    let title = pick(&tags, &["og:title", "twitter:title"])
        .map(str::to_owned)
        .or_else(|| page_title(&document))?;
    let base = Url::parse(url).ok();
    let image = pick(&tags, &["og:image", "twitter:image"])
        .and_then(|raw| resolve_image(base.as_ref(), raw));
    Some(Card {
        url: url.to_owned(),
        title: truncate_chars(&title, MAX_CARD_TITLE),
        description: truncate_chars(
            pick(
                &tags,
                &["og:description", "twitter:description", "description"],
            )
            .unwrap_or_default(),
            MAX_CARD_DESCRIPTION,
        ),
        image,
        site_type: pick(&tags, &["og:type"]).unwrap_or_default().to_owned(),
        site_name: pick(&tags, &["og:site_name"])
            .unwrap_or_default()
            .to_owned(),
        opengraph: raw_meta_pairs(&tags),
    })
}

/// The card as the Mastodon entity JSON the API embeds.
///
/// This is the shape Pleroma's `Pleroma.Web.RichMedia.Card` renders:
/// `type` fixed to `"link"`, the author and provider fields blank except a
/// `provider_name` of the page's `og:site_name` or the URL's host, the card
/// image or `null`, and the raw Open Graph pairs under `pleroma.opengraph`.
#[must_use]
pub fn card_json(card: &Card) -> Value {
    let provider_name = if card.site_name.is_empty() {
        host_of(&card.url)
    } else {
        card.site_name.clone()
    };
    json!({
        "url": card.url,
        "title": card.title,
        "description": card.description,
        "type": "link",
        "author_name": "",
        "author_url": "",
        "provider_name": provider_name,
        "provider_url": "",
        "html": "",
        "width": 0,
        "height": 0,
        "image": card.image,
        "embed_url": "",
        "blurhash": null,
        "pleroma": { "opengraph": card.opengraph },
    })
}

/// The [`LinkCard::status`] text of a row whose fetch produced a card:
/// the one outcome that renders as a card (the fetch job writes the same
/// text it stores under).
const STATUS_OK: &str = "ok";

/// The status entity JSON behind one [`link_card`] cache row.
///
/// The fetch job stores [`card_json`]'s own output in
/// [`LinkCard::card`], so an `ok` row answers its cached JSON verbatim;
/// a `failed` row — the remembered dead URL — and a row that stores no
/// card answer `None`, which renders as JSON `null`. The status
/// entity's `card` key and the deprecated
/// `GET /api/v1/statuses/{id}/card` endpoint share this one reading of
/// the cache, so the two can never disagree.
///
/// [`link_card`]: crate::models::link_card
#[must_use]
pub fn stored_card_json(card: &LinkCard) -> Option<Value> {
    card.card.clone().filter(|_card| card.status == STATUS_OK)
}

/// The host of `url` for [`card_json`]'s `provider_name` fallback; "" when
/// the URL does not parse or names no host.
fn host_of(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Whether `url`'s host is on `config`'s rich media ignore list: links to
/// such a host are never fetched and get no card.
///
/// The comparison is case-insensitive and covers only the host (never the
/// port or path). A URL whose host cannot be determined — it does not
/// parse, or names no host — counts as ignored: better to skip the fetch
/// than to guess.
#[must_use]
pub fn host_ignored(config: &Config, url: &str) -> bool {
    let parsed = Url::parse(url).ok();
    let Some(host) = parsed.as_ref().and_then(Url::host_str) else {
        return true;
    };
    config
        .rich_media_ignore_hosts
        .iter()
        .any(|ignored| ignored.eq_ignore_ascii_case(host))
}

/// How fetching one rich media card can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RichMediaError {
    /// A local policy refused the URL before a usable response arrived:
    /// the SSRF guard, a non-`http(s)` or unparsable URL, or a transport
    /// failure that never produced a response.
    Blocked,
    /// The peer answered 4xx/5xx; the payload is the status code.
    HttpStatus(u16),
    /// The response's `content-type` names no HTML media type.
    NotHtml,
    /// The response body passed `MAX_CARD_BODY_BYTES`.
    TooLarge,
    /// The fetch and parse ran past the overall timeout budget.
    Timeout,
    /// The page parsed into no card: it names no title anywhere.
    Empty,
}

impl fmt::Display for RichMediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blocked => f.write_str("card URL refused by a local policy"),
            Self::HttpStatus(code) => write!(f, "card peer answered {code}"),
            Self::NotHtml => f.write_str("card peer answered a non-HTML content type"),
            Self::TooLarge => f.write_str("card page exceeded the body size limit"),
            Self::Timeout => f.write_str("card fetch ran past its timeout budget"),
            Self::Empty => f.write_str("card page names no title, so there is no card"),
        }
    }
}

impl Error for RichMediaError {}

/// The transport half of a card fetch collapses onto the variants that
/// describe it: timeouts and oversized bodies keep their own, a peer
/// status carries its code, and everything that stopped the request
/// before a usable response arrived — an SSRF refusal, an unparsable or
/// non-`http(s)` URL, a transport failure — is [`RichMediaError::Blocked`].
impl From<HttpError> for RichMediaError {
    fn from(err: HttpError) -> Self {
        match err {
            HttpError::InvalidUrl | HttpError::Blocked(_) | HttpError::Connect(_) => Self::Blocked,
            HttpError::Timeout => Self::Timeout,
            HttpError::TooLarge => Self::TooLarge,
            HttpError::Status(code) => Self::HttpStatus(code),
        }
    }
}

/// Fetches `url` over the SSRF-guarded, redirects-pinned `http` client
/// (see [`crate::http_client`]) and answers the [`Card`] the page
/// describes, under the overall `FETCH_TIMEOUT` budget.
///
/// # Errors
///
/// [`RichMediaError::HttpStatus`] when the peer answers 4xx/5xx,
/// [`RichMediaError::NotHtml`] when the response's content type is not
/// HTML, [`RichMediaError::TooLarge`] when the body exceeds 1 MiB,
/// [`RichMediaError::Timeout`] when fetch and parse run past the budget,
/// [`RichMediaError::Empty`] when the page yields no card, and
/// [`RichMediaError::Blocked`] when a local policy (the SSRF guard, an
/// unparsable or non-`http(s)` URL) or a transport failure stopped the
/// request.
pub async fn fetch_card(http: &HttpClient, url: &str) -> Result<Card, RichMediaError> {
    fetch_card_within(http, url, FETCH_TIMEOUT).await
}

/// [`fetch_card`] with the overall budget as a parameter, so a timeout
/// test does not wait the full five seconds (house style: the private
/// `HttpClient::with_timeout`).
#[cfg(test)]
async fn fetch_card_with_timeout(
    http: &HttpClient,
    url: &str,
    budget: Duration,
) -> Result<Card, RichMediaError> {
    fetch_card_within(http, url, budget).await
}

/// Runs one [`fetch_and_parse`] under `budget`: whichever finishes first
/// answers, and an elapsed budget is [`RichMediaError::Timeout`].
async fn fetch_card_within(
    http: &HttpClient,
    url: &str,
    budget: Duration,
) -> Result<Card, RichMediaError> {
    match tokio::time::timeout(budget, fetch_and_parse(http, url)).await {
        Ok(outcome) => outcome,
        Err(_elapsed) => Err(RichMediaError::Timeout),
    }
}

/// One untimed card fetch: a capped `GET` asking for HTML, its status
/// checked, its content type gated, and its body read lossily into
/// [`parse_card`].
async fn fetch_and_parse(http: &HttpClient, url: &str) -> Result<Card, RichMediaError> {
    let response = http
        .get_limited(url, &[("accept", HTML_ACCEPT)], MAX_CARD_BODY_BYTES)
        .await?
        .error_for_status()?;
    if !denotes_html(&response) {
        return Err(RichMediaError::NotHtml);
    }
    let html = String::from_utf8_lossy(&response.body);
    parse_card(&html, url).ok_or(RichMediaError::Empty)
}

/// Whether a response's `content-type` names HTML: `text/html` or a
/// `+html` suffixed media type, with parameters (a charset suffix) and
/// case ignored. A missing or unrepresentable header is not HTML.
fn denotes_html(response: &HttpResponse) -> bool {
    response
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let media_type = value.trim().to_ascii_lowercase();
            media_type.starts_with("text/html") || media_type.contains("+html")
        })
}

/// The page's `<meta>` tags as `(key, content)` pairs in document order,
/// where the key is the tag's `property` attribute (the Open Graph spelling)
/// falling back to its `name` attribute (Twitter and description spelling).
/// Tags without a key or without a `content` attribute contribute nothing.
fn meta_tags(document: &Html) -> Vec<(String, String)> {
    let Some(selector) = Selector::parse("meta").ok() else {
        return Vec::new();
    };
    document
        .select(&selector)
        .filter_map(|element| {
            let key = element.attr("property").or_else(|| element.attr("name"))?;
            let value = element.attr("content")?;
            Some((key.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

/// The trimmed content of the first key of `keys` that holds a non-blank
/// value, in `keys` order: the Open Graph-then-Twitter-then-plain priority
/// of the parent ticket, per field.
fn pick<'a>(tags: &'a [(String, String)], keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        tags.iter()
            .find(|(tag_key, value)| tag_key == key && !value.is_empty())
            .map(|(_, value)| value.as_str())
    })
}

/// The document's `<title>` element text, trimmed; None when the page has
/// no title element or its text is blank.
fn page_title(document: &Html) -> Option<String> {
    let selector = Selector::parse("title").ok()?;
    let text: String = document.select(&selector).next()?.text().collect();
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Resolves a raw image value against the page URL and keeps it only when
/// the result is an `http(s)` URL: relative URLs join onto the base (a
/// protocol-relative value takes the page's scheme), and non-`http(s)`
/// schemes such as `data:` or `ftp:` are rejected after resolution.
fn resolve_image(base: Option<&Url>, raw: &str) -> Option<String> {
    let resolved = match base {
        Some(url) => url.join(raw).ok()?,
        None => Url::parse(raw).ok()?,
    };
    matches!(resolved.scheme(), "http" | "https").then(|| resolved.to_string())
}

/// Whether a meta key belongs in [`Card`]'s raw Open Graph map.
fn is_raw_meta(key: &str) -> bool {
    RAW_META_PREFIXES
        .iter()
        .any(|prefix| key.starts_with(prefix))
}

/// The raw Open Graph map: every meta pair whose key is an `og:` or
/// `twitter:` property, keyed by the full property name, the first tag
/// with a repeated key winning.
fn raw_meta_pairs(tags: &[(String, String)]) -> Map<String, Value> {
    let mut pairs = Map::new();
    for (key, value) in tags.iter().filter(|(key, _value)| is_raw_meta(key)) {
        let _entry: &mut Value = pairs
            .entry(key.clone())
            .or_insert_with(|| Value::String(value.clone()));
    }
    pairs
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        http::{HeaderName, StatusCode, header},
        routing::get,
    };
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;

    /// A full Open Graph page: every field comes from its `og:*` tag, and
    /// every `og:*`/`twitter:*` pair is kept raw.
    #[test]
    fn parses_a_full_opengraph_page() {
        let html = r#"<!DOCTYPE html><html><head>
            <title>Ignored Fallback</title>
            <meta property="og:title" content="The Title">
            <meta property="og:description" content="The Description">
            <meta property="og:image" content="https://cdn.example.com/og.png">
            <meta property="og:type" content="article">
            <meta property="og:site_name" content="Example Blog">
            <meta name="twitter:card" content="summary_large_image">
            <meta name="twitter:image" content="https://cdn.example.com/tw.png">
        </head><body><p>hi</p></body></html>"#;
        let card = parse_card(html, "https://example.com/post/1").expect("an OG page has a title");
        assert_eq!(
            card.url, "https://example.com/post/1",
            "url is the page URL"
        );
        assert_eq!(card.title, "The Title", "og:title wins over <title>");
        assert_eq!(card.description, "The Description", "og:description");
        assert_eq!(
            card.image.as_deref(),
            Some("https://cdn.example.com/og.png"),
            "og:image wins over twitter:image"
        );
        assert_eq!(card.site_type, "article", "og:type");
        assert_eq!(card.site_name, "Example Blog", "og:site_name");
        assert_eq!(
            card.opengraph.get("og:title"),
            Some(&Value::String("The Title".into())),
            "raw pairs keep the full key"
        );
        assert_eq!(
            card.opengraph.get("twitter:card"),
            Some(&Value::String("summary_large_image".into())),
            "twitter pairs are kept too"
        );
        assert_eq!(
            card.opengraph.get("twitter:image"),
            Some(&Value::String("https://cdn.example.com/tw.png".into())),
            "the losing twitter:image still rides along raw"
        );
        assert_eq!(
            card.opengraph.len(),
            7,
            "every og:/twitter: pair, nothing else"
        );
    }

    /// A Twitter-card-only page: its fields come from `twitter:*`, and
    /// `twitter:site` does not become a site name (minimal per parent
    /// ticket).
    #[test]
    fn parses_a_twitter_only_page() {
        let html = r#"<!DOCTYPE html><html><head>
            <meta name="twitter:title" content="TW Title">
            <meta name="twitter:description" content="TW Description">
            <meta name="twitter:image" content="https://cdn.example.net/tw.jpg">
            <meta name="twitter:site" content="@example">
            <meta name="twitter:creator" content="@someone">
        </head><body></body></html>"#;
        let card = parse_card(html, "https://example.net/a").expect("a titled page parses");
        assert_eq!(card.title, "TW Title", "twitter:title");
        assert_eq!(card.description, "TW Description", "twitter:description");
        assert_eq!(
            card.image.as_deref(),
            Some("https://cdn.example.net/tw.jpg"),
            "twitter:image"
        );
        assert_eq!(card.site_type, "", "no og:type means empty");
        assert_eq!(card.site_name, "", "twitter:site is not used as site name");
    }

    /// A page with neither OG nor Twitter meta falls back to the `<title>`
    /// element plus `meta[name=description]`.
    #[test]
    fn falls_back_to_plain_title_and_description() {
        let html = r#"<!DOCTYPE html><html><head>
            <title>  Plain Title  </title>
            <meta name="description" content="Plain description">
        </head><body></body></html>"#;
        let card = parse_card(html, "https://example.org/").expect("a titled page parses");
        assert_eq!(card.title, "Plain Title", "the title element, trimmed");
        assert_eq!(
            card.description, "Plain description",
            "meta[name=description]"
        );
        assert_eq!(card.image, None, "no image means no image");
        assert!(
            card.opengraph.is_empty(),
            "no og:/twitter: pairs were found"
        );
    }

    /// A page that names no title at all yields no card.
    #[test]
    fn a_titleless_page_is_no_card() {
        let html = r#"<!DOCTYPE html><html><head>
            <meta property="og:description" content="But with a description">
            <title>   </title>
        </head><body></body></html>"#;
        assert!(
            parse_card(html, "https://example.com/").is_none(),
            "a blank <title> and no og/twitter title gives no card"
        );
        assert!(
            parse_card("<html><body>nothing</body></html>", "https://example.com/").is_none(),
            "no title anywhere gives no card"
        );
    }

    /// A path-relative `og:image` is resolved against the page URL.
    #[test]
    fn resolves_a_path_relative_image_against_the_page_url() {
        let html = r#"<html><head>
            <meta property="og:title" content="T">
            <meta property="og:image" content="/img/pic.png">
        </head><body></body></html>"#;
        let card = parse_card(html, "https://example.com/deep/path/page.html").expect("parses");
        assert_eq!(
            card.image.as_deref(),
            Some("https://example.com/img/pic.png"),
            "path-relative image joins onto the base URL"
        );
    }

    /// A protocol-relative `og:image` takes the page's scheme.
    #[test]
    fn takes_the_page_scheme_for_a_protocol_relative_image() {
        let html = r#"<html><head>
            <meta property="og:title" content="T">
            <meta property="og:image" content="//cdn.example.com/x.png">
        </head><body></body></html>"#;
        let card = parse_card(html, "https://example.com/").expect("parses");
        assert_eq!(
            card.image.as_deref(),
            Some("https://cdn.example.com/x.png"),
            "protocol-relative image takes the page's scheme"
        );
    }

    /// Images whose resolved scheme is not http(s) — `data:` and `ftp:` —
    /// are dropped without dropping the card.
    #[test]
    fn rejects_non_http_images() {
        for raw in [
            "data:image/png;base64,iVBORw0KGgo=",
            "ftp://example.com/pub/pic.png",
        ] {
            let html = format!(
                r#"<html><head>
                    <meta property="og:title" content="T">
                    <meta property="og:image" content="{raw}">
                </head><body></body></html>"#
            );
            let card = parse_card(&html, "https://example.com/").expect("a titled page parses");
            assert_eq!(card.image, None, "{raw} must not become a card image");
        }
    }

    /// Title and description are cut at 255 and 500 characters.
    #[test]
    fn truncates_title_and_description() {
        let long_title = "t".repeat(MAX_CARD_TITLE + 100);
        let long_description = "d".repeat(MAX_CARD_DESCRIPTION + 100);
        let html = format!(
            r#"<html><head>
                <meta property="og:title" content="{long_title}">
                <meta property="og:description" content="{long_description}">
            </head><body></body></html>"#
        );
        let card = parse_card(&html, "https://example.com/").expect("a titled page parses");
        assert_eq!(card.title, "t".repeat(MAX_CARD_TITLE), "title cut to 255");
        assert_eq!(
            card.description,
            "d".repeat(MAX_CARD_DESCRIPTION),
            "description cut to 500"
        );
    }

    /// Truncation counts characters, not bytes: a multibyte title is cut on
    /// a character boundary and stays valid UTF-8.
    #[test]
    fn truncation_stays_on_a_char_boundary() {
        let title = "é日".repeat(200); // 400 chars, far more bytes
        let html = format!(
            r#"<html><head>
                <meta property="og:title" content="{title}">
            </head><body></body></html>"#
        );
        let card = parse_card(&html, "https://example.com/").expect("a titled page parses");
        assert_eq!(
            card.title.chars().count(),
            MAX_CARD_TITLE,
            "exactly 255 characters are kept"
        );
        assert_eq!(card.title, title.chars().take(255).collect::<String>());
        assert!(card.title.is_char_boundary(card.title.len()), "valid UTF-8");
    }

    /// `card_json` produces exactly the Mastodon card entity: fixed fields,
    /// the site name as provider, the image or null, and the raw Open Graph
    /// pairs under `pleroma.opengraph`.
    #[test]
    fn card_json_has_the_exact_entity_shape() {
        let card = Card {
            url: "https://example.com/post/1".to_owned(),
            title: "The Title".to_owned(),
            description: "The Description".to_owned(),
            image: Some("https://cdn.example.com/og.png".to_owned()),
            site_type: "article".to_owned(),
            site_name: "Example Blog".to_owned(),
            opengraph: Map::from_iter([(
                "og:type".to_owned(),
                Value::String("article".to_owned()),
            )]),
        };
        assert_eq!(
            card_json(&card),
            json!({
                "url": "https://example.com/post/1",
                "title": "The Title",
                "description": "The Description",
                "type": "link",
                "author_name": "",
                "author_url": "",
                "provider_name": "Example Blog",
                "provider_url": "",
                "html": "",
                "width": 0,
                "height": 0,
                "image": "https://cdn.example.com/og.png",
                "embed_url": "",
                "blurhash": null,
                "pleroma": { "opengraph": { "og:type": "article" } },
            }),
            "the full entity matches key for key"
        );
    }

    /// Without a site name, `provider_name` falls back to the URL's host,
    /// and a missing image renders as null.
    #[test]
    fn card_json_falls_back_to_the_host() {
        let card = Card {
            url: "https://sub.example.co.uk:4000/a/b?c=1".to_owned(),
            title: "T".to_owned(),
            description: String::new(),
            image: None,
            site_type: String::new(),
            site_name: String::new(),
            opengraph: Map::new(),
        };
        let value = card_json(&card);
        assert_eq!(
            value["provider_name"],
            Value::String("sub.example.co.uk".to_owned()),
            "provider_name falls back to the URL host"
        );
        assert_eq!(value["image"], Value::Null, "no image is JSON null");
        assert_eq!(
            value["pleroma"]["opengraph"],
            Value::Object(Map::new()),
            "the raw map is always present"
        );
        assert_eq!(
            value["description"],
            Value::String(String::new()),
            "an empty description stays an empty string"
        );
    }

    /// `stored_card_json` answers the cached card of an `ok` row
    /// verbatim — the fetch job stores [`card_json`]'s output there — and
    /// nothing for a `failed` row or one that stores no card.
    #[test]
    fn stored_card_json_renders_the_cached_card_or_nothing() {
        let stored = json!({
            "url": "https://example.test",
            "title": "Example",
            "pleroma": { "opengraph": {} },
        });
        let ok = cache_row("ok", Some(stored.clone()));
        assert_eq!(
            stored_card_json(&ok).as_ref(),
            Some(&stored),
            "an ok row's card renders exactly as stored"
        );
        assert_eq!(
            stored_card_json(&cache_row("ok", None)),
            None,
            "a row that parsed no card renders nothing"
        );
        assert_eq!(
            stored_card_json(&cache_row("failed", None)),
            None,
            "a remembered dead fetch renders nothing"
        );
        assert_eq!(
            stored_card_json(&cache_row("failed", Some(stored))),
            None,
            "and even a card stored under a failure never renders"
        );
    }

    /// A [`LinkCard`] cache row for the URL [`cache_row`] names, holding
    /// `card` under fetch outcome `status`.
    fn cache_row(status: &str, card: Option<Value>) -> LinkCard {
        LinkCard {
            url: "https://example.test".to_owned(),
            status: status.to_owned(),
            card,
            fetched_at: chrono::Utc::now(),
        }
    }

    /// `User-Agent` of the test fetch clients.
    const TEST_UA: &str = "Wired (https://wired.test)";
    /// An overall budget small enough that a stalling server loses the race.
    const SHORT_BUDGET: Duration = Duration::from_millis(50);
    /// A delay far past `SHORT_BUDGET`, served by the stalling handler.
    const STALL: Duration = Duration::from_secs(30);
    /// A minimal Open Graph page, served labelled as HTML by the mocks.
    const OG_PAGE: &str = r#"<html><head>
        <meta property="og:title" content="Fetched Title">
        <meta property="og:description" content="Fetched Description">
        <meta property="og:image" content="https://cdn.example.com/pic.png">
    </head><body></body></html>"#;

    /// Binds an unused loopback port.
    async fn bind() -> TcpListener {
        TcpListener::bind("127.0.0.1:0")
            .await
            .expect("free loopback port")
    }

    /// The `path` on `listener` as an absolute `http` URL.
    fn url_for(listener: &TcpListener, path: &str) -> String {
        format!(
            "http://{}{path}",
            listener.local_addr().expect("bound address")
        )
    }

    /// Serves `app` on `listener` until the caller aborts the task.
    fn spawn_server(listener: TcpListener, app: Router) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test server stopped: {err}");
            }
        })
    }

    /// Answers 200 with [`OG_PAGE`] labelled `text/html; charset=utf-8`.
    async fn html_page() -> ([(HeaderName, &'static str); 1], &'static str) {
        (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            OG_PAGE,
        )
    }

    /// Answers 200 with JSON: the content-type gate must refuse it.
    async fn json_page() -> ([(HeaderName, &'static str); 1], &'static str) {
        (
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"og:title":"Not HTML"}"#,
        )
    }

    /// Answers 200 with HTML that names no title: a parse, but no card.
    async fn titleless_page() -> ([(HeaderName, &'static str); 1], &'static str) {
        (
            [(header::CONTENT_TYPE, "text/html")],
            "<html><body>no title here</body></html>",
        )
    }

    /// Answers 200 with an HTML body one byte past the fetch cap.
    async fn oversized_page() -> ([(HeaderName, &'static str); 1], String) {
        (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "x".repeat(MAX_CARD_BODY_BYTES + 1),
        )
    }

    /// Answers 404 with an empty body.
    async fn missing_page() -> StatusCode {
        StatusCode::NOT_FOUND
    }

    /// Accepts the request and then never answers: only the budget ends it.
    async fn stalling_page() -> StatusCode {
        tokio::time::sleep(STALL).await;
        StatusCode::OK
    }

    /// A served Open Graph page comes back as its card, page URL and all.
    #[tokio::test]
    async fn fetch_card_parses_a_served_page() {
        let listener = bind().await;
        let url = url_for(&listener, "/post");
        let server = spawn_server(listener, Router::new().route("/post", get(html_page)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let card = fetch_card(&client, &url)
            .await
            .expect("a served OG page yields a card");

        assert_eq!(card.url, url, "the card keeps the page URL");
        assert_eq!(card.title, "Fetched Title", "og:title");
        assert_eq!(card.description, "Fetched Description", "og:description");
        assert_eq!(
            card.image.as_deref(),
            Some("https://cdn.example.com/pic.png"),
            "og:image"
        );
        server.abort();
    }

    /// A response labelled `application/json` is refused before parsing.
    #[tokio::test]
    async fn fetch_card_refuses_a_non_html_response() {
        let listener = bind().await;
        let url = url_for(&listener, "/api");
        let server = spawn_server(listener, Router::new().route("/api", get(json_page)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let result = fetch_card(&client, &url).await;

        assert_eq!(
            result.unwrap_err(),
            RichMediaError::NotHtml,
            "JSON is not a card page"
        );
        server.abort();
    }

    /// A body past the 1 MiB cap is refused as `TooLarge`.
    #[tokio::test]
    async fn fetch_card_refuses_an_oversized_body() {
        let listener = bind().await;
        let url = url_for(&listener, "/huge");
        let server = spawn_server(listener, Router::new().route("/huge", get(oversized_page)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let result = fetch_card(&client, &url).await;

        assert_eq!(
            result.unwrap_err(),
            RichMediaError::TooLarge,
            "one byte past the cap is too large"
        );
        server.abort();
    }

    /// A server that accepts and never answers loses the race against the
    /// budget and is refused as `Timeout` (short override so the test does
    /// not wait out the real 5 s).
    #[tokio::test]
    async fn a_stalling_server_times_out() {
        let listener = bind().await;
        let url = url_for(&listener, "/stall");
        let server = spawn_server(listener, Router::new().route("/stall", get(stalling_page)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let result = fetch_card_with_timeout(&client, &url, SHORT_BUDGET).await;

        assert_eq!(
            result.unwrap_err(),
            RichMediaError::Timeout,
            "the overall budget fires while the peer stalls"
        );
        server.abort();
    }

    /// A 404 answer is refused as `HttpStatus` carrying the code.
    #[tokio::test]
    async fn a_404_answer_is_a_http_status_error() {
        let listener = bind().await;
        let url = url_for(&listener, "/gone");
        let server = spawn_server(listener, Router::new().route("/gone", get(missing_page)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let result = fetch_card(&client, &url).await;

        assert_eq!(
            result.unwrap_err(),
            RichMediaError::HttpStatus(404),
            "the peer's 404 is kept as the error's payload"
        );
        server.abort();
    }

    /// An HTML page that names no title parses, but yields no card.
    #[tokio::test]
    async fn a_page_without_a_title_is_empty() {
        let listener = bind().await;
        let url = url_for(&listener, "/blank");
        let server = spawn_server(listener, Router::new().route("/blank", get(titleless_page)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let result = fetch_card(&client, &url).await;

        assert_eq!(
            result.unwrap_err(),
            RichMediaError::Empty,
            "a titleless page is no card"
        );
        server.abort();
    }

    /// An SSRF refusal, like an unparsable URL, is `Blocked`: nothing left
    /// this process.
    #[tokio::test]
    async fn refused_urls_are_blocked_before_parsing() {
        let guarded = HttpClient::new(TEST_UA);

        let loopback = fetch_card(&guarded, "http://127.0.0.1:9/page").await;
        assert_eq!(
            loopback.unwrap_err(),
            RichMediaError::Blocked,
            "the SSRF guard refuses loopback"
        );
        let junk = fetch_card(&guarded, "not a url").await;
        assert_eq!(
            junk.unwrap_err(),
            RichMediaError::Blocked,
            "an unparsable URL never leaves the process"
        );
    }

    /// `host_ignored` matches the list case-insensitively on the host
    /// alone, and reports every other host as fetchable.
    #[test]
    fn host_ignored_matches_the_list_case_insensitively() {
        let mut config = Config::for_tests();
        config.rich_media_ignore_hosts = vec!["Example.ORG".to_owned(), "cdn.example".to_owned()];
        assert!(
            host_ignored(&config, "https://example.org/a/b?c=1"),
            "a listed host hits case-insensitively"
        );
        assert!(
            host_ignored(&config, "http://CDN.EXAMPLE/x"),
            "the second entry hits too"
        );
        assert!(
            host_ignored(&config, "https://example.org:4000/"),
            "the port takes no part in the comparison"
        );
        assert!(
            !host_ignored(&config, "https://ok.example/x"),
            "an unlisted host is fetchable"
        );
        assert!(
            !host_ignored(&config, "https://example.org.evil.test/x"),
            "a lookalike suffix does not hit"
        );
    }

    /// With an empty list nothing is ignored.
    #[test]
    fn an_empty_list_ignores_nothing() {
        let config = Config::for_tests();
        assert!(
            config.rich_media_ignore_hosts.is_empty(),
            "the test config ships no entries"
        );
        assert!(
            !host_ignored(&config, "https://example.org/"),
            "every host stays fetchable"
        );
    }

    /// A URL with no host to look up — unparsable or empty — is ignored,
    /// i.e. never fetched.
    #[test]
    fn an_unparsable_url_is_ignored() {
        let mut config = Config::for_tests();
        config.rich_media_ignore_hosts = vec!["example.org".to_owned()];
        assert!(
            host_ignored(&config, "not a url"),
            "garbage is never fetched"
        );
        assert!(
            host_ignored(&config, ""),
            "the empty string is never fetched"
        );
    }

    /// Every variant has its one-line description, statuses carrying the
    /// code they report.
    #[test]
    fn rich_media_error_display_names_the_cause() {
        assert_eq!(
            RichMediaError::Blocked.to_string(),
            "card URL refused by a local policy",
            "Blocked"
        );
        assert_eq!(
            RichMediaError::HttpStatus(404).to_string(),
            "card peer answered 404",
            "HttpStatus carries the code"
        );
        assert_eq!(
            RichMediaError::NotHtml.to_string(),
            "card peer answered a non-HTML content type",
            "NotHtml"
        );
        assert_eq!(
            RichMediaError::TooLarge.to_string(),
            "card page exceeded the body size limit",
            "TooLarge"
        );
        assert_eq!(
            RichMediaError::Timeout.to_string(),
            "card fetch ran past its timeout budget",
            "Timeout"
        );
        assert_eq!(
            RichMediaError::Empty.to_string(),
            "card page names no title, so there is no card",
            "Empty"
        );
        assert!(
            RichMediaError::Timeout.source().is_none(),
            "no wrapped source"
        );
    }

    /// `denotes_html` gates on the media type, not the whole header: the
    /// `text/html` prefix with any parameters, and `+html` suffix types,
    /// in any case; anything else — including no header at all — is not
    /// HTML.
    #[test]
    fn html_content_type_gate() {
        for (value, expected) in [
            ("text/html", true),
            ("text/html; charset=utf-8", true),
            ("Text/HTML; Charset=UTF-8", true),
            (" application/xhtml+xml;charset=gbk", false),
            ("application/feed+html", true),
            ("application/json", false),
            ("text/plain", false),
        ] {
            let response = served_with_content_type(value);
            assert_eq!(denotes_html(&response), expected, "content-type {value}");
        }
        let headerless = HttpResponse {
            status: 200,
            headers: axum::http::HeaderMap::new(),
            body: bytes::Bytes::new(),
        };
        assert!(
            !denotes_html(&headerless),
            "no content-type header is not HTML"
        );
    }

    /// A 200 response carrying only the `content-type` header `value`.
    fn served_with_content_type(value: &str) -> HttpResponse {
        let mut headers = axum::http::HeaderMap::new();
        drop(headers.insert(
            "content-type",
            axum::http::HeaderValue::from_str(value).expect("a visible header value"),
        ));
        HttpResponse {
            status: 200,
            headers,
            body: bytes::Bytes::from_static(b"<html></html>"),
        }
    }
}
