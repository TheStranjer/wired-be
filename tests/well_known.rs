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

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::user::{Flag, NewUser};

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
                "template": "https://test.example/ostatus_subscribe?acct={uri}",
            },
        ],
    })
}

/// The exact XRD document of the same answer: one `Subject`, one `Alias`
/// and the three `Link` elements, in order.
fn alice_xrd() -> String {
    format!(
        r#"<XRD xmlns="{XRD_NS}"><Subject>acct:alice@test.example</Subject><Alias>{ALICE_AP_ID}</Alias><Link rel="http://webfinger.net/rel/profile-page" type="text/html" href="{ALICE_AP_ID}"/><Link rel="self" type="application/activity+json" href="{ALICE_AP_ID}"/><Link rel="http://ostatus.org/schema/1.0/subscribe" template="{BASE}/ostatus_subscribe?acct={{uri}}"/></XRD>"#
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

/// The sorted key names of `object`.
fn key_set(object: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = object
        .as_object()
        .expect("a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The JSON answer to an `acct:` lookup: subject, aliases and the three
/// links, with nothing else — exact key sets included.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn acct_resource_resolves_the_local_user(pool: PgPool) {
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
        key_set(&document),
        ["aliases", "links", "subject"],
        "top-level keys"
    );
    assert_eq!(
        key_set(&document["links"][0]),
        ["href", "rel", "type"],
        "profile link keys"
    );
    assert_eq!(
        key_set(&document["links"][1]),
        ["href", "rel", "type"],
        "self link keys"
    );
    assert_eq!(
        key_set(&document["links"][2]),
        ["rel", "template"],
        "the subscribe link has no type or href"
    );
}

/// A bare actor URL as `resource` resolves the same document (Pleroma's
/// actor-id fallback lookup), and so does the `acct:` form without its
/// prefix.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn actor_url_and_bare_acct_resolve_the_same_user(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_lookup_is_case_insensitive(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_profile_url_joins_the_aliases_and_the_profile_link(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn resources_that_name_no_local_user_are_not_found(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn remote_and_deactivated_users_are_not_found(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn accept_xrd_xml_switches_to_the_xml_representation(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn format_param_switches_to_the_xml_representation(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn missing_resource_is_a_bad_request(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn host_meta_advertises_the_webfinger_template(pool: PgPool) {
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

/// Values from the database cannot break out of the XML: every special
/// character of a stored profile URL arrives escaped in the XRD document
/// and raw in the JRD one.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn stored_urls_are_escaped_in_xml_and_raw_in_json(pool: PgPool) {
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
        body.contains("https://test.example/&amp;&lt;&gt;a&quot;b&apos;c"),
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
