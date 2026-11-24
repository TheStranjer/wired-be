//! Reading an answer: which of the two representations a response states,
//! the JRD and XRD readers that turn a body into a [`FingerDocument`], and
//! the normalization that renders an XRD answer as the JRD document it
//! states.

use quick_xml::{escape::unescape, events::BytesStart, events::Event, reader::Reader};
use reqwest::header::{CONTENT_TYPE, HeaderMap};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{http_client::HttpResponse, util::media_type::essence};

use super::{
    JRD_JSON, WebfingerError, XRD_XML, XRD_XML_VERSION,
    document::{FingerDocument, FingerLink},
    note_malformed,
};

/// Reads a `WebFinger` answer, choosing the JRD or XRD reader from the
/// response's content type and — when that is absent or names neither
/// family — from the document's first character.
pub(super) fn parse_finger_response(
    response: &HttpResponse,
) -> Result<FingerDocument, WebfingerError> {
    let body = String::from_utf8(response.body.to_vec()).map_err(note_malformed)?;
    if answered_json(&response.headers, &response.body) {
        parse_jrd(&body)
    } else {
        parse_xrd(&body)
    }
}

/// The whole `WebFinger` answer as a JRD JSON object, chosen by the same
/// format rules as [`parse_finger_response`]: a JSON body is returned
/// verbatim (every member it carries survives, and anything that is not a
/// JSON object is [`WebfingerError::Malformed`]); an XRD body is
/// normalized through [`jrd_of_xrd`].
pub(super) fn full_jrd(response: &HttpResponse) -> Result<Value, WebfingerError> {
    if answered_json(&response.headers, &response.body) {
        let document: Value = serde_json::from_slice(&response.body).map_err(note_malformed)?;
        if document.is_object() {
            Ok(document)
        } else {
            Err(note_malformed(
                "a webfinger JRD answer is not a JSON object",
            ))
        }
    } else {
        let body = String::from_utf8(response.body.to_vec()).map_err(note_malformed)?;
        let document = parse_xrd(&body)?;
        Ok(jrd_of_xrd(&document))
    }
}

/// An XRD answer as the JRD document it states: `{subject?, links[]}`,
/// every `<Link>` element one object carrying exactly the attributes it
/// had. The vocabulary XRD has and JRD does not (namespaces, element
/// ordering, XML syntax) is dropped, as any reader of the JRD shape never
/// sees it anyway.
fn jrd_of_xrd(document: &FingerDocument) -> Value {
    let links: Vec<Value> = document.links.iter().map(FingerLink::jrd_object).collect();
    let mut object = Map::new();
    if let Some(subject) = &document.subject {
        let _set: Option<Value> =
            object.insert("subject".to_owned(), Value::String(subject.clone()));
    }
    let _set: Option<Value> = object.insert("links".to_owned(), Value::Array(links));
    Value::Object(object)
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
        .map(essence)
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
pub(super) fn parse_xrd(body: &str) -> Result<FingerDocument, WebfingerError> {
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
    use serde_json::json;

    use super::*;
    use crate::federation::webfinger_client::{finger_with_base, test_support::*};

    #[tokio::test]
    async fn finger_answers_the_full_jrd_document() {
        let app = static_route("/.well-known/webfinger", Some(JRD_JSON), JRD_ALICE);
        let (server, base) = serve(app).await;
        let document = finger_with_base(&client(), ACCT, Some(&base))
            .await
            .expect("the mock answers a JRD document");
        server.abort();
        assert_eq!(
            document["subject"],
            json!("acct:alice@remote.example"),
            "the subject survives"
        );
        assert_eq!(
            document["aliases"],
            json!(["https://remote.example/users/alice"]),
            "members the actor reader ignores stay in the document"
        );
        let links = document["links"].as_array().expect("the links array");
        assert_eq!(links.len(), 3, "every advertised link, in order");
        assert_eq!(
            links[2]["template"],
            json!("https://remote.example/ostatus_subscribe?acct={uri}"),
            "the subscribe template is there to read"
        );
    }

    #[tokio::test]
    async fn finger_normalizes_an_xrd_answer_to_jrd() {
        let app = static_route("/.well-known/webfinger", Some(XRD_XML), XRD_ALICE);
        let (server, base) = serve(app).await;
        let document = finger_with_base(&client(), ACCT, Some(&base))
            .await
            .expect("an XRD answer normalizes");
        server.abort();
        assert_eq!(
            document,
            json!({
                "subject": "acct:alice@remote.example",
                "links": [
                    {"rel": "http://webfinger.net/rel/profile-page",
                     "type": "text/html",
                     "href": "https://remote.example/@alice"},
                    {"rel": "self",
                     "type": "application/activity+json",
                     "href": "https://remote.example/users/alice"},
                ],
            }),
            "each <Link> becomes exactly the object its attributes state"
        );
    }

    #[tokio::test]
    async fn finger_rejects_a_json_answer_that_is_no_object() {
        let app = static_route("/.well-known/webfinger", Some(JRD_JSON), "[]");
        let error = finger_against(app, ACCT).await.unwrap_err();
        assert!(
            matches!(error, WebfingerError::Malformed),
            "a JRD document is a JSON object: {error:?}"
        );
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
}
