//! Unit tests of the RFC 9421 verifier over hand-built requests: a fixed
//! key and a fixed `created`, the signature base written out as literal text.

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use chrono::TimeDelta;

use super::super::rfc9421_test_support::{
    Rfc9421Key, content_digest, params_for, sign, signature_over,
};
use super::super::test_support::{BODY, HELLO_B64, HOST, KEY_ID, NOW_SECONDS, now};
use super::*;
use crate::util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM};

/// The URL every request here is addressed to.
const INBOX_URL: &str = "https://peer.example/inbox";

/// The fixed signing identity: the fixture key at the fixed `created`.
fn key() -> Rfc9421Key<'static> {
    Rfc9421Key {
        key_id: KEY_ID,
        private_pem: FIXTURE_PRIVATE_PKCS1_PEM,
        created: NOW_SECONDS,
    }
}

/// A header map out of `(name, value)` pairs.
fn header_map(pairs: &[(&str, String)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        let previous = headers.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        drop(previous);
    }
    headers
}

/// The signed `POST` of [`BODY`] to [`INBOX_URL`] as received.
fn signed_post() -> HeaderMap {
    header_map(&sign("POST", INBOX_URL, Some(BODY), &key()))
}

/// A request for `method` `path` carrying `headers`.
fn request<'a>(method: &'a str, path: &'a str, headers: &HeaderMap) -> SignedRequestParts<'a> {
    SignedRequestParts {
        method,
        path_and_query: path,
        headers: headers.clone(),
    }
}

/// Parses the one signature `headers` carry.
fn only(headers: &HeaderMap) -> Rfc9421Signature {
    let mut found = parse_rfc9421(headers).unwrap();
    assert_eq!(found.len(), 1, "exactly one signature is presented");
    found.remove(0)
}

/// Verifies the signature of `headers` as a `method` of `path` at `at`.
fn verdict(
    headers: &HeaderMap,
    method: &str,
    path: &str,
    at: DateTime<Utc>,
) -> Result<(), SignatureError> {
    verify_rfc9421(
        &only(headers),
        &request(method, path, headers),
        "https",
        FIXTURE_PUBLIC_PKCS1_PEM,
        at,
    )
}

/// [`verdict`] for the signed inbox `POST` at the fixed clock.
fn post_verdict(headers: &HeaderMap) -> Result<(), SignatureError> {
    verdict(headers, "POST", "/inbox", now())
}

/// Headers whose `Signature-Input` member carries exactly `params`, signed
/// over `lines` (the literal base lines before `@signature-params`), for
/// the `POST` of [`BODY`].
fn crafted(params: &str, lines: &str) -> HeaderMap {
    let base = format!("{lines}\n\"@signature-params\": {params}");
    header_map(&[
        ("host", HOST.to_owned()),
        ("content-digest", content_digest(BODY)),
        ("signature-input", format!("sig1={params}")),
        ("signature", signature_over("sig1", &base, &key())),
    ])
}

/// The base lines of the standard three components of the inbox `POST`.
fn standard_lines() -> String {
    format!(
        "\"@method\": POST\n\"@target-uri\": {INBOX_URL}\n\"content-digest\": {}",
        content_digest(BODY)
    )
}

#[test]
fn a_hand_built_post_verifies() {
    let headers = signed_post();
    assert_eq!(
        headers.get("content-digest").unwrap(),
        &format!("sha-256=:{HELLO_B64}:"),
        "the digest of `hello`, computed independently"
    );
    assert_eq!(post_verdict(&headers), Ok(()), "the signature verifies");
    assert_eq!(
        verify_content_digest(&headers, BODY),
        Ok(()),
        "and the digest describes the body"
    );
    assert_eq!(only(&headers).key_id(), KEY_ID, "the keyid is read");
}

#[test]
fn a_hand_built_get_with_a_query_verifies() {
    let headers = header_map(&sign(
        "GET",
        "https://peer.example/users/alice?page=2",
        None,
        &key(),
    ));
    assert_eq!(
        verdict(&headers, "GET", "/users/alice?page=2", now()),
        Ok(()),
        "a bodyless request covers no digest"
    );
    assert_eq!(
        verify_content_digest(&headers, b""),
        Ok(()),
        "and an empty body needs none"
    );
}

#[test]
fn authority_and_path_may_stand_for_the_target_uri() {
    let params = params_for(r#""@method" "@authority" "@path" "content-digest""#, &key());
    let lines = format!(
        "\"@method\": POST\n\"@authority\": {HOST}\n\"@path\": /inbox\n\"content-digest\": {}",
        content_digest(BODY)
    );
    assert_eq!(
        post_verdict(&crafted(&params, &lines)),
        Ok(()),
        "the split spelling verifies"
    );
}

#[test]
fn a_tampered_component_is_refused() {
    let headers = signed_post();
    assert_eq!(
        verdict(&headers, "POST", "/other", now()),
        Err(SignatureError::BadSignature),
        "another path"
    );
    assert_eq!(
        verdict(&headers, "GET", "/inbox", now()),
        Err(SignatureError::BadSignature),
        "another method"
    );
    assert_eq!(
        verify_rfc9421(
            &only(&headers),
            &request("POST", "/inbox", &headers),
            "http",
            FIXTURE_PUBLIC_PKCS1_PEM,
            now()
        ),
        Err(SignatureError::BadSignature),
        "another scheme"
    );
    let mut moved = headers.clone();
    let old_host = moved.insert("host", HeaderValue::from_static("evil.example"));
    drop(old_host);
    assert_eq!(
        post_verdict(&moved),
        Err(SignatureError::BadSignature),
        "another authority"
    );
    let mut forged = headers.clone();
    let input = headers.get("signature-input").unwrap().to_str().unwrap();
    let old_input = forged.insert(
        "signature-input",
        HeaderValue::from_str(&input.replace(";keyid=", ";nonce=\"x\";keyid=")).unwrap(),
    );
    drop(old_input);
    assert_eq!(
        post_verdict(&forged),
        Err(SignatureError::BadSignature),
        "a parameter added after signing changes the signed text"
    );
}

#[test]
fn a_wrong_content_digest_is_refused() {
    let headers = signed_post();
    assert_eq!(
        verify_content_digest(&headers, b"hellp"),
        Err(SignatureError::BadDigest),
        "a body that changed after signing"
    );
    let mut swapped = headers;
    let previous = swapped.insert(
        "content-digest",
        HeaderValue::from_str(&content_digest(b"hellp")).unwrap(),
    );
    drop(previous);
    assert_eq!(
        post_verdict(&swapped),
        Err(SignatureError::BadSignature),
        "a digest swapped after signing breaks the signature"
    );

    let lying = header_map(&sign("POST", INBOX_URL, Some(b"other"), &key()));
    assert_eq!(
        post_verdict(&lying),
        Ok(()),
        "a signature made over a digest of other bytes is itself valid"
    );
    assert_eq!(
        verify_content_digest(&lying, BODY),
        Err(SignatureError::BadDigest),
        "but that digest does not describe this body"
    );
}

#[test]
fn the_content_digest_header_is_read_strictly() {
    let none = HeaderMap::new();
    assert_eq!(
        verify_content_digest(&none, BODY),
        Err(SignatureError::Missing),
        "a body without a digest"
    );
    for (value, expected) in [
        ("sha-512=:AAAA:", SignatureError::UnsupportedAlgorithm),
        ("sha-256", MALFORMED_DIGEST),
        ("sha-256=AAAA", MALFORMED_DIGEST),
        ("sha-256=:!!!:", MALFORMED_BASE64),
    ] {
        let headers = header_map(&[("content-digest", value.to_owned())]);
        assert_eq!(
            verify_content_digest(&headers, BODY),
            Err(expected),
            "`{value}`"
        );
    }
    let both = header_map(&[(
        "content-digest",
        format!("sha-512=:AAAA:, sha-256=:{HELLO_B64}:"),
    )]);
    assert_eq!(
        verify_content_digest(&both, BODY),
        Ok(()),
        "other algorithms beside a matching sha-256 are ignored"
    );
}

#[test]
fn the_clock_window_is_the_cavage_one() {
    let headers = signed_post();
    for (offset, expected) in [
        (TimeDelta::hours(11) + TimeDelta::minutes(59), Ok(())),
        (
            TimeDelta::hours(12) + TimeDelta::seconds(1),
            Err(SignatureError::Expired),
        ),
        (TimeDelta::hours(-1) + TimeDelta::minutes(1), Ok(())),
        (
            TimeDelta::hours(-1) - TimeDelta::seconds(1),
            Err(SignatureError::Expired),
        ),
    ] {
        assert_eq!(
            verdict(&headers, "POST", "/inbox", now() + offset),
            expected,
            "verified {offset} after it was created"
        );
    }
}

#[test]
fn an_expired_signature_is_refused() {
    let past = params_for(r#""@method" "@target-uri" "content-digest""#, &key())
        .replace(";keyid=", &format!(";expires={};keyid=", NOW_SECONDS - 1));
    assert_eq!(
        post_verdict(&crafted(&past, &standard_lines())),
        Err(SignatureError::Expired),
        "an `expires` before now"
    );
    let future = params_for(r#""@method" "@target-uri" "content-digest""#, &key())
        .replace(";keyid=", &format!(";expires={};keyid=", NOW_SECONDS + 60));
    assert_eq!(
        post_verdict(&crafted(&future, &standard_lines())),
        Ok(()),
        "an `expires` still ahead"
    );
}

#[test]
fn another_key_does_not_verify() {
    let headers = signed_post();
    let other = verify_rfc9421(
        &only(&headers),
        &request("POST", "/inbox", &headers),
        "https",
        "not a public key",
        now(),
    );
    assert_eq!(
        other,
        Err(SignatureError::BadSignature),
        "an unparsable key"
    );
    let private = verify_rfc9421(
        &only(&headers),
        &request("POST", "/inbox", &headers),
        "https",
        FIXTURE_PRIVATE_PKCS1_PEM,
        now(),
    );
    assert_eq!(private, Err(SignatureError::BadSignature), "a private key");
}

#[test]
fn the_required_components_must_be_covered() {
    let cases = [
        (r#""@method""#, "\"@method\": POST".to_owned(), "no target"),
        (
            r#""@target-uri" "content-digest""#,
            format!(
                "\"@target-uri\": {INBOX_URL}\n\"content-digest\": {}",
                content_digest(BODY)
            ),
            "no method",
        ),
        (
            r#""@method" "@target-uri""#,
            format!("\"@method\": POST\n\"@target-uri\": {INBOX_URL}"),
            "a POST without its digest",
        ),
        (
            r#""@method" "@authority" "content-digest""#,
            format!(
                "\"@method\": POST\n\"@authority\": {HOST}\n\"content-digest\": {}",
                content_digest(BODY)
            ),
            "an authority without a path",
        ),
    ];
    for (components, lines, why) in cases {
        let params = params_for(components, &key());
        assert_eq!(
            post_verdict(&crafted(&params, &lines)),
            Err(SignatureError::Missing),
            "{why}"
        );
    }
}

#[test]
fn an_uncovered_header_is_missing_not_empty() {
    let params = params_for(
        r#""@method" "@target-uri" "content-digest" "x-absent""#,
        &key(),
    );
    let lines = format!("{}\n\"x-absent\": nothing", standard_lines());
    assert_eq!(
        post_verdict(&crafted(&params, &lines)),
        Err(SignatureError::Missing),
        "a covered header the request lacks cannot be signed over"
    );
}

#[test]
fn only_rsa_v1_5_sha256_is_checked() {
    let components = r#""@method" "@target-uri" "content-digest""#;
    let ed25519 = params_for(components, &key()).replace("rsa-v1_5-sha256", "ed25519");
    assert_eq!(
        post_verdict(&crafted(&ed25519, &standard_lines())),
        Err(SignatureError::UnsupportedAlgorithm),
        "ed25519 needs a key actor documents do not publish"
    );
    let unnamed = params_for(components, &key()).replace(";alg=\"rsa-v1_5-sha256\"", "");
    assert_eq!(
        post_verdict(&crafted(&unnamed, &standard_lines())),
        Ok(()),
        "an absent alg is read as RSA"
    );
}

#[test]
fn a_signature_without_created_is_refused() {
    let params = format!(
        r#"("@method" "@target-uri" "content-digest");keyid="{KEY_ID}";alg="rsa-v1_5-sha256""#
    );
    assert_eq!(
        post_verdict(&crafted(&params, &standard_lines())),
        Err(SignatureError::Missing),
        "no created means no freshness"
    );
}

#[test]
fn unrelated_or_unreadable_headers_present_no_signature() {
    assert_eq!(
        parse_rfc9421(&HeaderMap::new()),
        Ok(Vec::new()),
        "no headers"
    );
    assert!(!rfc9421_offered(&HeaderMap::new()), "so none offered");
    let headers = signed_post();
    assert!(rfc9421_offered(&headers), "Signature-Input is the marker");

    let mut other_label = headers.clone();
    let old_signature = other_label.insert(
        "signature",
        HeaderValue::from_str(
            &headers
                .get("signature")
                .unwrap()
                .to_str()
                .unwrap()
                .replace("sig1=", "sig9="),
        )
        .unwrap(),
    );
    drop(old_signature);
    assert_eq!(
        parse_rfc9421(&other_label),
        Ok(Vec::new()),
        "an input member without a signature of its label is skipped"
    );

    let mut cavage_form = headers.clone();
    let rfc_signature = cavage_form.insert(
        "signature",
        HeaderValue::from_static(r#"keyId="k",headers="date",signature="aGk=""#),
    );
    drop(rfc_signature);
    assert!(
        parse_rfc9421(&cavage_form).is_err(),
        "a cavage Signature beside Signature-Input is not an RFC 9421 one"
    );

    let mut no_signature = headers;
    let removed = no_signature.remove("signature");
    drop(removed);
    assert_eq!(
        parse_rfc9421(&no_signature),
        Err(SignatureError::Missing),
        "Signature-Input alone"
    );
}
