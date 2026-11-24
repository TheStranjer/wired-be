//! The `/.well-known` discovery routes (`docs/ARCHITECTURE.md` decision 3).
//!
//! This router is merged at the top level of [`crate::app`], not under
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
//! the same data through [`xml_escape`] —
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
    models::user::{User, get_by_ap_id, get_by_nickname},
    state::AppState,
    util::xml::xml_escape,
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
/// `rel` of the `OStatus` follow-intent link, the template remote servers
/// redirect users to in order to follow this account.
const SUBSCRIBE_REL: &str = "http://ostatus.org/schema/1.0/subscribe";
/// `type` of the profile link: an HTML page (which, for a remote fetcher,
/// is where the `ActivityPub` document gets negotiated from).
const TEXT_HTML: &str = "text/html";
/// `type` of the self link: the actor's `ActivityPub` document.
const ACTIVITY_JSON: &str = "application/activity+json";
/// The 400 body message when the `resource` query parameter is absent.
const MISSING_RESOURCE: &str = "Missing resource";

/// The top-level `/.well-known` router, merged by [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/.well-known/webfinger", get(webfinger))
        .route("/.well-known/host-meta", get(host_meta))
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
/// when it names no live local account (unknown, wrong domain, remote
/// mirror or deactivated user). Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn webfinger(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WebFingerQuery>,
) -> AppResult<Response> {
    let Some(resource) = query.resource.as_deref() else {
        return Err(AppError::BadRequest(MISSING_RESOURCE.to_owned()));
    };
    let Some(user) = find_local_active_user(&state, resource).await? else {
        return Err(AppError::NotFound);
    };
    let finger = Finger::for_user(&user, &state.config);
    if wants_xml(&headers, query.format.as_deref()) {
        Ok(xrd_response(&finger))
    } else {
        jrd_response(&finger)
    }
}

/// `GET /.well-known/host-meta` — the XRD descriptor telling clients that
/// this host resolves accounts through `/.well-known/webfinger`
/// (Pleroma's `WebFinger.host_meta/0`). No parameters, no authentication,
/// no failure: the document only names this instance's own URLs.
async fn host_meta(State(state): State<AppState>) -> Response {
    let template = format!(
        "{}/.well-known/webfinger?resource={{uri}}",
        state.config.base_url
    );
    xrd_body(format!(
        r#"<XRD xmlns="{XRD_NAMESPACE}"><Link rel="lrdd" template="{}"/></XRD>"#,
        xml_escape(&template)
    ))
}

/// How a `WebFinger` `resource` names the account it asks about, the two
/// branches Pleroma's controller walks: its `acct` regex first, the
/// verbatim actor-id lookup as fallback.
#[derive(Debug, PartialEq, Eq)]
enum Resource<'a> {
    /// The `acct:nickname@domain` (or bare `nickname@domain`) form whose
    /// domain equals this instance's, case-insensitively: the part before
    /// the `@` is a local nickname.
    Nickname(&'a str),
    /// Anything else — a full actor URL, or an `acct` on some other
    /// domain — tried verbatim as an `ActivityPub` actor id.
    ActorId(&'a str),
}

/// Classifies `resource` against this instance's `domain`. An `acct` form
/// on the wrong domain is deliberately not a [`Resource::Nickname`]: it
/// falls through to the actor-id lookup, where no row matches it, which is
/// how Pleroma answers a foreign `acct:` on this host (404).
fn parse_resource<'a>(resource: &'a str, domain: &str) -> Resource<'a> {
    let account = resource.strip_prefix("acct:").unwrap_or(resource);
    let Some((nickname, host)) = account.split_once('@') else {
        return Resource::ActorId(resource);
    };
    if !nickname.is_empty() && host.eq_ignore_ascii_case(domain) {
        Resource::Nickname(nickname)
    } else {
        Resource::ActorId(resource)
    }
}

/// The user `resource` names, `None` when that is no live local account:
/// the lookup follows [`parse_resource`], and remote mirrors and
/// deactivated users are filtered out — `WebFinger` here only serves actors
/// this instance owns and still answers with.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn find_local_active_user(state: &AppState, resource: &str) -> AppResult<Option<User>> {
    let user = match parse_resource(resource, state.config.domain()) {
        Resource::Nickname(nickname) => get_by_nickname(&state.db, nickname).await?,
        Resource::ActorId(ap_id) => get_by_ap_id(&state.db, ap_id).await?,
    };
    Ok(user.filter(User::is_local_active))
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
/// representations render: subject, aliases and links exactly as Pleroma's
/// `represent_user/2` builds them.
#[derive(Debug, Serialize)]
struct Finger {
    /// `acct:nickname@domain` on this instance, the nickname stored case.
    subject: String,
    /// The actor id, plus the profile URL when the row carries a distinct
    /// one (Pleroma's aliases start from the `ap_id`; its `also_known_as`
    /// moves here when the migration ticket adds the column).
    aliases: Vec<String>,
    /// The profile-page, self and subscribe links, in that order.
    links: Vec<FingerLink>,
}

/// One `Link` element or object of a [`Finger`] document. The `type`
/// attribute is omitted for the subscribe link, which is identified by
/// its `template` instead; Pleroma's map rendering drops the same key.
#[derive(Debug, Serialize)]
struct FingerLink {
    /// The relation URI (or `self`) this link states.
    rel: &'static str,
    /// The media type of the link target, renamed to `type` in JSON.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    link_type: Option<&'static str>,
    /// The target URL, for the `href` links.
    #[serde(skip_serializing_if = "Option::is_none")]
    href: Option<String>,
    /// The templated follow URL, for the subscribe link.
    #[serde(skip_serializing_if = "Option::is_none")]
    template: Option<String>,
}

impl Finger {
    /// The document describing the local user `user` on the instance
    /// `config` serves: the `profile_url` column is the human-facing URL
    /// where a row carries one, and the actor id stands in for it in the
    /// profile link and the alias list otherwise.
    fn for_user(user: &User, config: &Config) -> Self {
        let profile_url = user.profile_url.clone();
        let mut aliases = vec![user.ap_id.clone()];
        if let Some(url) = profile_url
            .as_ref()
            .filter(|url| url.as_str() != user.ap_id)
        {
            aliases.push(url.clone());
        }
        Self {
            subject: format!("acct:{}@{}", user.nickname, config.domain()),
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
        .map(|alias| format!("<Alias>{}</Alias>", xml_escape(alias)))
        .collect();
    let links: Vec<String> = finger.links.iter().map(xrd_link).collect();
    xrd_body(format!(
        r#"<XRD xmlns="{XRD_NAMESPACE}"><Subject>{}</Subject>{}{}</XRD>"#,
        xml_escape(&finger.subject),
        aliases.concat(),
        links.concat(),
    ))
}

/// One `<Link/>` element of an XRD document: `rel` first, then whichever of
/// `type`, `href` and `template` the link carries, every value run
/// through [`xml_escape`] so a database-sourced URL cannot break out of
/// its quotes or its element.
fn xrd_link(link: &FingerLink) -> String {
    let mut attributes = vec![format!(r#"rel="{}""#, xml_escape(link.rel))];
    if let Some(link_type) = link.link_type {
        attributes.push(format!(r#"type="{}""#, xml_escape(link_type)));
    }
    if let Some(href) = &link.href {
        attributes.push(format!(r#"href="{}""#, xml_escape(href)));
    }
    if let Some(template) = &link.template {
        attributes.push(format!(r#"template="{}""#, xml_escape(template)));
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
        for (resource, want) in [
            ("acct:alice@test.example", "alice"),
            ("alice@test.example", "alice"),
            ("acct:alice@TEST.example", "alice"),
            ("acct:alice@localhost:4000", "alice"),
        ] {
            let domain = if resource.contains("localhost") {
                "LoCaLhOsT:4000"
            } else {
                "test.example"
            };
            assert_eq!(
                parse_resource(resource, domain),
                Resource::Nickname(want),
                "{resource} is a local acct form"
            );
        }
    }

    #[test]
    fn anything_else_falls_through_to_the_actor_id_lookup() {
        for resource in [
            "https://test.example/users/alice",
            "acct:alice@other.example",
            "alice@other.example",
            "@test.example",
            "acct:alice",
            "",
        ] {
            assert_eq!(
                parse_resource(resource, "test.example"),
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
