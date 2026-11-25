//! Tests for `GET /.well-known/webfinger` and `GET /.well-known/host-meta`
//! (`src/api/well_known.rs`).
//!
//! `WebFinger` resolves `acct:nick@domain`, bare `nick@domain` and actor URLs
//! to live local accounts and answers JRD JSON by default or XRD XML when
//! the client asks; unknown resources, foreign domains and deactivated or
//! remote accounts are 404s, a missing `resource` parameter is a 400, and
//! host-meta always serves the lrdd template pointing at `WebFinger`.
//!
//! The pure request classification and format negotiation are covered
//! where they live, in `src/api/well_known.rs`; what these tests own is
//! the route: status codes, media types, the exact document bodies and
//! XML escaping end to end over HTTP.

#![cfg(test)]

use axum::http::StatusCode;
use quick_xml::{XmlVersion, events::Event, reader::Reader};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    config::{
        BASE_URL_VAR, Config, DATABASE_URL_VAR, PROTOCOLS_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
    },
    models::user::{Flag, NewUser},
};

use crate::common;

/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";
/// The media type of the JSON answer.
const JRD_JSON: &str = "application/jrd+json";
/// The media type of the XML answers.
const XRD_XML: &str = "application/xrd+xml";
/// The namespace every XRD root element declares.
const XRD_NS: &str = "http://docs.oasis-open.org/ns/xri/xrd-1.0";
/// The actor id of the fixture `alice`.
const ALICE_AP_ID: &str = "https://test.example/users/alice";
/// `acct:alice@test.example` percent-encoded as a query-parameter value.
const ALICE_ACCT: &str = "acct%3Aalice%40test.example";
/// Alice's actor id percent-encoded as a query-parameter value.
const ALICE_URL: &str = "https%3A%2F%2Ftest.example%2Fusers%2Falice";
/// `acct:erin@test.example` percent-encoded, for the deactivated fixture.
const ERIN_ACCT: &str = "acct%3Aerin%40test.example";

/// The `/.well-known/webfinger` path for a percent-encoded `resource`.
fn webfinger_path(resource: &str) -> String {
    format!("/.well-known/webfinger?resource={resource}")
}

/// The exact JRD document served for a bare local `alice`.
fn alice_jrd() -> Value {
    json!({
        "subject": "acct:alice@test.example",
        "aliases": [ALICE_AP_ID],
        "links": [
            {
                "rel": "http://webfinger.net/rel/profile-page",
                "type": "text/html",
                "href": ALICE_AP_ID,
            },
            {
                "rel": "self",
                "type": "application/activity+json",
                "href": ALICE_AP_ID,
            },
            {
                "rel": "http://ostatus.org/schema/1.0/subscribe",
                "template": "https://test.example/authorize_interaction?uri={uri}",
            },
        ],
    })
}

/// The exact XRD document of the same answer: one `Subject`, one `Alias`
/// and the three `Link` elements, in order.
fn alice_xrd() -> String {
    format!(
        r#"<XRD xmlns="{XRD_NS}"><Subject>acct:alice@test.example</Subject><Alias>{ALICE_AP_ID}</Alias><Link rel="http://webfinger.net/rel/profile-page" type="text/html" href="{ALICE_AP_ID}"/><Link rel="self" type="application/activity+json" href="{ALICE_AP_ID}"/><Link rel="http://ostatus.org/schema/1.0/subscribe" template="{BASE}/authorize_interaction?uri={{uri}}"/></XRD>"#
    )
}

/// Requests `/.well-known/webfinger?resource=<encoded>` from `pool`'s app.
async fn webfinger(pool: &PgPool, resource: &str) -> common::TestResponse {
    let app = common::test_app(pool.clone());
    common::TestRequest::get(&webfinger_path(resource))
        .send(&app)
        .await
}

/// The same request with an explicit `Accept` header.
async fn webfinger_with_accept(
    pool: &PgPool,
    resource: &str,
    accept: &str,
) -> common::TestResponse {
    let app = common::test_app(pool.clone());
    common::TestRequest::get(&webfinger_path(resource))
        .header("accept", accept)
        .send(&app)
        .await
}

/// The JSON answer to an `acct:` lookup: subject, aliases and the three
/// links, with nothing else — exact key sets included.
#[tokio::test]
async fn acct_resource_resolves_the_local_user() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let response = webfinger(&pool, ALICE_ACCT).await;
    assert_eq!(response.status, StatusCode::OK, "live local user");
    assert_eq!(
        response.header("content-type"),
        Some(JRD_JSON),
        "the default answer is JRD"
    );
    let document = response.json_value();
    assert_eq!(document, alice_jrd(), "the exact JRD document");
    assert_eq!(
        common::sorted_keys(&document),
        ["aliases", "links", "subject"],
        "top-level keys"
    );
    assert_eq!(
        common::sorted_keys(&document["links"][0]),
        ["href", "rel", "type"],
        "profile link keys"
    );
    assert_eq!(
        common::sorted_keys(&document["links"][1]),
        ["href", "rel", "type"],
        "self link keys"
    );
    assert_eq!(
        common::sorted_keys(&document["links"][2]),
        ["rel", "template"],
        "the subscribe link has no type or href"
    );
}

/// A bare actor URL as `resource` resolves the same document (Pleroma's
/// actor-id fallback lookup), and so does the `acct:` form without its
/// prefix.
#[tokio::test]
async fn actor_url_and_bare_acct_resolve_the_same_user() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    for resource in [ALICE_URL, "alice%40test.example"] {
        let response = webfinger(&pool, resource).await;
        assert_eq!(response.status, StatusCode::OK, "{resource} resolves alice");
        assert_eq!(
            response.json_value(),
            alice_jrd(),
            "{resource} gets the same document"
        );
    }
}

/// Domain and nickname are matched case-insensitively; the subject uses
/// the stored spelling either way.
#[tokio::test]
async fn the_lookup_is_case_insensitive() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    for resource in [
        "acct:alice@TEST.EXAMPLE",
        "acct:ALiCE@test.example",
        "acct:ALICE@Test.Example",
    ] {
        let response = webfinger(&pool, resource).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{resource} still finds alice"
        );
        assert_eq!(
            response.json_value()["subject"],
            "acct:alice@test.example",
            "{resource} gets the stored spelling in the subject"
        );
    }
}

/// A user with a profile URL aliases it and points the profile-page link
/// at it; a profile URL that is just the actor id again adds no alias.
#[tokio::test]
async fn a_profile_url_joins_the_aliases_and_the_profile_link() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        profile_url: Some(format!("{BASE}/@alice")),
        ..new
    })
    .await;
    let response = webfinger(&pool, ALICE_ACCT).await;
    assert_eq!(response.status, StatusCode::OK, "alice");
    let document = response.json_value();
    assert_eq!(
        document["aliases"],
        json!([alice.ap_id, format!("{BASE}/@alice")]),
        "the profile URL is the second alias"
    );
    assert_eq!(
        document["links"][0]["href"],
        format!("{BASE}/@alice"),
        "the profile-page link points at the profile URL"
    );
    assert_eq!(
        document["links"][1]["href"], alice.ap_id,
        "the self link stays the actor id"
    );

    let carol = common::create_local_user_with(&pool, "carol", |new| NewUser {
        profile_url: Some(new.ap_id.clone()),
        ..new
    })
    .await;
    let carol_response = webfinger(&pool, "acct%3Acarol%40test.example").await;
    assert_eq!(
        carol_response.json_value()["aliases"],
        json!([carol.ap_id]),
        "a profile URL equal to the actor id is not aliased twice"
    );
}

/// A wrong domain, an unknown nickname, an empty resource and a foreign
/// actor URL are all 404s with the standard JSON error body.
#[tokio::test]
async fn resources_that_name_no_local_user_are_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    for resource in [
        "acct:alice@other.example",
        "alice@other.example",
        "acct:nobody@test.example",
        "https://other.example/users/alice",
        "",
    ] {
        let response = webfinger(&pool, resource).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{resource:?} names no local user"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the 404 for {resource:?} is JSON"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Record not found"}),
            "the 404 body for {resource:?}"
        );
    }
}

/// Remote mirrors and deactivated local accounts are not served: both the
/// `acct:` and the actor-URL form answer 404 for them.
#[tokio::test]
async fn remote_and_deactivated_users_are_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let erin = common::create_local_user_with(&pool, "erin", |new| NewUser {
        is_active: Flag::new(false),
        ..new
    })
    .await;
    let bob_url = bob.ap_id.replace(':', "%3A").replace('/', "%2F");
    let erin_url = erin.ap_id.replace(':', "%3A").replace('/', "%2F");
    for resource in [
        "acct%3Abob%40test.example".to_owned(),
        bob_url,
        ERIN_ACCT.to_owned(),
        erin_url,
    ] {
        let response = webfinger(&pool, &resource).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{resource} is no live local account"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Record not found"}),
            "the 404 body for {resource}"
        );
    }
}

/// `Accept: application/xrd+xml` gets the same data as XRD XML, element
/// for element; an `Accept` naming JRD (or nothing usable) keeps the JSON.
#[tokio::test]
async fn accept_xrd_xml_switches_to_the_xml_representation() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    for accept in [XRD_XML, "text/html, application/xrd+xml;q=0.9"] {
        let response = webfinger_with_accept(&pool, ALICE_ACCT, accept).await;
        assert_eq!(response.status, StatusCode::OK, "{accept} gets alice");
        assert_eq!(
            response.header("content-type"),
            Some(XRD_XML),
            "the XML answer's media type for {accept}"
        );
        assert_eq!(response.text(), alice_xrd(), "the exact XRD document");
    }
    for accept in ["application/jrd+json", "application/json", "*/*"] {
        let response = webfinger_with_accept(&pool, ALICE_ACCT, accept).await;
        assert_eq!(
            response.header("content-type"),
            Some(JRD_JSON),
            "{accept} keeps the JSON answer"
        );
    }
}

/// The `format=xml` query parameter is the explicit override Pleroma's
/// `SetFormatPlug` honors: it wins over an `Accept`, and `.xml` is the
/// same request.
#[tokio::test]
async fn format_param_switches_to_the_xml_representation() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    for format in ["xml", ".xml"] {
        let path = format!("{}&format={format}", webfinger_path(ALICE_ACCT));
        let response = common::TestRequest::get(&path)
            .header("accept", "application/json")
            .send(&app)
            .await;
        assert_eq!(response.status, StatusCode::OK, "format={format}");
        assert_eq!(
            response.header("content-type"),
            Some(XRD_XML),
            "format={format} asks for XML against the Accept header"
        );
        assert_eq!(response.text(), alice_xrd(), "the exact XRD document");
    }
    let json_path = format!("{}&format=json", webfinger_path(ALICE_ACCT));
    let json_response = common::TestRequest::get(&json_path).send(&app).await;
    assert_eq!(
        json_response.header("content-type"),
        Some(JRD_JSON),
        "format=json keeps the JSON answer"
    );
}

/// A request without the `resource` parameter is the ticket's 400 with
/// the `Missing resource` JSON body.
#[tokio::test]
async fn missing_resource_is_a_bad_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/.well-known/webfinger")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "no resource parameter"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "errors are JSON"
    );
    assert_eq!(
        response.text(),
        r#"{"error":"Missing resource"}"#,
        "the exact 400 body"
    );
}

/// `GET /.well-known/host-meta` is the static lrdd descriptor pointing at
/// `WebFinger`, as XRD XML.
#[tokio::test]
async fn host_meta_advertises_the_webfinger_template() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/.well-known/host-meta")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "host-meta status");
    assert_eq!(
        response.header("content-type"),
        Some(XRD_XML),
        "host-meta is XRD XML"
    );
    assert_eq!(
        response.text(),
        format!(
            r#"<XRD xmlns="{XRD_NS}"><Link rel="lrdd" template="{BASE}/.well-known/webfinger?resource={{uri}}"/></XRD>"#
        ),
        "the exact host-meta document"
    );
}

/// Every attribute of every element in the XRD `body` as `(name, value)`,
/// with character references resolved; panics when `body` is not
/// well-formed XML.
fn xrd_attributes(body: &str) -> Vec<(String, String)> {
    let mut reader = Reader::from_str(body);
    let mut found = Vec::new();
    loop {
        match reader.read_event().expect("the XRD is well-formed XML") {
            Event::Start(element) | Event::Empty(element) => {
                for attribute in element.attributes() {
                    let attribute = attribute.expect("a well-formed attribute");
                    let value = attribute
                        .normalized_value(XmlVersion::Explicit1_0)
                        .expect("a resolvable attribute value");
                    found.push((attribute.key.as_ref().to_owned(), value.into_owned()));
                }
            }
            Event::Eof => break,
            Event::End(_)
            | Event::Text(_)
            | Event::CData(_)
            | Event::Comment(_)
            | Event::Decl(_)
            | Event::PI(_)
            | Event::DocType(_)
            | Event::GeneralRef(_) => {}
        }
    }
    found
}

/// An apostrophe in a stored value is written as `&#39;`, which is a valid
/// XML character reference: the `WebFinger` XRD still parses with `quick_xml`
/// and the attribute reads back as the original URL; host-meta parses too.
#[tokio::test]
async fn xrd_with_an_apostrophe_parses_as_xml() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let url = "https://test.example/o'brien";
    let _alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        profile_url: Some(url.to_owned()),
        ..new
    })
    .await;
    let xml = webfinger_with_accept(&pool, ALICE_ACCT, XRD_XML).await;
    assert_eq!(xml.status, StatusCode::OK, "the XML lookup succeeds");
    let body = xml.text();
    assert!(
        body.contains("https://test.example/o&#39;brien"),
        "the apostrophe is a numeric character reference: {body}"
    );
    assert!(
        xrd_attributes(&body)
            .iter()
            .any(|(name, value)| name == "href" && value == url),
        "an href attribute reads back as the original URL: {body}"
    );
    let app = common::test_app(pool);
    let host_meta = common::TestRequest::get("/.well-known/host-meta")
        .send(&app)
        .await;
    assert!(
        xrd_attributes(&host_meta.text())
            .iter()
            .any(|(name, _value)| name == "template"),
        "host-meta parses and carries its template"
    );
}

/// Values from the database cannot break out of the XML: every special
/// character of a stored profile URL arrives escaped in the XRD document
/// and raw in the JRD one.
#[tokio::test]
async fn stored_urls_are_escaped_in_xml_and_raw_in_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let evil = r#"https://test.example/&<>a"b'c"#;
    let _alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        profile_url: Some(evil.to_owned()),
        ..new
    })
    .await;
    let xml = webfinger_with_accept(&pool, ALICE_ACCT, XRD_XML).await;
    assert_eq!(xml.status, StatusCode::OK, "the XML lookup succeeds");
    let body = xml.text();
    assert!(
        body.contains("https://test.example/&amp;&lt;&gt;a&quot;b&#39;c"),
        "every special character arrives as its entity: {body}"
    );
    assert!(
        !body.contains(evil),
        "the raw URL never appears in the document"
    );
    let document = webfinger(&pool, ALICE_ACCT).await.json_value();
    assert_eq!(
        document["aliases"][1], evil,
        "the JSON document carries the URL verbatim"
    );
    assert_eq!(
        document["links"][0]["href"], evil,
        "the profile link carries it verbatim too"
    );
}

/// The test configuration with `PROTOCOLS` set to `protocols`.
///
/// # Panics
///
/// If `protocols` is not a valid `PROTOCOLS` value; the tests pass fixed ones.
fn config_with_protocols(protocols: &str) -> Config {
    Config::from_lookup(|key| match key {
        DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
        BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
        PROTOCOLS_VAR => Some(protocols.to_owned()),
        _ => None,
    })
    .expect("the fixed protocols test config is valid")
}

/// With `activitypub,fixture` the fixture's alias follows `ActivityPub`'s
/// aliases, in the JSON and in the XML, and `ActivityPub`'s links are unchanged.
#[tokio::test]
async fn the_fixture_alias_follows_the_activitypub_aliases() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        profile_url: Some(format!("{BASE}/@alice")),
        ..new
    })
    .await;
    let app = common::test_app_with_config(pool, config_with_protocols("activitypub,fixture"));

    let jrd = common::TestRequest::get(&webfinger_path(ALICE_ACCT))
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        jrd["aliases"],
        json!([ALICE_AP_ID, format!("{BASE}/@alice"), "fixture:alice"]),
        "the fixture alias comes last in the JSON"
    );
    assert_eq!(
        jrd["links"].as_array().map(Vec::len),
        Some(3),
        "the fixture adds no links"
    );

    let xrd = common::TestRequest::get(&webfinger_path(ALICE_ACCT))
        .header("accept", XRD_XML)
        .send(&app)
        .await
        .text();
    let want_aliases = format!(
        "<Alias>{ALICE_AP_ID}</Alias><Alias>{BASE}/@alice</Alias><Alias>fixture:alice</Alias><Link"
    );
    assert!(
        xrd.contains(&want_aliases),
        "the fixture alias comes last in the XML: {xrd}"
    );
}

/// With only `fixture` enabled the subject is still answered, with the
/// fixture's alias alone and none of `ActivityPub`'s links.
#[tokio::test]
async fn a_fixture_only_instance_answers_without_activitypub_links() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app_with_config(pool, config_with_protocols("fixture"));

    let jrd = common::TestRequest::get(&webfinger_path(ALICE_ACCT))
        .send(&app)
        .await;
    assert_eq!(jrd.status, StatusCode::OK, "the lookup still answers");
    assert_eq!(
        jrd.json_value(),
        json!({
            "subject": "acct:alice@test.example",
            "aliases": ["fixture:alice"],
            "links": [],
        }),
        "the subject and the fixture alias, nothing of ActivityPub"
    );

    let xrd = common::TestRequest::get(&webfinger_path(ALICE_ACCT))
        .header("accept", XRD_XML)
        .send(&app)
        .await;
    assert_eq!(
        xrd.text(),
        format!(
            r#"<XRD xmlns="{XRD_NS}"><Subject>acct:alice@test.example</Subject><Alias>fixture:alice</Alias></XRD>"#
        ),
        "the XML carries the same subject and alias"
    );
}
