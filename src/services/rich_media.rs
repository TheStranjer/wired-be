//! Rich media cards: the preview card a page's HTML describes.
//!
//! The reference is Pleroma's rich media pipeline,
//! `lib/pleroma/web/rich_media/parser.ex` (`Pleroma.Web.RichMedia.Parser`)
//! and `lib/pleroma/web/rich_media/card.ex` (`Pleroma.Web.RichMedia.Card`).
//! Pleroma fetches the page, runs the Open Graph and Twitter-card parsers
//! over it, and renders the result as the Mastodon `card` object; this
//! module is the pure halves of that — the fetch, the storage and the job
//! that runs them live elsewhere.
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

use scraper::{Html, Selector};
use serde::Serialize;
use serde_json::{Map, Value, json};
use url::Url;

use crate::util::format::truncate_chars;

/// Maximum number of characters kept from a card title.
const MAX_CARD_TITLE: usize = 255;

/// Maximum number of characters kept from a card description.
const MAX_CARD_DESCRIPTION: usize = 500;

/// The prefixes of the raw Open Graph and Twitter-card pairs kept on a [`Card`].
const RAW_META_PREFIXES: [&str; 2] = ["og:", "twitter:"];

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

/// The host of `url` for [`card_json`]'s `provider_name` fallback; "" when
/// the URL does not parse or names no host.
fn host_of(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_default()
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
}
