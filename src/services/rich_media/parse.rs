//! Reading one page of HTML as the [`Card`] it describes.
//!
//! [`parse_card`] is the whole reading: the page's `<meta>` tags as
//! [`meta_tags`] keys them, [`pick`] applies the Open Graph-then-Twitter-then
//! -plain priority per field, [`page_title`] is the last title source,
//! [`resolve_image`] turns a raw image value into an `http(s)` URL against
//! the page URL, and [`raw_meta_pairs`] — gated by [`is_raw_meta`] — keeps
//! every `og:`/`twitter:` pair for [`Card`]'s `opengraph` map. The card
//! type, its limits and the entity JSON it renders into stand in the module
//! root.

use scraper::{Html, Selector};
use serde_json::{Map, Value};
use url::Url;

use super::{Card, MAX_CARD_DESCRIPTION, MAX_CARD_TITLE, RAW_META_PREFIXES};
use crate::util::format::truncate_chars;

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
}
