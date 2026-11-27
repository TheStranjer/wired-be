//! The `/.well-known` discovery routes (`docs/ARCHITECTURE.md` decision 3).
//!
//! This router is mounted by [`crate::discovery`], at the top level, not under
//! `/api`: `/.well-known` is where federation and clients look accounts up,
//! and those paths are fixed by RFC 7033 and the `OStatus` profile.
//!
//! Today it holds `WebFinger` (`webfinger` below) and host-meta
//! (`host_meta` below) —
//! Pleroma's `WebFingerController.webfinger/2` and `host_meta/2`
//! (`lib/pleroma/web/web_finger/web_finger_controller.ex`) rendering
//! `Pleroma.Web.WebFinger` (`lib/pleroma/web/web_finger.ex`). `NodeInfo`
//! got its own ticket, module ([`crate::api::nodeinfo`]) and mount point,
//! merged beside this one.
//!
//! `WebFinger` answers the same document in two representations: the JRD
//! JSON body by default, `application/jrd+json`, and the XRD XML body —
//! the same data through [`escape_html`] —
//! when the client asks with an `Accept` naming `application/xrd+xml` or a
//! `format=xml` query parameter, Pleroma's `SetFormatPlug` behavior.

use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::{
    config::Config,
    error::{AppError, AppResult},
    federation::vocab::ACTIVITY_JSON,
    models::user::{User, get_by_ap_id, get_by_nickname},
    protocols::registry,
    state::AppState,
    util::sanitize::escape_html,
};

/// `Content-Type` of the JRD JSON answer, the media type RFC 7033
/// registers for `WebFinger` responses; spelled without a charset because
/// that is what the ticket prescribes and what Pleroma sends.
const JRD_JSON: &str = "application/jrd+json";
/// `Content-Type` of the XRD XML answer, both here and in host-meta
/// (Pleroma's `put_resp_content_type("application/xrd+xml", …)`).
const XRD_XML: &str = "application/xrd+xml";
/// The namespace of an XRD 1.0 document (OASIS `xri/xrd-1.0`), the one
/// attribute Pleroma's `WebFinger.host_meta/0` and `represent_user(user,
/// "XML")` put on the root element.
const XRD_NAMESPACE: &str = "http://docs.oasis-open.org/ns/xri/xrd-1.0";
/// `rel` of the human-facing profile link (the `WebFinger` core relation).
const PROFILE_PAGE_REL: &str = "http://webfinger.net/rel/profile-page";
/// `rel` of the `ActivityPub` actor link (Mastodon and Pleroma both
/// advertise the actor id under the plain `self`).
const SELF_REL: &str = "self";
/// `rel` of the host-meta link advertising the `WebFinger` template (the
/// link-based resource descriptor discovery of RFC 6415).
const LRDD_REL: &str = "lrdd";
/// `rel` of the `OStatus` follow-intent link, the template remote servers
/// redirect users to in order to follow this account.
const SUBSCRIBE_REL: &str = "http://ostatus.org/schema/1.0/subscribe";
/// `type` of the profile link: an HTML page (which, for a remote fetcher,
/// is where the `ActivityPub` document gets negotiated from).
const TEXT_HTML: &str = "text/html";
/// The 400 body message when the `resource` query parameter is absent.
const MISSING_RESOURCE: &str = "Missing resource";

/// The path of the `WebFinger` document.
pub const WEBFINGER_PATH: &str = "/.well-known/webfinger";
/// The path of the host-meta document.
pub const HOST_META_PATH: &str = "/.well-known/host-meta";

/// The `WebFinger` and host-meta routes, mounted by [`crate::discovery::router`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route(WEBFINGER_PATH, get(webfinger))
        .route(HOST_META_PATH, get(host_meta))
}

/// The query parameters of `/.well-known/webfinger`. An absent `resource`
/// is the 400 and an unknown one is a 404, neither of which the type can
/// express, so both fields are optional here.
#[derive(Debug, Deserialize)]
struct WebFingerQuery {
    /// The account to look up: `acct:nick@domain`, `nick@domain` or an
    /// actor URL (see [`parse_resource`]).
    resource: Option<String>,
    /// Pleroma's `SetFormatPlug` format override: `xml` asks for the XRD
    /// XML representation whatever the `Accept` header says.
    format: Option<String>,
}

/// `GET /.well-known/webfinger?resource=…` — the `WebFinger` answer about a
/// local account. No authentication: this is how other instances and
/// clients find actors.
///
/// # Errors
///
/// [`AppError::BadRequest`] when `resource` is absent; [`AppError::NotFound`]
/// when it names no local account (unknown, wrong domain or remote
/// mirror); [`AppError::Gone`] when the account's deletion has started. A
/// deactivated or suspended account is served like an active one. Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn webfinger(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WebFingerQuery>,
) -> AppResult<Response> {
    let Some(resource) = query.resource.as_deref() else {
        return Err(AppError::BadRequest(MISSING_RESOURCE.to_owned()));
    };
    let Some(user) = find_local_resolvable_user(&state, resource).await? else {
        return Err(AppError::NotFound);
    };
    let finger = Finger::for_user(&user, &state.config);
    if wants_xml(&headers, query.format.as_deref()) {
        Ok(xrd_response(&finger))
    } else {
        jrd_response(&finger)
    }
}

/// `GET /.well-known/host-meta` — the XRD descriptor of this host: the
/// root element, then the links each enabled protocol adds through
/// [`ProtocolAdapter::host_meta_links`](crate::protocols::adapter::ProtocolAdapter::host_meta_links),
/// in registry order. No parameters, no authentication, no failure: the
/// document only names this instance's own URLs.
async fn host_meta(State(state): State<AppState>) -> Response {
    let links: Vec<String> = registry::enabled(&state.config)
        .flat_map(|adapter| adapter.host_meta_links(&state.config))
        .map(|link| xrd_link(&link))
        .collect();
    xrd_body(format!(
        r#"<XRD xmlns="{XRD_NAMESPACE}">{}</XRD>"#,
        links.concat()
    ))
}

/// How a `WebFinger` `resource` names the account it asks about, the two
/// branches Pleroma's controller walks: its `acct` regex first, the
/// verbatim actor-id lookup as fallback.
#[derive(Debug, PartialEq, Eq)]
enum Resource<'a> {
    /// The `acct:nickname@domain` (or bare `nickname@domain`) form whose
    /// domain is either of this instance's (the handle domain or the web
    /// host), case-insensitively: the part before the `@` is a local
    /// nickname.
    Nickname(&'a str),
    /// Anything else — a full actor URL, or an `acct` on some other
    /// domain — tried verbatim as an `ActivityPub` actor id.
    ActorId(&'a str),
}

/// Classifies `resource` against this instance's domains. An `acct` form
/// on a domain that is neither the handle domain nor the web host is
/// deliberately not a [`Resource::Nickname`]: it falls through to the
/// actor-id lookup, where no row matches it, which is how Pleroma answers a
/// foreign `acct:` on this host (404).
fn parse_resource<'a>(resource: &'a str, config: &Config) -> Resource<'a> {
    let account = resource.strip_prefix("acct:").unwrap_or(resource);
    let Some((nickname, host)) = account.split_once('@') else {
        return Resource::ActorId(resource);
    };
    if !nickname.is_empty() && config.is_local_domain(host) {
        Resource::Nickname(nickname)
    } else {
        Resource::ActorId(resource)
    }
}

/// The user `resource` names, `None` when that is no local account: the
/// lookup follows [`parse_resource`], and remote mirrors are filtered out.
///
/// A deactivated or suspended account still resolves, as it does on the
/// actor route (whose document says `suspended`); only an account whose
/// deletion started is [`AppError::Gone`].
///
/// # Errors
///
/// [`AppError::Gone`] for an account whose deletion has started; any
/// database failure arrives as [`AppError::Database`].
async fn find_local_resolvable_user(state: &AppState, resource: &str) -> AppResult<Option<User>> {
    let user = match parse_resource(resource, &state.config) {
        Resource::Nickname(nickname) => get_by_nickname(&state.db, nickname).await?,
        Resource::ActorId(ap_id) => get_by_ap_id(&state.db, ap_id).await?,
    };
    let Some(user) = user.filter(|user| user.local.get()) else {
        return Ok(None);
    };
    if user.deleted_at.is_some() {
        return Err(AppError::Gone);
    }
    Ok(Some(user))
}

/// Whether the client asked for the XRD XML representation: a `format`
/// parameter naming XML (checked first, it is the explicit override
/// Pleroma's `SetFormatPlug` honors over the header), or an `Accept`
/// naming `application/xrd+xml` — case-insensitively, and among other
/// media types, as a header value may list.
fn wants_xml(headers: &HeaderMap, format: Option<&str>) -> bool {
    if format.is_some_and(is_xml_format) {
        return true;
    }
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.to_ascii_lowercase().contains(XRD_XML))
}

/// Whether a `format` query parameter names XML: `xml`, the ticket's and
/// Pleroma's spelling, and the media type itself, with or without the
/// leading dot, in any case.
fn is_xml_format(value: &str) -> bool {
    let folded = value.trim_start_matches('.').to_ascii_lowercase();
    folded == "xml" || folded == "xrd+xml" || folded == "application/xrd+xml"
}

/// The `WebFinger` answer about one actor, the document both
/// representations render.
///
/// Core owns the subject; the aliases and links are what each enabled
/// protocol contributes, in registry order (see [`WebfingerContribution`]).
#[derive(Debug, Serialize)]
struct Finger {
    /// `acct:nickname@domain` on this instance's handle domain, the nickname
    /// stored case.
    subject: String,
    /// Every enabled protocol's aliases, in registry order.
    aliases: Vec<String>,
    /// Every enabled protocol's links, in registry order.
    links: Vec<FingerLink>,
}

/// The aliases and links one protocol adds to an account's `WebFinger` document.
///
/// Returned by
/// [`ProtocolAdapter::webfinger`](crate::protocols::adapter::ProtocolAdapter::webfinger).
/// Core appends it to the document after the subject and after the
/// contributions of the protocols before it.
///
/// Later, an account's identities on other protocols (`matrix:u/bob:example.com`,
/// `xmpp:bob@example.com`, `at://did:...`) are listed here as alias URIs for
/// cross-protocol account linking. Nothing links accounts yet.
#[derive(Debug, Default)]
pub struct WebfingerContribution {
    /// URIs that also name the account.
    pub aliases: Vec<String>,
    /// Links the document states about the account.
    pub links: Vec<FingerLink>,
}

/// One `Link` element or object of an XRD or JRD document: a `WebFinger`
/// answer or host-meta.
///
/// The `type` attribute is omitted for the subscribe link, which is identified by
/// its `template` instead; Pleroma's map rendering drops the same key.
#[derive(Debug, Serialize)]
pub struct FingerLink {
    /// The relation URI (or `self`) this link states.
    pub rel: &'static str,
    /// The media type of the link target, renamed to `type` in JSON.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub link_type: Option<&'static str>,
    /// The target URL, for the `href` links.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    /// The templated follow URL, for the subscribe link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

impl Finger {
    /// The document describing the local user `user` on the instance
    /// `config` serves: the subject, then the contribution of each protocol
    /// `config` enables, in registry order.
    fn for_user(user: &User, config: &Config) -> Self {
        let mut finger = Self {
            subject: format!("acct:{}@{}", user.nickname, config.local_domain()),
            aliases: Vec::new(),
            links: Vec::new(),
        };
        for adapter in registry::enabled(config) {
            let contribution = adapter.webfinger(config, user);
            finger.aliases.extend(contribution.aliases);
            finger.links.extend(contribution.links);
        }
        finger
    }
}

/// What `ActivityPub` adds to the `WebFinger` document of the local user `user`.
///
/// As Pleroma's `represent_user/2` builds it: the actor id as an alias, plus
/// the profile URL when the row carries a distinct one; then the
/// profile-page, self and subscribe links, in that order.
///
/// The `profile_url` column is the human-facing URL where a row carries one,
/// and the actor id stands in for it in the profile link otherwise.
#[must_use]
pub fn activitypub_contribution(user: &User, config: &Config) -> WebfingerContribution {
    let profile_url = user.profile_url.clone();
    let mut aliases = vec![user.ap_id.clone()];
    if let Some(url) = profile_url
        .as_ref()
        .filter(|url| url.as_str() != user.ap_id)
    {
        aliases.push(url.clone());
    }
    WebfingerContribution {
        aliases,
        links: vec![
            FingerLink {
                rel: PROFILE_PAGE_REL,
                link_type: Some(TEXT_HTML),
                href: Some(profile_url.unwrap_or_else(|| user.ap_id.clone())),
                template: None,
            },
            FingerLink {
                rel: SELF_REL,
                link_type: Some(ACTIVITY_JSON),
                href: Some(user.ap_id.clone()),
                template: None,
            },
            subscribe_link(config),
        ],
    }
}

/// What `ActivityPub` adds to host-meta: the LRDD link whose template
/// resolves an account through this instance's `WebFinger` route
/// (Pleroma's `WebFinger.host_meta/0`).
#[must_use]
pub fn activitypub_host_meta_links(config: &Config) -> Vec<FingerLink> {
    vec![FingerLink {
        rel: LRDD_REL,
        link_type: None,
        href: None,
        template: Some(format!(
            "{}{WEBFINGER_PATH}?resource={{uri}}",
            config.base_url
        )),
    }]
}

/// The `OStatus` subscribe link: the follow-intent template naming the
/// frontend's `/authorize_interaction` page (Mastodon's shape), with the
/// literal `{uri}` macro left for the remote server to fill in.
fn subscribe_link(config: &Config) -> FingerLink {
    FingerLink {
        rel: SUBSCRIBE_REL,
        link_type: None,
        href: None,
        template: Some(format!(
            "{}/authorize_interaction?uri={{uri}}",
            config.frontend_base_url
        )),
    }
}

/// The [`Finger`] document as a JRD JSON response, `application/jrd+json`.
///
/// # Errors
///
/// [`AppError::Internal`] if serialization fails, which a plain struct of
/// strings never does.
fn jrd_response(finger: &Finger) -> AppResult<Response> {
    let body = serde_json::to_vec(finger)
        .map_err(|err| AppError::Internal(format!("JRD serialization failed: {err}")))?;
    Ok((
        [(header::CONTENT_TYPE, HeaderValue::from_static(JRD_JSON))],
        body,
    )
        .into_response())
}

/// The [`Finger`] document rendered as an XRD XML response.
fn xrd_response(finger: &Finger) -> Response {
    let aliases: Vec<String> = finger
        .aliases
        .iter()
        .map(|alias| format!("<Alias>{}</Alias>", escape_html(alias)))
        .collect();
    let links: Vec<String> = finger.links.iter().map(xrd_link).collect();
    xrd_body(format!(
        r#"<XRD xmlns="{XRD_NAMESPACE}"><Subject>{}</Subject>{}{}</XRD>"#,
        escape_html(&finger.subject),
        aliases.concat(),
        links.concat(),
    ))
}

/// One `<Link/>` element of an XRD document: `rel` first, then whichever of
/// `type`, `href` and `template` the link carries, every value run
/// through [`escape_html`] so a database-sourced URL cannot break out of
/// its quotes or its element.
fn xrd_link(link: &FingerLink) -> String {
    let mut attributes = vec![format!(r#"rel="{}""#, escape_html(link.rel))];
    if let Some(link_type) = link.link_type {
        attributes.push(format!(r#"type="{}""#, escape_html(link_type)));
    }
    if let Some(href) = &link.href {
        attributes.push(format!(r#"href="{}""#, escape_html(href)));
    }
    if let Some(template) = &link.template {
        attributes.push(format!(r#"template="{}""#, escape_html(template)));
    }
    format!("<Link {}/>", attributes.join(" "))
}

/// `body` answered as an `application/xrd+xml` document.
fn xrd_body(body: String) -> Response {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static(XRD_XML))],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    /// A [`HeaderMap`] holding only `Accept: <accept>` (`None` for no
    /// `Accept` at all).
    fn headers_with_accept(accept: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(accept) = accept {
            let previous = headers.insert(header::ACCEPT, HeaderValue::from_str(accept).unwrap());
            drop(previous);
        }
        headers
    }

    #[test]
    fn the_subscribe_template_names_the_frontend_authorize_interaction_page() {
        let mut config = Config::for_tests();
        config.frontend_base_url = "https://fe.example".to_owned();
        let link = subscribe_link(&config);
        assert_eq!(
            link.template.as_deref(),
            Some("https://fe.example/authorize_interaction?uri={uri}"),
            "the JRD template points at the frontend"
        );
        assert_eq!(
            xrd_link(&link),
            r#"<Link rel="http://ostatus.org/schema/1.0/subscribe" template="https://fe.example/authorize_interaction?uri={uri}"/>"#,
            "the XRD carries the same template"
        );
    }

    #[test]
    fn the_subscribe_template_defaults_onto_the_base_url() {
        let config = Config::for_tests();
        let link = subscribe_link(&config);
        let want = format!("{}/authorize_interaction?uri={{uri}}", config.base_url);
        assert_eq!(
            link.template.as_deref(),
            Some(want.as_str()),
            "with the default config the frontend is the backend"
        );
        assert!(
            !xrd_link(&link).contains("ostatus_subscribe"),
            "no WebFinger link mentions the login form"
        );
    }

    #[test]
    fn the_xrd_subscribe_template_is_xml_escaped() {
        let mut config = Config::for_tests();
        config.frontend_base_url = "https://fe.example/a&b".to_owned();
        let xml = xrd_link(&subscribe_link(&config));
        assert!(
            xml.contains(
                r#"template="https://fe.example/a&amp;b/authorize_interaction?uri={uri}""#
            ),
            "the ampersand is escaped: {xml}"
        );
    }

    #[test]
    fn acct_forms_on_the_local_domain_are_nicknames() {
        let config = Config::for_tests();
        for (resource, want) in [
            ("acct:alice@test.example", "alice"),
            ("alice@test.example", "alice"),
            ("acct:alice@TEST.example", "alice"),
        ] {
            assert_eq!(
                parse_resource(resource, &config),
                Resource::Nickname(want),
                "{resource} is a local acct form"
            );
        }
        let mut with_port = Config::for_tests();
        with_port.base_url = "http://localhost:4000".to_owned();
        with_port.local_domain = "localhost:4000".to_owned();
        assert_eq!(
            parse_resource("acct:alice@LoCaLhOsT:4000", &with_port),
            Resource::Nickname("alice"),
            "a port is part of the domain"
        );
    }

    #[test]
    fn a_split_config_accepts_the_handle_domain_and_the_web_host() {
        let mut config = Config::for_tests();
        config.local_domain = "whatever.com".to_owned();
        for resource in [
            "acct:alice@whatever.com",
            "alice@whatever.com",
            "acct:alice@WHATEVER.com",
            "acct:alice@test.example",
            "alice@TEST.example",
        ] {
            assert_eq!(
                parse_resource(resource, &config),
                Resource::Nickname("alice"),
                "{resource} names the local user"
            );
        }
        assert_eq!(
            parse_resource("acct:alice@other.example", &config),
            Resource::ActorId("acct:alice@other.example"),
            "a foreign domain is still no nickname"
        );
    }

    #[test]
    fn anything_else_falls_through_to_the_actor_id_lookup() {
        let config = Config::for_tests();
        for resource in [
            "https://test.example/users/alice",
            "acct:alice@other.example",
            "alice@other.example",
            "@test.example",
            "acct:alice",
            "",
        ] {
            assert_eq!(
                parse_resource(resource, &config),
                Resource::ActorId(resource),
                "{resource} is tried verbatim as an actor id"
            );
        }
    }

    #[test]
    fn the_xml_format_parameter_overrides_the_header() {
        for value in ["xml", "XML", ".xml", "xrd+xml", "application/xrd+xml"] {
            assert!(
                wants_xml(&HeaderMap::new(), Some(value)),
                "format={value} asks for XML"
            );
        }
        for value in ["json", "jrd", "jrd+json", ""] {
            assert!(
                !wants_xml(&HeaderMap::new(), Some(value)),
                "format={value} does not"
            );
        }
        assert!(
            !wants_xml(&HeaderMap::new(), None),
            "no parameter and no header is the JSON default"
        );
    }

    #[test]
    fn an_accept_naming_xrd_xml_asks_for_xml() {
        for accept in [
            "application/xrd+xml",
            "APPLICATION/XRD+XML",
            "text/html, application/xrd+xml;q=0.9",
        ] {
            let headers = headers_with_accept(Some(accept));
            assert!(wants_xml(&headers, None), "{accept} names xrd+xml");
        }
        for accept in ["*/*", "application/json", "text/xml"] {
            let headers = headers_with_accept(Some(accept));
            assert!(!wants_xml(&headers, None), "{accept} does not");
        }
        assert!(
            !wants_xml(&headers_with_accept(None), None),
            "a request that states nothing gets JSON"
        );
    }
}
