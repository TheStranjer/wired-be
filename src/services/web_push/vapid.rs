//! The RFC 8292 VAPID authorization beside the content coding: the ES256
//! JWT [`vapid_headers`] signs over one endpoint's origin
//! ([`endpoint_origin`]), the instance's [`vapid_subject`], and the `TTL`
//! and `Urgency` header values every push `POST` carries.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeDelta, Utc};
use p256::{
    ecdsa::{Signature, SigningKey, signature::Signer as _},
    pkcs8::DecodePrivateKey as _,
};
use serde_json::json;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    util::url::parse_http_url,
};

/// The `TTL` header every push message carries: four weeks, Pleroma's
/// `web_push_encryption` default and Mastodon's push TTL.
pub const TTL_HEADER: &str = "2419200";
/// The `Urgency` header: the middle of RFC 8030 §5.3's three levels, which
/// a notification delivery is.
pub const URGENCY_HEADER: &str = "normal";
/// RFC 8292 §2.1 caps a VAPID JWT's life at 24 hours; the ticket signs
/// half of that.
const VAPID_JWT_LIFETIME: TimeDelta = TimeDelta::hours(12);

/// The VAPID authorization for one push `POST`: an ES256 JWT.
///
/// The token is signed over the endpoint's origin, issued now and expiring
/// twelve hours later, with `subject` as its `sub`, rendered as
/// the header value `vapid t=<jwt>, k=<public key>`.
///
/// Returns the header value and the JWT's expiry, so a caller with a long
/// retry chain can tell when to re-sign.
///
/// # Errors
///
/// [`AppError::Unprocessable`] when `endpoint` is not an `http(s)` URL with
/// a host (there would be no origin to sign); [`AppError::Internal`] when
/// `private_pem` is not a PKCS#8 P-256 key or the ECDSA operation fails —
/// the messages name the failure, never key material.
pub fn vapid_headers(
    endpoint: &str,
    private_pem: &str,
    public_key_b64: &str,
    subject: &str,
) -> AppResult<(String, DateTime<Utc>)> {
    let audience = endpoint_origin(endpoint)?;
    let expires_at = Utc::now() + VAPID_JWT_LIFETIME;
    let header = URL_SAFE_NO_PAD.encode(json!({ "typ": "JWT", "alg": "ES256" }).to_string());
    let claims = URL_SAFE_NO_PAD.encode(
        json!({
            "aud": audience,
            "exp": expires_at.timestamp(),
            "sub": subject,
        })
        .to_string(),
    );
    let signing_input = format!("{header}.{claims}");
    let signing_key = SigningKey::from_pkcs8_pem(private_pem).map_err(|err| {
        AppError::Internal(format!(
            "the stored VAPID private key does not parse: {err}"
        ))
    })?;
    let signature: Signature = signing_key.sign(signing_input.as_bytes());
    let token = format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    );
    Ok((format!("vapid t={token}, k={public_key_b64}"), expires_at))
}

/// The JWT `aud`: the endpoint URL's origin — `scheme://host`, the port
/// appended only when it is not the scheme's default, the normalization a
/// push service compares against.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for an unparsable URL, a non-`http(s)`
/// scheme or a missing host.
fn endpoint_origin(endpoint: &str) -> AppResult<String> {
    parse_http_url(endpoint)
        .map(|url| url.origin().ascii_serialization())
        .ok_or_else(|| {
            AppError::Unprocessable(
                "the push endpoint is not an http(s) URL with a host".to_owned(),
            )
        })
}

/// The VAPID `sub` this instance signs with (RFC 8292 §2.1: a `mailto:` or
/// `https:` URI): the configured admin contact when there is one, the
/// instance's own origin otherwise.
#[must_use]
pub fn vapid_subject(config: &Config) -> String {
    config.contact_email.as_deref().map_or_else(
        || config.base_url.clone(),
        |email| format!("mailto:{email}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::Value;

    use crate::{
        config::{
            BASE_URL_VAR, CONTACT_EMAIL_VAR, DATABASE_URL_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
        },
        services::{vapid, web_push::test_support::b64},
    };

    #[test]
    fn vapid_signs_an_es256_jwt_for_the_endpoint_origin() {
        use p256::ecdsa::{VerifyingKey, signature::Verifier as _};

        let pair = vapid::generate_pair_for_tests().expect("a test VAPID pair");
        let (authorization, expires_at) = vapid_headers(
            "https://push.example.net/push/JzLQ3raZJf?token=1",
            pair.private_pem(),
            pair.public_key(),
            "mailto:admin@example.org",
        )
        .expect("the header signs");
        let (token, key) = authorization
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split_once(", k="))
            .expect("the header is the vapid scheme with both parameters");
        assert_eq!(key, pair.public_key(), "k names the instance public key");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "header.claims.signature");
        let jwt_header: Value =
            serde_json::from_slice(&b64(parts[0])).expect("the JWT header is base64url JSON");
        assert_eq!(jwt_header["alg"], "ES256", "the VAPID algorithm");
        assert_eq!(jwt_header["typ"], "JWT", "the token type");
        let claims: Value =
            serde_json::from_slice(&b64(parts[1])).expect("the JWT claims are base64url JSON");
        assert_eq!(
            claims["aud"], "https://push.example.net",
            "aud is the origin, without path or query"
        );
        assert_eq!(
            claims["sub"], "mailto:admin@example.org",
            "sub passes through"
        );
        let expected = (Utc::now() + TimeDelta::hours(12)).timestamp();
        let exp = claims["exp"].as_i64().expect("exp is a number");
        assert!(
            (exp - expected).abs() <= 60,
            "exp is twelve hours out: {exp} vs {expected}"
        );
        assert_eq!(
            exp,
            expires_at.timestamp(),
            "the reported expiry is the signed `exp`"
        );
        let signature_bytes = b64(parts[2]);
        assert_eq!(
            signature_bytes.len(),
            64,
            "the JWT signature is the fixed r||s form"
        );
        let verifying =
            VerifyingKey::from_sec1_bytes(&b64(pair.public_key())).expect("the public key parses");
        let signature = Signature::from_slice(&signature_bytes).expect("the signature parses");
        verifying
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("the signature verifies against the instance public key");
        assert!(
            verifying.verify(b"tampered", &signature).is_err(),
            "a JWT over other bytes must not verify with this key"
        );
    }

    #[test]
    fn a_vapid_endpoint_keeps_a_non_default_port_and_refuses_junk() {
        let pair = vapid::generate_pair_for_tests().expect("a test VAPID pair");
        let (authorization, _expires) = vapid_headers(
            "https://push.example:8443/p",
            pair.private_pem(),
            pair.public_key(),
            "https://wired.example",
        )
        .expect("a non-default port signs fine");
        let claims_text = authorization
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split_once(','))
            .map(|(token, _rest)| token.split('.').nth(1).unwrap_or_default().to_owned())
            .expect("the header carries claims");
        let claims: Value = serde_json::from_slice(&b64(&claims_text)).expect("the claims decode");
        assert_eq!(
            claims["aud"], "https://push.example:8443",
            "the port is signed"
        );
        assert_eq!(
            claims["sub"], "https://wired.example",
            "the https subject form is fine too"
        );
        assert!(
            endpoint_origin("ftp://push.example/p").is_err(),
            "a non-http scheme has no signable origin"
        );
        assert!(
            endpoint_origin("not a url").is_err(),
            "junk is refused before anything is signed"
        );
    }

    /// A default port is dropped from the origin, a non-default one kept.
    #[test]
    fn endpoint_origin_drops_default_ports_and_keeps_others() {
        assert_eq!(
            endpoint_origin("https://push.example:443/x")
                .as_deref()
                .ok(),
            Some("https://push.example"),
        );
        assert_eq!(
            endpoint_origin("http://p.example:8080/x").as_deref().ok(),
            Some("http://p.example:8080"),
        );
        assert_eq!(
            endpoint_origin("http://p.example:80/x").as_deref().ok(),
            Some("http://p.example"),
        );
    }

    #[test]
    fn an_unparseable_private_key_fails_the_signing_without_leaking() {
        let err = vapid_headers(
            "https://push.example/p",
            "-----BEGIN PRIVATE KEY-----\nQUJD\n-----END PRIVATE KEY-----",
            "public",
            "mailto:a@b.c",
        )
        .expect_err("junk key material cannot sign");
        assert!(
            matches!(err, AppError::Internal(_)),
            "broken stored secrets are a server failure, got {err}"
        );
    }

    #[test]
    fn the_subject_prefers_the_contact_email() {
        let with_email = Config::from_lookup(|key| match key {
            DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
            BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
            CONTACT_EMAIL_VAR => Some("admin@wired.example".to_owned()),
            _ => None,
        })
        .expect("the fixed config parses");
        assert_eq!(
            vapid_subject(&with_email),
            "mailto:admin@wired.example",
            "CONTACT_EMAIL is the mailto subject"
        );
        let without = Config::for_tests();
        assert_eq!(
            vapid_subject(&without),
            TEST_BASE_URL,
            "with no contact configured the instance origin stands in"
        );
    }
}
