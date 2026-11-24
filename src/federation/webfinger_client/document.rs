//! The shapes of a `WebFinger` answer this client consumes: a subject and an
//! ordered list of links, read from either representation, plus the two
//! tests a link has to pass to name an actor.

use serde::Deserialize;
use serde_json::{Map, Value};

use super::{ACTIVITY_JSON, LD_JSON, SELF_REL};
use crate::util::{media_type::essence, url::is_http_url};

/// One `Link` element or object of a `WebFinger` document: JRD objects
/// deserialize straight into it, and the XRD reader fills the same shape
/// from the element's attributes (`template` matters for host-meta, `rel`,
/// `type` and `href` for the actor).
#[derive(Debug, Default, Deserialize)]
pub(super) struct FingerLink {
    /// The relation this link states.
    pub(super) rel: Option<String>,
    /// The media type of the link target, the JSON key `type`.
    #[serde(rename = "type")]
    pub(super) link_type: Option<String>,
    /// The target URL.
    pub(super) href: Option<String>,
    /// The templated URL (LRDD links in host-meta).
    pub(super) template: Option<String>,
}

/// The parts of a `WebFinger` answer this client consumes, read from either
/// representation.
#[derive(Debug)]
pub(super) struct FingerDocument {
    /// The `subject` / `<Subject>` value, when present.
    pub(super) subject: Option<String>,
    /// Every advertised link, in document order.
    pub(super) links: Vec<FingerLink>,
}

impl FingerDocument {
    /// The actor URL: the `href` of the first `rel=self` link whose `type`
    /// names an `ActivityPub` media type (see [`is_activity_media_type`])
    /// and whose `href` is an absolute `http(s)` URL. Links of any other
    /// shape — including the profile-page link — are not actors.
    pub(super) fn actor_href(&self) -> Option<&str> {
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

    /// This link as the JRD object an XRD `<Link>` element states: exactly
    /// the members its attributes carry, under their JSON names (`type` for
    /// [`link_type`](Self::link_type)).
    pub(super) fn jrd_object(&self) -> Value {
        let mut object = Map::new();
        for (name, value) in [
            ("rel", &self.rel),
            ("type", &self.link_type),
            ("href", &self.href),
            ("template", &self.template),
        ] {
            if let Some(text) = value {
                let _set: Option<Value> =
                    object.insert(name.to_owned(), Value::String(text.clone()));
            }
        }
        Value::Object(object)
    }
}

/// Whether a link's declared `type` names an `ActivityPub` JSON document,
/// ignoring parameters and whitespace: `application/activity+json` or
/// `application/ld+json` with any (or no) parameters after the `;`.
fn is_activity_media_type(candidate: &str) -> bool {
    let media_type = essence(candidate);
    media_type == ACTIVITY_JSON || media_type == LD_JSON
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::webfinger_client::{JRD_JSON, WebfingerError, test_support::*};

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
}
