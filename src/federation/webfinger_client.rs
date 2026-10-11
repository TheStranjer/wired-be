//! Client-side `WebFinger`: resolving an `acct:nick@domain` handle to the
//! `ActivityPub` actor URL of a remote account.
//!
//! [`resolve_acct`] is the lookup this instance starts every remote-follow
//! and remote-fetch with. The reference behaviour is Pleroma's
//! `Pleroma.Web.WebFinger.finger/1`
//! (`lib/pleroma/web/web_finger.ex`) and its controller
//! (`lib/pleroma/web/web_finger/web_finger_controller.ex`); the ticket rules
//! here are authoritative where the two differ.
//!
//! The protocol shape, per RFC 7033:
//!
//! 1. `GET https://{domain}/.well-known/webfinger?resource=acct:{nick}@{domain}`
//!    asking for `application/jrd+json`, with the body capped at 256 KiB.
//! 2. When that answers 404 or 410, or the network fails, `GET`
//!    `https://{domain}/.well-known/host-meta` (XRD), take the
//!    `<Link rel="lrdd" template="...">` template, expand its `{uri}` macro
//!    with the `acct:` resource and fetch that URL instead. A successful
//!    first attempt never consults host-meta.
//! 3. The actor URL is the `href` of the first `rel=self` link whose `type`
//!    is `application/activity+json` or `application/ld+json` (parameters
//!    and whitespace in the declared type are ignored). A
//!    `rel=http://webfinger.net/rel/profile-page` link only confirms that the
//!    host knows the account; it points at an HTML page, never at an actor
//!    document, so with no usable self link the lookup fails as
//!    [`WebfingerError::NoActorLink`] rather than falling back to it.
//! 4. Both representations are read: JRD JSON, and XRD XML through the
//!    `quick-xml` event reader. The format is decided by the response's
//!    content type, falling back to the document's first character when the
//!    type is absent or unknown.
//!
//! Every resolved document is also checked for domain containment (see
//! [`resolve_acct`]): a host must not be able to point this instance at an
//! actor on someone else's host.

use std::{error::Error, fmt};

use quick_xml::{XmlVersion, escape::unescape, events::BytesStart, events::Event, reader::Reader};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap};
use serde::Deserialize;
use tracing::debug;
use url::Url;

use crate::http_client::{HttpClient, HttpError, HttpResponse};

/// Response body cap for every `WebFinger` exchange (256 KiB): JRD and XRD
/// documents are small, and a hostile host must not be able to stream an
/// endless one into this process.
const MAX_BODY_BYTES: usize = 256 * 1024;
/// Media type asked for on every `WebFinger` GET (RFC 7033's JRD).
const JRD_JSON: &str = "application/jrd+json";
/// Media type sent as `Accept` on the host-meta GET (RFC 7033's XRD).
const XRD_XML: &str = "application/xrd+xml";
/// The `rel` naming the actor document.
const SELF_REL: &str = "self";
/// The `rel` of the LRDD template link inside host-meta.
const LRDD_REL: &str = "lrdd";
/// First accepted media type of the self link: `ActivityPub` JSON.
const ACTIVITY_JSON: &str = "application/activity+json";
/// Second accepted media type of the self link. Its `profile` parameter is
/// never compared — the ticket fixes "ignore parameters" — so any
/// `application/ld+json` (with or without parameters) names the actor.
const LD_JSON: &str = "application/ld+json";
/// The resource macro of an LRDD template (RFC 7033 Section 5).
const URI_MACRO: &str = "{uri}";
/// The percent-encoded spelling of [`URI_MACRO`], as Mastodon emits it.
const URI_MACRO_ENCODED: &str = "%7Buri%7D";
/// The `acct:` URI scheme prefix `WebFinger` resources carry.
const ACCT_PREFIX: &str = "acct:";
/// XRD is an XML 1.0 vocabulary; attribute values are normalized and
/// unescaped under those rules.
const XRD_XML_VERSION: XmlVersion = XmlVersion::Explicit1_0;

/// How an `acct` to actor URL resolution can fail.
#[derive(Debug)]
pub enum WebfingerError {
    /// The handle is not a `nick@domain` this client may look up: empty or
    /// at-sign-less, or with characters outside the nickname and hostname
    /// alphabets (see [`split_acct`]).
    InvalidAcct,
    /// A request failed or answered 4xx/5xx: the transport and status
    /// failures of [`crate::http_client`].
    Http(HttpError),
    /// The document carried no `rel=self` link with an `ActivityPub` media
    /// type: the account has no actor URL to return. A profile-page link
    /// does not count.
    NoActorLink,
    /// The document does not belong to the requested domain: neither its
    /// subject nor its actor URL names that domain (see [`resolve_acct`]).
    DomainMismatch,
    /// The body is neither a parseable JRD document nor a parseable XRD
    /// one (or it is not valid UTF-8).
    Malformed,
}

impl fmt::Display for WebfingerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAcct => f.write_str("not a valid acct:nick@domain handle"),
            Self::Http(err) => write!(f, "webfinger request failed: {err}"),
            Self::NoActorLink => f.write_str("webfinger document has no ActivityPub self link"),
            Self::DomainMismatch => {
                f.write_str("webfinger result does not belong to the requested domain")
            }
            Self::Malformed => f.write_str("webfinger document is malformed"),
        }
    }
}

impl Error for WebfingerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(err) => Some(err),
            Self::InvalidAcct | Self::NoActorLink | Self::DomainMismatch | Self::Malformed => None,
        }
    }
}

/// Any HTTP failure is a [`WebfingerError::Http`], so `?` works throughout
/// the lookup.
impl From<HttpError> for WebfingerError {
    fn from(err: HttpError) -> Self {
        Self::Http(err)
    }
}

/// Turns any parse-level cause into [`WebfingerError::Malformed`], after
/// logging what the document actually failed on.
fn note_malformed<Cause: fmt::Display>(cause: Cause) -> WebfingerError {
    debug!(%cause, "could not parse a webfinger document");
    WebfingerError::Malformed
}

/// Resolves a remote handle to its `ActivityPub` actor URL.
///
/// `acct` names the account as `nick@domain`, optionally prefixed with `@`
/// and/or `acct:` (both are stripped before validation). The document rules
/// are the ones in this module's [docs][self]; the returned URL is the self
/// link's `href` verbatim.
///
/// The answer must belong to the requested domain: either its `subject` (if
/// present) names that domain, or the actor URL's host equals it. Mastodon
/// enforces this harder with a full round trip: it re-resolves the returned
/// `acct:` subject and requires the second answer to lead back to the same
/// actor, because a `WebFinger` host could otherwise claim an account on any
/// other host and get the requester to talk to (and trust) it. This client
/// enforces the containment the ticket fixes — subject domain OR actor host
/// equals the requested domain, mirroring the check Pleroma's
/// `WebFinger.validate_webfinger/2` puts first — and a later fetch ticket
/// that dereferences the actor URL with its own origin checks completes the
/// round trip. Anything outside that rule fails as
/// [`WebfingerError::DomainMismatch`].
///
/// # Errors
///
/// [`WebfingerError::InvalidAcct`] for an unusable handle (no request is
/// made), [`WebfingerError::Http`] for transport, status and body-size
/// failures of the `WebFinger` and template fetches,
/// [`WebfingerError::Malformed`] for a body that is neither valid JRD nor
/// valid XRD, [`WebfingerError::NoActorLink`] when it names no actor, and
/// [`WebfingerError::DomainMismatch`] when it does not belong to the
/// requested domain.
pub async fn resolve_acct(http: &HttpClient, acct: &str) -> Result<String, WebfingerError> {
    resolve_acct_with_base(http, acct, None).await
}

/// [`resolve_acct`] with `scheme_and_authority_override` replacing the
/// spec-fixed `https://{domain}` base of both the `WebFinger` and the
/// host-meta URL. `WebFinger` is HTTPS-only by RFC 7033, but the tests
/// serve their mocks over plain HTTP on loopback, so they go through here;
/// production resolves with [`None`] and speaks the spec.
async fn resolve_acct_with_base(
    http: &HttpClient,
    acct: &str,
    scheme_and_authority_override: Option<&str>,
) -> Result<String, WebfingerError> {
    let (nickname, domain) = split_acct(acct).ok_or(WebfingerError::InvalidAcct)?;
    let base = scheme_and_authority_override.map_or_else(|| default_base(&domain), str::to_owned);
    let resource = format!("acct:{nickname}@{domain}");
    let document = match fetch(http, &finger_url(&base, &resource), JRD_JSON).await {
        Ok(response) => parse_finger_response(&response)?,
        Err(first_error) if should_fall_back(&first_error) => {
            fall_back_to_lrdd(http, first_error, &base, &resource).await?
        }
        Err(first_error) => return Err(first_error),
    };
    let actor = document.actor_href().ok_or(WebfingerError::NoActorLink)?;
    verify_round_trip(document.subject.as_deref(), actor, &domain)?;
    Ok(actor.to_owned())
}

/// The base the spec fixes for a domain: `https://{domain}` — never a port,
/// which [`split_acct`] rules out of the domain it accepts.
fn default_base(domain: &str) -> String {
    format!("https://{domain}")
}

/// The second path of a failed first `WebFinger`: ask host-meta for an LRDD
/// template, fetch through it, and keep the original error when host-meta
/// has nothing usable.
async fn fall_back_to_lrdd(
    http: &HttpClient,
    first_error: WebfingerError,
    base: &str,
    resource: &str,
) -> Result<FingerDocument, WebfingerError> {
    let Some(template_url) = lrdd_url_from_host_meta(http, base, resource).await else {
        return Err(first_error);
    };
    let response = fetch(http, &template_url, JRD_JSON).await?;
    parse_finger_response(&response)
}

/// The first-attempt failures that justify consulting host-meta (the ticket
/// list: 404, 410, and network trouble). Every other failure — other
/// statuses, a too-large body, a blocked or unparsable target — is the
/// answer already.
const fn should_fall_back(error: &WebfingerError) -> bool {
    matches!(
        error,
        WebfingerError::Http(
            HttpError::Status(404 | 410) | HttpError::Connect(_) | HttpError::Timeout
        )
    )
}

/// The `{base}/.well-known/webfinger?resource={resource}` URL; every
/// character of `resource` is from the alphabets [`split_acct`] enforces,
/// so none of it needs percent-encoding.
fn finger_url(base: &str, resource: &str) -> String {
    format!("{base}/.well-known/webfinger?resource={resource}")
}

/// `GET`s `url` asking for `accept`, capped at [`MAX_BODY_BYTES`], with any
/// 4xx/5xx converted to [`WebfingerError::Http`].
async fn fetch(http: &HttpClient, url: &str, accept: &str) -> Result<HttpResponse, WebfingerError> {
    http.get_limited(url, &[(ACCEPT.as_str(), accept)], MAX_BODY_BYTES)
        .await?
        .error_for_status()
        .map_err(WebfingerError::from)
}

/// The URL the `{base}/.well-known/host-meta` LRDD template expands to for
/// `resource`, [`None`] when host-meta is unreachable, unparseable, or
/// advertises no usable `rel="lrdd"` template.
async fn lrdd_url_from_host_meta(http: &HttpClient, base: &str, resource: &str) -> Option<String> {
    let url = format!("{base}/.well-known/host-meta");
    let response = fetch(http, &url, XRD_XML).await.ok()?;
    let body = String::from_utf8(response.body.to_vec()).ok()?;
    let document = parse_xrd(&body).ok()?;
    lrdd_url(&document.links, resource)
}

/// The URL an `lrdd` link's `template` names for `resource`, expanding
/// either `{uri}` spelling. A template without a macro cannot carry the
/// resource, so it is not used.
fn lrdd_url(links: &[FingerLink], resource: &str) -> Option<String> {
    links.iter().find_map(|link| {
        if link.rel.as_deref() != Some(LRDD_REL) {
            return None;
        }
        let template = link.template.as_deref()?;
        let expanded = if template.contains(URI_MACRO) {
            template.replace(URI_MACRO, resource)
        } else {
            template.replace(URI_MACRO_ENCODED, resource)
        };
        if expanded == template {
            return None;
        }
        Some(expanded)
    })
}

/// The actor URL rule of a `WebFinger` answer: its `subject` (an `acct:`
/// URI) names `domain`, or the actor URL's own host equals `domain` — the
/// OR the ticket fixes, with [`resolve_acct`]'s doc comment for why (the
/// Mastodon round trip).
fn verify_round_trip(
    subject: Option<&str>,
    actor: &str,
    domain: &str,
) -> Result<(), WebfingerError> {
    if subject_names_domain(subject, domain) || actor_names_domain(actor, domain) {
        Ok(())
    } else {
        Err(WebfingerError::DomainMismatch)
    }
}

/// Whether `subject` carries an `@domain` part equal to `domain`
/// (case-insensitively); an absent subject, or one without an `@`, names
/// nothing.
fn subject_names_domain(subject: Option<&str>, domain: &str) -> bool {
    subject.is_some_and(|subject| {
        let value = strip_prefix_ignore_case(subject, ACCT_PREFIX).unwrap_or(subject);
        value
            .rsplit_once('@')
            .is_some_and(|(_name, host)| host.eq_ignore_ascii_case(domain))
    })
}

/// Whether `actor` parses as a URL whose host equals `domain`
/// (case-insensitively; URL parsing already lowercases hosts).
fn actor_names_domain(actor: &str, domain: &str) -> bool {
    Url::parse(actor).is_ok_and(|url| {
        url.host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(domain))
    })
}

/// Splits a `WebFinger` handle into its `(nickname, domain)` parts, with the
/// domain lowercased.
///
/// Accepts `nick@domain` with any leading `@` and `acct:` (in either order,
/// case-insensitively for the scheme). The nickname must be non-empty and
/// made of the handle alphabet `[A-Za-z0-9._-]` (Pleroma's `webfinger`
/// regex, `[a-z0-9A-Z_\.-]+`); the domain must be a dotted hostname of ASCII
/// letters, digits and interior hyphens whose top-level label starts with a
/// letter — no port, IP literal or single label, because the lookup builds
/// the authority straight out of it. The nickname is returned as given; the
/// domain is lowercased.
#[must_use]
pub fn split_acct(acct: &str) -> Option<(String, String)> {
    let (nickname, domain) = strip_finger_prefixes(acct).split_once('@')?;
    if nickname.is_empty() || !nickname.chars().all(is_nickname_char) {
        return None;
    }
    if !is_valid_domain(domain) {
        return None;
    }
    Some((nickname.to_owned(), domain.to_ascii_lowercase()))
}

/// `acct` without its leading `@` and `acct:` prefixes (each optional, in
/// either order).
fn strip_finger_prefixes(acct: &str) -> &str {
    let without_at = acct.strip_prefix('@').unwrap_or(acct);
    let without_scheme = strip_prefix_ignore_case(without_at, ACCT_PREFIX).unwrap_or(without_at);
    without_scheme.strip_prefix('@').unwrap_or(without_scheme)
}

/// `value` with a case-insensitive ASCII `prefix` removed, `None` when
/// `value` does not start with it.
fn strip_prefix_ignore_case<'value>(value: &'value str, prefix: &str) -> Option<&'value str> {
    let mut remaining = value;
    for expected in prefix.chars() {
        match remaining.chars().next() {
            Some(found) if found.eq_ignore_ascii_case(&expected) => {
                remaining = remaining.split_at(found.len_utf8()).1;
            }
            _ => return None,
        }
    }
    Some(remaining)
}

/// One character of the handle alphabet [`split_acct`] accepts in the
/// nickname: ASCII alphanumerics plus `.`, `_` and `-`.
const fn is_nickname_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Whether `domain` is a hostname [`split_acct`] accepts: dot-separated
/// labels, at least two of them, each 1..=63 characters of ASCII letters,
/// digits and interior hyphens, with the top-level label starting with a
/// letter (so IPv4 literals and port numbers are out).
fn is_valid_domain(domain: &str) -> bool {
    domain.contains('.')
        && domain.split('.').all(is_valid_label)
        && domain
            .split('.')
            .next_back()
            .is_some_and(|tld| tld.starts_with(|c: char| c.is_ascii_alphabetic()))
}

/// One DNS label as [`is_valid_domain`] requires it.
fn is_valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

/// One `Link` element or object of a `WebFinger` document: JRD objects
/// deserialize straight into it, and the XRD reader fills the same shape
/// from the element's attributes (`template` matters for host-meta, `rel`,
/// `type` and `href` for the actor).
#[derive(Debug, Default, Deserialize)]
struct FingerLink {
    /// The relation this link states.
    rel: Option<String>,
    /// The media type of the link target, the JSON key `type`.
    #[serde(rename = "type")]
    link_type: Option<String>,
    /// The target URL.
    href: Option<String>,
    /// The templated URL (LRDD links in host-meta).
    template: Option<String>,
}

/// The parts of a `WebFinger` answer this client consumes, read from either
/// representation.
#[derive(Debug)]
struct FingerDocument {
    /// The `subject` / `<Subject>` value, when present.
    subject: Option<String>,
    /// Every advertised link, in document order.
    links: Vec<FingerLink>,
}

impl FingerDocument {
    /// The actor URL: the `href` of the first `rel=self` link whose `type`
    /// names an `ActivityPub` media type (see [`is_activity_media_type`])
    /// and whose `href` is an absolute `http(s)` URL. Links of any other
    /// shape — including the profile-page link — are not actors.
    fn actor_href(&self) -> Option<&str> {
        self.links.iter().find_map(FingerLink::actor_href)
    }
}

impl FingerLink {
    /// This link's actor URL, [`None`] unless it is a `rel=self` link of an
    /// `ActivityPub` media type carrying a usable URL (all values trimmed).
    fn actor_href(&self) -> Option<&str> {
        if self.rel.as_deref().map(str::trim) != Some(SELF_REL) {
            return None;
        }
        if !self
            .link_type
            .as_deref()
            .is_some_and(is_activity_media_type)
        {
            return None;
        }
        self.href
            .as_deref()
            .map(str::trim)
            .filter(|href| is_http_url(href))
    }
}

/// Whether a link's declared `type` names an `ActivityPub` JSON document,
/// ignoring parameters and whitespace: `application/activity+json` or
/// `application/ld+json` with any (or no) parameters after the `;`.
fn is_activity_media_type(candidate: &str) -> bool {
    let media_type = candidate
        .split_once(';')
        .map_or(candidate, |(media_type, _parameters)| media_type)
        .trim()
        .to_ascii_lowercase();
    media_type == ACTIVITY_JSON || media_type == LD_JSON
}

/// Whether `text` parses as an absolute URL whose scheme is `http` or
/// `https` and that has a host.
fn is_http_url(text: &str) -> bool {
    Url::parse(text)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
}

/// Reads a `WebFinger` answer, choosing the JRD or XRD reader from the
/// response's content type and — when that is absent or names neither
/// family — from the document's first character.
fn parse_finger_response(response: &HttpResponse) -> Result<FingerDocument, WebfingerError> {
    let body = String::from_utf8(response.body.to_vec()).map_err(note_malformed)?;
    if answered_json(&response.headers, &response.body) {
        parse_jrd(&body)
    } else {
        parse_xrd(&body)
    }
}

/// Whether the answer is to be read as JRD JSON: Pleroma's `do_finger`
/// dispatch, media types `application/jrd+json` or `application/json` read
/// as JSON and `application/xrd+xml`, `application/xml` or `text/xml` as
/// XML, and everything else (including no content type) sniffed from the
/// first non-whitespace character — `{` or `[` open JSON.
fn answered_json(headers: &HeaderMap, body: &[u8]) -> bool {
    match declared_media_type(headers).as_deref() {
        Some(JRD_JSON | "application/json") => true,
        Some(XRD_XML | "application/xml" | "text/xml") => false,
        _ => looks_like_json(body),
    }
}

/// The media type a response declares, parameters stripped and lowercased;
/// [`None`] when it declares none or names bytes that are not text.
fn declared_media_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split_once(';')
                .map_or(value, |(media_type, _parameters)| media_type)
                .trim()
                .to_ascii_lowercase()
        })
}

/// Whether `body`'s first non-whitespace byte opens a JSON value.
fn looks_like_json(body: &[u8]) -> bool {
    let opener = body
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace());
    matches!(opener, Some(b'{' | b'['))
}

/// The JSON shape of a JRD document: only `subject` and `links` are read;
/// everything else (`aliases`, `properties`) is ignored.
#[derive(Debug, Deserialize)]
struct Jrd {
    /// The account this document answers about, `acct:nick@domain`.
    subject: Option<String>,
    /// The advertised links; absent or null reads as none.
    links: Option<Vec<FingerLink>>,
}

/// Reads a JRD document.
fn parse_jrd(body: &str) -> Result<FingerDocument, WebfingerError> {
    let document: Jrd = serde_json::from_str(body).map_err(note_malformed)?;
    Ok(FingerDocument {
        subject: document.subject,
        links: document.links.unwrap_or_default(),
    })
}

/// Reads an XRD document (a `WebFinger` answer or host-meta): the
/// `<Subject>` text and every `<Link>` element's attributes, with entity
/// references resolved. Everything else in the vocabulary — `Alias`
/// elements, the subscribe link's template beyond its attribute — is
/// skipped.
fn parse_xrd(body: &str) -> Result<FingerDocument, WebfingerError> {
    let mut reader = Reader::from_str(body);
    let mut links = Vec::new();
    let mut subject: Option<String> = None;
    let mut subject_text = String::new();
    let mut in_subject = false;
    loop {
        match reader.read_event().map_err(note_malformed)? {
            Event::Start(start) if start.local_name().as_ref() == "Subject" => {
                in_subject = true;
            }
            Event::Text(text) if in_subject => subject_text.push_str(&text.xml10_content()),
            Event::GeneralRef(reference) if in_subject => {
                // The parser splits entity references out of the text;
                // rebuild `&name;` here and resolve the whole run at the end.
                subject_text.push('&');
                subject_text.push_str(reference.into_inner().as_ref());
                subject_text.push(';');
            }
            Event::End(end) if in_subject && end.local_name().as_ref() == "Subject" => {
                in_subject = false;
                subject = Some(
                    unescape(&subject_text)
                        .map_err(note_malformed)?
                        .trim()
                        .to_owned(),
                );
                subject_text.clear();
            }
            Event::Start(start) | Event::Empty(start) if start.local_name().as_ref() == "Link" => {
                links.push(parse_xrd_link(&start)?);
            }
            Event::Eof => break,
            other @ (Event::Start(_)
            | Event::End(_)
            | Event::Empty(_)
            | Event::Text(_)
            | Event::CData(_)
            | Event::Comment(_)
            | Event::Decl(_)
            | Event::PI(_)
            | Event::DocType(_)
            | Event::GeneralRef(_)) => drop(other),
        }
    }
    Ok(FingerDocument { subject, links })
}

/// One `<Link>` element as a [`FingerLink`]: the `rel`, `type`, `href` and
/// `template` attributes, entity-unescaped and trimmed; other attributes are
/// skipped.
fn parse_xrd_link(start: &BytesStart<'_>) -> Result<FingerLink, WebfingerError> {
    let mut link = FingerLink::default();
    for attribute in start.attributes() {
        let attribute = attribute.map_err(note_malformed)?;
        let value = attribute
            .normalized_value(XRD_XML_VERSION)
            .map_err(note_malformed)?
            .trim()
            .to_owned();
        match attribute.key.local_name().as_ref() {
            "rel" => link.rel = Some(value),
            "type" => link.link_type = Some(value),
            "href" => link.href = Some(value),
            "template" => link.template = Some(value),
            _other => {}
        }
    }
    Ok(link)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode, header},
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use serde_json::json;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;

    /// `User-Agent` for the test clients.
    const TEST_UA: &str = "Wired (https://test.example)";
    /// A `WebFinger` handle whose domain is [`DOMAIN`].
    const ACCT: &str = "alice@remote.example";
    /// The domain every mock advertises as its own.
    const DOMAIN: &str = "remote.example";
    /// The actor URL every correct mock links to for [`ACCT`].
    const ACTOR: &str = "https://remote.example/users/alice";
    /// The namespace a real XRD document declares.
    const XRD_NAMESPACE: &str = "http://docs.oasis-open.org/ns/xri/xrd-1.0";
    /// A good JRD answer about [`ACCT`]: the profile-page link first (which
    /// the reader must skip), the self link second.
    const JRD_ALICE: &str = r#"{
        "subject": "acct:alice@remote.example",
        "aliases": ["https://remote.example/users/alice"],
        "links": [
            {"rel": "http://webfinger.net/rel/profile-page",
             "type": "text/html",
             "href": "https://remote.example/@alice"},
            {"rel": "self",
             "type": "application/activity+json",
             "href": "https://remote.example/users/alice"},
            {"rel": "http://ostatus.org/schema/1.0/subscribe",
             "template": "https://remote.example/ostatus_subscribe?acct={uri}"}
        ]
    }"#;
    /// The same document as XRD.
    const XRD_ALICE: &str = r#"<XRD xmlns="http://docs.oasis-open.org/ns/xri/xrd-1.0">
        <Subject>acct:alice@remote.example</Subject>
        <Link rel="http://webfinger.net/rel/profile-page" type="text/html"
              href="https://remote.example/@alice"/>
        <Link rel="self" type="application/activity+json"
              href="https://remote.example/users/alice"/>
    </XRD>"#;
    /// A JRD answer whose every self link names the wrong media type.
    const JRD_WRONG_TYPES: &str = r#"{"subject": "acct:alice@remote.example", "links": [
        {"rel": "self", "type": "text/html", "href": "https://remote.example/users/alice"},
        {"rel": "self", "type": "application/json", "href": "https://remote.example/wrong"},
        {"rel": "http://webfinger.net/rel/profile-page", "type": "text/html",
         "href": "https://remote.example/@alice"}
    ]}"#;
    /// A JRD answer with only the profile-page link: no actor at all.
    const JRD_PROFILE_ONLY: &str = r#"{"subject": "acct:alice@remote.example", "links": [
        {"rel": "http://webfinger.net/rel/profile-page", "type": "text/html",
         "href": "https://remote.example/@alice"}
    ]}"#;
    /// A JRD answer for another domain: wrong subject domain, wrong actor
    /// host — nothing here belongs to [`DOMAIN`].
    const JRD_EVIL: &str = r#"{"subject": "acct:alice@evil.example", "links": [
        {"rel": "self", "type": "application/activity+json",
         "href": "https://evil.example/users/alice"}
    ]}"#;
    /// A JRD answer whose subject belongs elsewhere but whose actor URL is
    /// on [`DOMAIN`]: the OR arm of the containment rule passes it.
    const JRD_FOREIGN_SUBJECT_LOCAL_ACTOR: &str = r#"{"subject": "acct:alice@other.example", "links": [
        {"rel": "self", "type": "application/activity+json",
         "href": "https://remote.example/users/alice"}
    ]}"#;
    /// A JRD answer whose subject is on [`DOMAIN`] but whose actor URL is on
    /// the webfinger domain: allowed by the OR rule, and what a correct
    /// answer must return — the foreign href.
    const JRD_LOCAL_SUBJECT_FOREIGN_ACTOR: &str = r#"{"subject": "acct:alice@remote.example", "links": [
        {"rel": "self", "type": "application/activity+json",
         "href": "https://webfinger.example/users/alice"}
    ]}"#;
    /// A JRD answer with no subject at all and an actor URL on another
    /// domain: with no subject to vouch for the answer, containment fails.
    const JRD_NO_SUBJECT_FOREIGN_ACTOR: &str = r#"{"links": [
        {"rel": "self", "type": "application/activity+json",
         "href": "https://evil.example/users/alice"}
    ]}"#;

    /// The client under test: the SSRF guard is off, so it may reach the
    /// loopback mocks (the mode `HTTP_ALLOW_PRIVATE_HOSTS` selects).
    fn client() -> HttpClient {
        HttpClient::new_for_tests(TEST_UA)
    }

    /// The resolved-actor assertion shared by the wire tests.
    fn assert_actor(result: Result<String, WebfingerError>, expect: &str) {
        match result {
            Ok(actor) => assert_eq!(actor, expect, "the self link's href"),
            Err(err) => panic!("resolution failed: {err}"),
        }
    }

    /// A router answering `path` with `body`, labelled `content_type` when
    /// one is given.
    fn static_route(
        path: &'static str,
        content_type: Option<&'static str>,
        body: impl Into<String>,
    ) -> Router {
        /// Answer with the configured content type, or `text/plain` (what
        /// an axum `String` response carries) when the fixture declares none.
        async fn handler(State((content_type, body)): State<(Option<String>, String)>) -> Response {
            let declared = content_type.unwrap_or_else(|| "text/plain; charset=utf-8".to_owned());
            ([(CONTENT_TYPE, declared)], body).into_response()
        }
        Router::new()
            .route(path, get(handler))
            .with_state((content_type.map(str::to_owned), body.into()))
    }

    /// A router answering `path` with a fixed status.
    fn status_route(path: &'static str, status: StatusCode) -> Router {
        /// Reply with the configured status.
        async fn handler(State(status): State<StatusCode>) -> StatusCode {
            status
        }
        Router::new().route(path, get(handler)).with_state(status)
    }

    /// A host-meta route advertising an LRDD template that points back at
    /// this server's `/lrdd`, using the percent-encoded macro on request.
    fn host_meta_route(encoded_macro: bool) -> Router {
        /// Advertise `<own authority>/lrdd?uri=<configured macro>` as the
        /// LRDD template, the host taken from the request so the template
        /// points back at this very mock.
        async fn handler(
            headers: HeaderMap,
            State(macro_text): State<String>,
        ) -> Result<Response, StatusCode> {
            let host = headers
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                .ok_or(StatusCode::BAD_REQUEST)?;
            let body = format!(
                r#"<XRD xmlns="{XRD_NAMESPACE}"><Link rel="lrdd" template="http://{host}/lrdd?uri={macro_text}"/></XRD>"#
            );
            Ok(([(CONTENT_TYPE, XRD_XML)], body).into_response())
        }
        let macro_text = if encoded_macro {
            URI_MACRO_ENCODED
        } else {
            URI_MACRO
        };
        Router::new()
            .route("/.well-known/host-meta", get(handler))
            .with_state(macro_text.to_owned())
    }

    /// Serves `app` on a loopback port; returns its task handle (to abort
    /// when the test is done) and the `http://` base URL to hand
    /// [`resolve_acct_with_base`] as the scheme-and-authority override.
    async fn serve(app: Router) -> (JoinHandle<()>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test server stopped: {err}");
            }
        });
        (server, base)
    }

    /// Resolves `acct` against `app` served on loopback, through the
    /// scheme-and-authority override.
    async fn resolve_against(app: Router, acct: &str) -> Result<String, WebfingerError> {
        let (server, base) = serve(app).await;
        let result = resolve_acct_with_base(&client(), acct, Some(&base)).await;
        server.abort();
        result
    }

    /// A router counting the requests it answers and then failing, to prove
    /// a code path never reached it.
    fn counting_failure_route(path: &'static str, hits: &Arc<AtomicUsize>) -> Router {
        /// Record the hit and fail, so the count proves whether the client
        /// asked this path at all.
        async fn handler(State(counter): State<Arc<AtomicUsize>>) -> Response {
            let _previous = counter.fetch_add(1, Ordering::SeqCst);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Router::new()
            .route(path, get(handler))
            .with_state(Arc::clone(hits))
    }

    #[test]
    fn split_acct_parses_valid_handles() {
        for (input, nickname, domain) in [
            ("alice@remote.example", "alice", "remote.example"),
            ("@alice@remote.example", "alice", "remote.example"),
            ("acct:alice@remote.example", "alice", "remote.example"),
            ("@acct:alice@remote.example", "alice", "remote.example"),
            ("acct:@alice@remote.example", "alice", "remote.example"),
            ("ACCT:alice@remote.example", "alice", "remote.example"),
            ("Alice.U-2@a1.example.io", "Alice.U-2", "a1.example.io"),
            ("alice@Remote.Example", "alice", "remote.example"),
        ] {
            assert_eq!(
                split_acct(input),
                Some((nickname.to_owned(), domain.to_owned())),
                "{input} is a valid handle"
            );
        }
    }

    #[test]
    fn split_acct_rejects_invalid_handles() {
        for input in [
            "",
            "@",
            "acct:",
            "alice",
            "alice@",
            "@remote.example",
            "acct:alice",
            "alice@remote",
            "alice@remote.example:8080",
            "alice@127.0.0.1",
            "alice@-bad.example",
            "alice@bad-.example",
            "alice@exam_ple.example",
            "ali ce@remote.example",
            "ali$ce@remote.example",
            "ali(ce@remote.example",
            "müller@remote.example",
            "alice@@remote.example",
            "alice@remote..example",
            "alice@.example",
            "alice@remote.",
            "alice@remote.1",
        ] {
            assert_eq!(split_acct(input), None, "{input} must be rejected");
        }
    }

    /// Serve [`JRD_ALICE`] only when the request asked for exactly
    /// `application/jrd+json`, and 400 otherwise — so a success also proves
    /// the client's `Accept` header and the request path.
    async fn jrd_checking_accept(headers: HeaderMap) -> Response {
        let asked = headers.get(ACCEPT).and_then(|value| value.to_str().ok()) == Some(JRD_JSON);
        if !asked {
            return StatusCode::BAD_REQUEST.into_response();
        }
        ([(CONTENT_TYPE, JRD_JSON)], JRD_ALICE).into_response()
    }

    #[tokio::test]
    async fn resolves_a_jrd_answer() {
        let app = Router::new().route("/.well-known/webfinger", get(jrd_checking_accept));
        let result = resolve_against(app, "@acct:alice@remote.example").await;
        assert_actor(result, ACTOR);
    }

    #[tokio::test]
    async fn resolves_an_xrd_answer() {
        let app = static_route("/.well-known/webfinger", Some(XRD_XML), XRD_ALICE);
        assert_actor(resolve_against(app, ACCT).await, ACTOR);
    }

    #[tokio::test]
    async fn a_self_link_of_the_wrong_type_is_ignored() {
        let app = static_route("/.well-known/webfinger", Some(JRD_JSON), JRD_WRONG_TYPES);
        let error = resolve_against(app, ACCT).await.unwrap_err();
        assert!(
            matches!(error, WebfingerError::NoActorLink),
            "no self link had an ActivityPub type: {error:?}"
        );
    }

    #[tokio::test]
    async fn ld_json_with_profile_parameters_and_spaces_matches() {
        let body = json!({
            "subject": "acct:alice@remote.example",
            "links": [{
                "rel": "self",
                "type": " application/ld+json ; profile = \"https://www.w3.org/ns/activitystreams\" ",
                "href": ACTOR,
            }],
        })
        .to_string();
        let app = static_route("/.well-known/webfinger", Some(JRD_JSON), body);
        assert_actor(resolve_against(app, ACCT).await, ACTOR);
    }

    #[tokio::test]
    async fn a_document_with_only_the_profile_page_link_has_no_actor() {
        let app = static_route("/.well-known/webfinger", Some(JRD_JSON), JRD_PROFILE_ONLY);
        let error = resolve_against(app, ACCT).await.unwrap_err();
        assert!(
            matches!(error, WebfingerError::NoActorLink),
            "the profile-page link is not an actor: {error:?}"
        );
    }

    #[tokio::test]
    async fn domain_containment_accepts_either_subject_or_actor_host() {
        for (body, expect) in [
            (JRD_FOREIGN_SUBJECT_LOCAL_ACTOR, ACTOR),
            (
                JRD_LOCAL_SUBJECT_FOREIGN_ACTOR,
                "https://webfinger.example/users/alice",
            ),
        ] {
            let app = static_route("/.well-known/webfinger", Some(JRD_JSON), body);
            assert_actor(resolve_against(app, ACCT).await, expect);
        }
    }

    #[tokio::test]
    async fn domain_containment_rejects_answers_naming_nothing_local() {
        for body in [JRD_EVIL, JRD_NO_SUBJECT_FOREIGN_ACTOR] {
            let app = static_route("/.well-known/webfinger", Some(JRD_JSON), body);
            let error = resolve_against(app, ACCT).await.unwrap_err();
            assert!(
                matches!(error, WebfingerError::DomainMismatch),
                "neither subject nor actor host is {DOMAIN}: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn falls_back_to_the_host_meta_lrdd_template() {
        let app = status_route("/.well-known/webfinger", StatusCode::NOT_FOUND)
            .merge(host_meta_route(false))
            .merge(static_route("/lrdd", Some(JRD_JSON), JRD_ALICE));
        assert_actor(resolve_against(app, ACCT).await, ACTOR);
    }

    #[tokio::test]
    async fn the_percent_encoded_uri_macro_is_expanded_too() {
        let app = status_route("/.well-known/webfinger", StatusCode::GONE)
            .merge(host_meta_route(true))
            .merge(static_route("/lrdd", Some(JRD_JSON), JRD_ALICE));
        assert_actor(resolve_against(app, ACCT).await, ACTOR);
    }

    #[tokio::test]
    async fn a_successful_first_attempt_never_consults_host_meta() {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = static_route("/.well-known/webfinger", Some(JRD_JSON), JRD_ALICE)
            .merge(counting_failure_route("/.well-known/host-meta", &hits));
        assert_actor(resolve_against(app, ACCT).await, ACTOR);
        assert_eq!(hits.load(Ordering::SeqCst), 0, "host-meta was never asked");
    }

    #[tokio::test]
    async fn a_non_fallback_status_propagates_without_consulting_host_meta() {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = status_route("/.well-known/webfinger", StatusCode::INTERNAL_SERVER_ERROR)
            .merge(counting_failure_route("/.well-known/host-meta", &hits));
        let error = resolve_against(app, ACCT).await.unwrap_err();
        assert!(
            matches!(error, WebfingerError::Http(HttpError::Status(500))),
            "a 500 is the answer, not a prompt for host-meta: {error:?}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0, "host-meta was never asked");
    }

    #[tokio::test]
    async fn network_failure_reports_the_first_attempt_error_when_host_meta_fails() {
        // A port nothing listens on: both the WebFinger attempt and the
        // host-meta fallback fail to connect, and the first error survives.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let error = resolve_acct_with_base(&client(), ACCT, Some(&dead_base))
            .await
            .unwrap_err();
        assert!(
            matches!(error, WebfingerError::Http(HttpError::Connect(_))),
            "a refused connection is reported: {error:?}"
        );
    }

    #[tokio::test]
    async fn an_unusable_host_meta_keeps_the_original_error() {
        let app =
            status_route("/.well-known/webfinger", StatusCode::NOT_FOUND).merge(static_route(
                "/.well-known/host-meta",
                Some(XRD_XML),
                format!(r#"<XRD xmlns="{XRD_NAMESPACE}"><Link rel="self" href="{ACTOR}"/></XRD>"#),
            ));
        let error = resolve_against(app, ACCT).await.unwrap_err();
        assert!(
            matches!(error, WebfingerError::Http(HttpError::Status(404))),
            "host-meta without an lrdd template: the 404 stands: {error:?}"
        );
    }

    #[tokio::test]
    async fn the_templated_attempt_reports_its_own_failures() {
        // The templated fetch answers 410: that status, not the original
        // 404, is what the caller sees — proof the template URL was fetched.
        let app = status_route("/.well-known/webfinger", StatusCode::NOT_FOUND)
            .merge(host_meta_route(false))
            .merge(status_route("/lrdd", StatusCode::GONE));
        let error = resolve_against(app, ACCT).await.unwrap_err();
        assert!(
            matches!(error, WebfingerError::Http(HttpError::Status(410))),
            "the template fetch failed: {error:?}"
        );
    }

    #[tokio::test]
    async fn invalid_handles_fail_before_any_request() {
        for acct in ["alice", "@", "alice@localhost", "alice@remote.example:8080"] {
            let error = resolve_acct_with_base(&client(), acct, None)
                .await
                .unwrap_err();
            assert!(
                matches!(error, WebfingerError::InvalidAcct),
                "{acct} is no handle, and no DNS is tried: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn unparseable_bodies_are_malformed() {
        for (content_type, body) in [
            (Some(JRD_JSON), "{\"subject\": 5, \"links\": []}"),
            (Some(JRD_JSON), "[]"),
            (Some(JRD_JSON), "{oops"),
            (Some(XRD_XML), "<XRD><a></b></XRD>"),
            (Some(XRD_XML), "<XRD><Link rel=\"self\""),
            (None, "{ not json ]"),
        ] {
            let app = static_route("/.well-known/webfinger", content_type, body);
            let error = resolve_against(app, ACCT).await.unwrap_err();
            assert!(
                matches!(error, WebfingerError::Malformed),
                "`{body}` ({content_type:?}) is neither valid JRD nor XRD: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_format_is_sniffed_from_the_first_character() {
        let json_app = static_route("/.well-known/webfinger", None, JRD_ALICE);
        assert_actor(resolve_against(json_app, ACCT).await, ACTOR);
        let xml_app = static_route(
            "/.well-known/webfinger",
            None,
            format!(r#"<?xml version="1.0" encoding="UTF-8"?>{XRD_ALICE}"#),
        );
        assert_actor(resolve_against(xml_app, ACCT).await, ACTOR);
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_too_large() {
        let huge = format!(
            r#"{{"subject": "acct:alice@remote.example", "links": [{{"rel": "self", "type": "application/activity+json", "href": "https://remote.example/users/alice", "note": "{}"}}]}}"#,
            "x".repeat(MAX_BODY_BYTES),
        );
        let app = static_route("/.well-known/webfinger", Some(JRD_JSON), huge);
        let error = resolve_against(app, ACCT).await.unwrap_err();
        assert!(
            matches!(error, WebfingerError::Http(HttpError::TooLarge)),
            "the 256 KiB cap stops an endless body: {error:?}"
        );
    }

    #[test]
    fn xrd_selections_resolve_entities_and_skip_other_vocabulary() {
        let document = parse_xrd(
            r#"<XRD xmlns="http://docs.oasis-open.org/ns/xri/xrd-1.0">
                <Subject>acct:alice&#64;remote.example &amp; friends</Subject>
                <Alias>https://remote.example/users/alice</Alias>
                <Link rel="self" type="application/activity+json"
                      href="https://remote.example/users/alice"/>
            </XRD>"#,
        )
        .unwrap();
        assert_eq!(
            document.subject.as_deref(),
            Some("acct:alice@remote.example & friends"),
            "entity references are resolved in the subject"
        );
        assert_eq!(document.actor_href(), Some(ACTOR), "the self link wins");
    }

    #[test]
    fn jrd_selection_ignores_unusable_links() {
        let document = parse_jrd(
            r#"{"subject": "acct:alice@remote.example", "links": [
                {"rel": "self", "href": "urn:uuid:1"},
                {"rel": " self ", "type": "APPLICATION/ACTIVITY+JSON",
                 "href": " https://remote.example/users/alice "},
                {"rel": "self", "type": "application/activity+json"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            document.actor_href(),
            Some(ACTOR),
            "trimming and case folding apply, and links without a usable href are skipped"
        );
    }

    #[test]
    fn documents_without_links_name_no_actor() {
        for body in ["{}", r#"{"links": null}"#] {
            let document = parse_jrd(body).unwrap();
            assert_eq!(document.actor_href(), None, "`{body}` has no links");
        }
    }

    #[test]
    fn lrdd_needs_its_macro_and_its_rel() {
        let with_template = |rel: Option<&str>, template: Option<&str>| FingerLink {
            rel: rel.map(str::to_owned),
            template: template.map(str::to_owned),
            ..FingerLink::default()
        };
        assert_eq!(
            lrdd_url(
                &[with_template(
                    Some(LRDD_REL),
                    Some("https://x.example/wf?r={uri}")
                )],
                "acct:a@x.example",
            )
            .unwrap(),
            "https://x.example/wf?r=acct:a@x.example",
            "the macro expands"
        );
        for links in [
            vec![with_template(
                Some("other"),
                Some("https://x.example/wf?r={uri}"),
            )],
            vec![with_template(Some(LRDD_REL), Some("https://x.example/wf"))],
            vec![with_template(Some(LRDD_REL), None)],
            vec![],
        ] {
            assert_eq!(
                lrdd_url(&links, "acct:a@x.example"),
                None,
                "no usable lrdd link"
            );
        }
    }

    #[test]
    fn only_404_410_and_network_failures_fall_back() {
        for (error, fallback) in [
            (HttpError::Status(404), true),
            (HttpError::Status(410), true),
            (HttpError::Status(500), false),
            (HttpError::Status(422), false),
            (HttpError::Connect("refused".to_owned()), true),
            (HttpError::Timeout, true),
            (HttpError::TooLarge, false),
            (HttpError::Blocked("loopback".to_owned()), false),
            (HttpError::InvalidUrl, false),
        ] {
            let error = WebfingerError::Http(error);
            assert_eq!(should_fall_back(&error), fallback, "{error}");
        }
        assert!(
            !should_fall_back(&WebfingerError::Malformed),
            "only HTTP failures fall back"
        );
    }

    #[test]
    fn media_types_match_activitypub_json_only() {
        for candidate in [
            "application/activity+json",
            "APPLICATION/Activity+JSON",
            " application/activity+json ; charset=utf-8 ",
            "application/ld+json",
            r#"application/ld+json; profile="https://www.w3.org/ns/activitystreams""#,
        ] {
            assert!(is_activity_media_type(candidate), "{candidate} names AP");
        }
        for candidate in [
            "text/html",
            "application/json",
            "application/jrd+json",
            "activity+json",
            "",
        ] {
            assert!(
                !is_activity_media_type(candidate),
                "{candidate} does not name AP"
            );
        }
    }

    #[test]
    fn prefix_stripping_handles_case_and_order() {
        assert_eq!(
            strip_finger_prefixes("@acct:alice@x.example"),
            "alice@x.example"
        );
        assert_eq!(
            strip_finger_prefixes("Acct:alice@x.example"),
            "alice@x.example"
        );
        assert_eq!(
            strip_finger_prefixes("@@alice@x.example"),
            "alice@x.example",
            "each prefix position strips at most one layer"
        );
        assert_eq!(strip_finger_prefixes("alice@x.example"), "alice@x.example");
    }

    #[test]
    fn the_spec_url_is_https_with_the_acct_resource() {
        assert_eq!(
            default_base(DOMAIN),
            "https://remote.example",
            "the production base is the spec's https origin"
        );
        assert_eq!(
            finger_url(&default_base(DOMAIN), "acct:alice@remote.example"),
            "https://remote.example/.well-known/webfinger?resource=acct:alice@remote.example"
        );
    }

    #[test]
    fn webfinger_errors_render_their_causes() {
        assert_eq!(
            WebfingerError::InvalidAcct.to_string(),
            "not a valid acct:nick@domain handle",
            "InvalidAcct"
        );
        assert_eq!(
            WebfingerError::Http(HttpError::Status(404)).to_string(),
            "webfinger request failed: HTTP peer answered 404",
            "Http keeps the transport message"
        );
        assert_eq!(
            WebfingerError::NoActorLink.to_string(),
            "webfinger document has no ActivityPub self link",
            "NoActorLink"
        );
        assert_eq!(
            WebfingerError::DomainMismatch.to_string(),
            "webfinger result does not belong to the requested domain",
            "DomainMismatch"
        );
        assert_eq!(
            WebfingerError::Malformed.to_string(),
            "webfinger document is malformed",
            "Malformed"
        );
        assert!(
            Error::source(&WebfingerError::Http(HttpError::Timeout)).is_some(),
            "Http keeps the transport error as its source"
        );
        assert!(
            Error::source(&WebfingerError::Malformed).is_none(),
            "a plain Malformed has no source"
        );
    }
}
