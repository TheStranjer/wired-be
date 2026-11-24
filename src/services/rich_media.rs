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
//! `MAX_CARD_DESCRIPTION` characters. The first `MAX_RAW_META_PAIRS` raw
//! `og:*`/`twitter:*` pairs found on the page (keys up to `MAX_RAW_META_KEY`
//! characters, values cut to `MAX_RAW_META_VALUE`) ride along in
//! [`Card::opengraph`], which
//! [`card_json`] exposes under `pleroma.opengraph` the way Pleroma does.
//!
//! This root holds the card itself — [`Card`], its limits, [`card_json`]'s
//! rendering and the cache reading of [`stored_card_json`], [`host_ignored`]
//! and [`RichMediaError`] — while the page reading lives in the `parse`
//! child and the capped, content-type-gated, time-budgeted network half
//! lives in the `fetch` child; both are re-exported here, so every path
//! into this service stays as it was.

mod fetch;
mod parse;

use std::{error::Error, fmt};

use serde::Serialize;
use serde_json::{Map, Value, json};
use url::Url;

pub use self::{fetch::fetch_card, parse::parse_card};

use crate::{
    config::Config, http_client::HttpError, models::link_card::LinkCard, services::media_proxy,
    util::url::http_host,
};

/// Maximum number of characters kept from a card title.
const MAX_CARD_TITLE: usize = 255;

/// Maximum number of characters kept from a card description.
const MAX_CARD_DESCRIPTION: usize = 500;

/// Maximum number of keys kept in a card's raw Open Graph map.
const MAX_RAW_META_PAIRS: usize = 32;

/// Maximum number of characters kept from one raw Open Graph value.
const MAX_RAW_META_VALUE: usize = 1000;

/// Maximum number of characters in a raw Open Graph key; longer keys are skipped.
const MAX_RAW_META_KEY: usize = 64;

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

/// The card as the Mastodon entity JSON the API embeds.
///
/// This is the shape Pleroma's `Pleroma.Web.RichMedia.Card` renders:
/// `type` fixed to `"link"`, the author and provider fields blank except a
/// `provider_name` of the page's `og:site_name` or the URL's host, the card
/// image or `null`, and the raw Open Graph pairs under `pleroma.opengraph`.
#[must_use]
pub fn card_json(card: &Card) -> Value {
    let provider_name = if card.site_name.is_empty() {
        // The URL's host for the `provider_name` fallback; "" when it has none.
        http_host(&card.url).unwrap_or_default()
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
/// [`LinkCard::card`], so an `ok` row answers its cached JSON, with one
/// render-time rewrite: a string `image` goes through
/// [`media_proxy::proxy_url`], so a reader's client never fetches the
/// linked page's image directly while the media proxy is enabled (a no-op
/// when it is off, and for local, blacklisted or non-`http(s)` URLs). The
/// stored row stays proxy-agnostic, so toggling the proxy takes effect
/// at once. A `failed` row — the remembered dead URL — and a row that
/// stores no card answer `None`, which renders as JSON `null`. The status
/// entity's `card` key and the deprecated
/// `GET /api/v1/statuses/{id}/card` endpoint share this one reading of
/// the cache, so the two can never disagree.
///
/// [`link_card`]: crate::models::link_card
#[must_use]
pub fn stored_card_json(card: &LinkCard, config: &Config) -> Option<Value> {
    let mut json = card.card.clone().filter(|_card| card.status == STATUS_OK)?;
    if let Some(image) = json.get_mut("image")
        && let Some(url) = image.as_str()
    {
        *image = Value::String(media_proxy::proxy_url(config, url));
    }
    Some(json)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let config = Config::for_tests();
        let ok = cache_row("ok", Some(stored.clone()));
        assert_eq!(
            stored_card_json(&ok, &config).as_ref(),
            Some(&stored),
            "an ok row's card renders exactly as stored"
        );
        assert_eq!(
            stored_card_json(&cache_row("ok", None), &config),
            None,
            "a row that parsed no card renders nothing"
        );
        assert_eq!(
            stored_card_json(&cache_row("failed", None), &config),
            None,
            "a remembered dead fetch renders nothing"
        );
        assert_eq!(
            stored_card_json(&cache_row("failed", Some(stored)), &config),
            None,
            "and even a card stored under a failure never renders"
        );
    }

    /// A signing secret comfortably past the 32-character minimum.
    const PROXY_SECRET: &str = "0123456789abcdef0123456789abcdef01234567";

    /// The remote image the proxy tests cache.
    const REMOTE_IMAGE: &str = "https://remote.example/a.png";

    /// A test [`Config`] with the media proxy switched on.
    fn proxy_config() -> Config {
        let mut config = Config::for_tests();
        config.media_proxy.enabled = true;
        config.media_proxy.secret = Some(PROXY_SECRET.to_owned());
        config
    }

    /// A stored card JSON whose image is `image`.
    fn card_with_image(image: &Value) -> Value {
        json!({ "url": "https://example.test", "title": "T", "image": image })
    }

    /// With the proxy on, an `ok` row's remote image becomes a signed
    /// proxy link that decodes back to the original.
    #[test]
    fn stored_card_json_proxies_the_image_when_the_proxy_is_enabled() {
        let config = proxy_config();
        let row = cache_row("ok", Some(card_with_image(&json!(REMOTE_IMAGE))));
        let rendered = stored_card_json(&row, &config).expect("an ok row renders");
        let image = rendered["image"]
            .as_str()
            .expect("the image stays a string");
        assert!(
            image.starts_with(&format!("{}/proxy/", config.base_url)),
            "the image is a proxy link: {image}"
        );
        let (_, tail) = image.split_once("/proxy/").expect("a proxy path");
        let mut segments = tail.split('/');
        let sig = segments.next().expect("a signature segment");
        let encoded = segments.next().expect("an encoded url segment");
        assert_eq!(
            media_proxy::decode(&config, sig, encoded).as_deref(),
            Ok(REMOTE_IMAGE),
            "the link decodes back to the stored image"
        );
        assert_eq!(rendered["title"], json!("T"), "other fields are untouched");
        assert_eq!(
            row.card,
            Some(card_with_image(&json!(REMOTE_IMAGE))),
            "the stored row stays proxy-agnostic"
        );
    }

    /// With the proxy off, the image is left as the page named it.
    #[test]
    fn stored_card_json_leaves_the_image_when_the_proxy_is_disabled() {
        let row = cache_row("ok", Some(card_with_image(&json!(REMOTE_IMAGE))));
        let rendered = stored_card_json(&row, &Config::for_tests()).expect("an ok row renders");
        assert_eq!(
            rendered["image"],
            json!(REMOTE_IMAGE),
            "a disabled proxy rewrites nothing"
        );
    }

    /// A null image stays null, an absent one stays absent, and a failed
    /// row still answers nothing under an enabled proxy.
    #[test]
    fn stored_card_json_keeps_null_images_and_failed_rows() {
        let config = proxy_config();
        let null_row = cache_row("ok", Some(card_with_image(&Value::Null)));
        assert_eq!(
            stored_card_json(&null_row, &config).expect("renders")["image"],
            Value::Null,
            "a null image stays null"
        );
        let bare = json!({ "url": "https://example.test", "title": "T" });
        let bare_row = cache_row("ok", Some(bare.clone()));
        assert_eq!(
            stored_card_json(&bare_row, &config),
            Some(bare),
            "a card with no image key gains none"
        );
        let failed = cache_row("failed", Some(card_with_image(&json!(REMOTE_IMAGE))));
        assert_eq!(
            stored_card_json(&failed, &config),
            None,
            "a failed row still answers None"
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
}
