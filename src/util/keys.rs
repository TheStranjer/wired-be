//! RSA keypair generation and the PEM/signature helpers built on it.
//!
//! Federation needs RSA keys: an actor's `publicKeyPem` and the private key
//! that signs `draft-cavage` HTTP Signatures over fetches and deliveries
//! (Pleroma stores the instance actor's key in its config, see
//! `config/description.exs` and `lib/pleroma/crypto/rsa/`). This module is
//! the single place that generates, parses and uses those keys, so the
//! formats are pinned to what the wider fediverse accepts:
//!
//! - [`generate_keypair`] makes an RSA-2048 key with the public exponent
//!   65537 and serializes it as PKCS#8 PEM (`-----BEGIN PRIVATE KEY-----`)
//!   and SPKI PEM (`-----BEGIN PUBLIC KEY-----`).
//! - [`parse_private_key`] and [`parse_public_key`] additionally accept the
//!   legacy PKCS#1 PEM labels (`-----BEGIN RSA PRIVATE KEY-----` /
//!   `-----BEGIN RSA PUBLIC KEY-----`) that Mastodon and older Pleroma
//!   versions emit.
//! - [`sign_sha256`] and [`verify_sha256`] compute and check
//!   RSASSA-PKCS1-v1_5 signatures over a SHA-256 digest, the scheme HTTP
//!   Signatures negotiate (`alg=rsa-sha256`).
//!
//! Key generation is CPU-heavy, so callers on the async runtime use
//! [`generate_keypair_async`], which runs it on the blocking pool. The
//! private key is secret material: [`KeyPair`]'s `Debug` impl redacts it and
//! nothing here formats a key into a log or an error message.

use std::fmt;

use rand::rngs::OsRng;
use rsa::{
    RsaPrivateKey, RsaPublicKey,
    pkcs1::{DecodeRsaPrivateKey as _, DecodeRsaPublicKey as _},
    pkcs1v15::{Signature, SigningKey, VerifyingKey},
    pkcs8::{
        DecodePrivateKey as _, DecodePublicKey as _, EncodePrivateKey as _, EncodePublicKey as _,
        LineEnding,
    },
    signature::{SignatureEncoding as _, Signer as _, Verifier as _},
};
use sha2::Sha256;
use tokio::task::spawn_blocking;

use crate::error::{AppError, AppResult};

/// Modulus size of generated keys: 2048 bits, the federation baseline.
const RSA_BITS: usize = 2048;
/// The placeholder [`KeyPair`]'s `Debug` impl prints instead of the private
/// key, so secret material never reaches a log.
const REDACTED: &str = "[redacted]";

/// A second, unrelated key pair in legacy PKCS#1 PEM (OpenSSL-generated,
/// 1024 bits, exponent 65537). It proves PKCS#1 input parses and supplies
/// a "wrong key" without paying for another slow key generation. Test code
/// across the crate signs with it to stay fast.
#[cfg(test)]
pub(crate) const FIXTURE_PRIVATE_PKCS1_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----
MIICXQIBAAKBgQDezaYH6514YK+2mfN5mm+B7L25+Nmwe+SFUZmeugbPx3E4GSqO
UKyVkqjX39d2gkWt0jb4akV0HEF/XzAGe5aXsBmlH8GXs8aau8vy/ndBaGRYGWMJ
jzPxDcjta+/1pOjS4+MgQJsNesXbsoQObQbOGCiwPhXkace6OCyOiByIjwIDAQAB
AoGAQB71MoBXoGZLC4VA8fVFNCIa9COvGsB9IvU5/dlRzoh6iAeOHMkQ8jb0CrSs
ALo7LWw7T8Dd3i2nl+2gF62Hx/QsAfkCybHnXR1F5hRcCLa9ggm0qTRa9G1Igp+b
LQdUxPY3/L3zKIxFYjOLA3IvXyjvTO28WxYoqoJSORdhiGkCQQD/RM5HXYYs7Wdm
cTJdW8mSQHDfZDMl1KGJi1UsEUjN14ao4fdg1hpsTEjwUV7Rw3NEEiKJG1FGLO9w
8yrkrcIDAkEA33EI/0feKD50aD93Sn8etK4J2St+bmtW174Qc0KF4PcLLOOqhilv
4aTuuugI4woc5AsuZaCWm1XVERQSTQg/hQJBAJXxdLm21fnN+c7S7yKnp9+guqx9
zw2JuNlyXrVj8b/3ucG5+fWHzj2PEckYAvp0NGDnxiGDSlpOStet/qQU7MECQE0n
lPnBcT7oQf31XFXHC21MY24wDJpISlZ49Ok34U7LP3qiqrFlAZMjBqRXlyn+xfcg
vUauaxL8UnRxP9iFrpkCQQDT53dVyGiJWdztFzZ/cLQrzBO1zIFd/9qpRnd3d8tD
Wu3YSqmZf+QFmfweXZZ9bHFjo76GP4v6ZaPI8VRYu0Ya
-----END RSA PRIVATE KEY-----
";
/// The public half of [`FIXTURE_PRIVATE_PKCS1_PEM`] in PKCS#1 PEM.
#[cfg(test)]
pub(crate) const FIXTURE_PUBLIC_PKCS1_PEM: &str = "-----BEGIN RSA PUBLIC KEY-----
MIGJAoGBAN7NpgfrnXhgr7aZ83mab4Hsvbn42bB75IVRmZ66Bs/HcTgZKo5QrJWS
qNff13aCRa3SNvhqRXQcQX9fMAZ7lpewGaUfwZezxpq7y/L+d0FoZFgZYwmPM/EN
yO1r7/Wk6NLj4yBAmw16xduyhA5tBs4YKLA+FeRpx7o4LI6IHIiPAgMBAAE=
-----END RSA PUBLIC KEY-----
";

/// An RSA key pair as PEM text: the two forms other instances exchange.
#[derive(Clone)]
pub struct KeyPair {
    /// The private key as PKCS#8 PEM (`-----BEGIN PRIVATE KEY-----`).
    pub private_pem: String,
    /// The public key as SPKI PEM (`-----BEGIN PUBLIC KEY-----`).
    pub public_pem: String,
}

/// `Debug` shows only the public half: the private PEM is replaced with a
/// `[redacted]` placeholder.
impl fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyPair")
            .field("private_pem", &REDACTED)
            .field("public_pem", &self.public_pem)
            .finish()
    }
}

/// Generates a fresh RSA-2048 key pair with the public exponent 65537 from
/// the OS CSPRNG, serialized as PKCS#8/SPKI PEM.
///
/// This is CPU-heavy; inside an async runtime prefer
/// [`generate_keypair_async`].
///
/// # Errors
///
/// [`AppError::Internal`] if the OS RNG fails or the key cannot be
/// generated or PEM-encoded. The detail names only the failure, never any
/// key material.
pub fn generate_keypair() -> AppResult<KeyPair> {
    let mut rng = OsRng;
    let private = RsaPrivateKey::new(&mut rng, RSA_BITS)
        .map_err(|err| AppError::Internal(format!("could not generate an RSA key pair: {err}")))?;
    let public = RsaPublicKey::from(&private);
    let private_pem = private.to_pkcs8_pem(LineEnding::LF).map_err(|err| {
        AppError::Internal(format!("could not PEM-encode the private key: {err}"))
    })?;
    let public_pem = public
        .to_public_key_pem(LineEnding::LF)
        .map_err(|err| AppError::Internal(format!("could not PEM-encode the public key: {err}")))?;
    Ok(KeyPair {
        private_pem: String::clone(&private_pem),
        public_pem,
    })
}

/// [`generate_keypair`] on the blocking pool, so a request handler does not
/// occupy a runtime worker for the duration of the key generation.
///
/// # Errors
///
/// Whatever [`generate_keypair`] returns, plus [`AppError::Internal`] if the
/// blocking task itself is cancelled or panics.
pub async fn generate_keypair_async() -> AppResult<KeyPair> {
    spawn_blocking(generate_keypair)
        .await
        .map_err(|err| AppError::Internal(format!("the RSA key generation task failed: {err}")))?
}

/// Rewrites `pem` into the one shape the strict RFC 7468 parsers accept.
///
/// `\r\n` becomes `\n`, leading and trailing whitespace is trimmed, and
/// exactly one `\n` is appended. Pleroma serves `publicKeyPem` ending in two
/// newlines, which the parsers refuse; Windows line endings and stray
/// padding are repaired the same way. The key material itself is untouched.
#[must_use]
pub fn normalize_pem(pem: &str) -> String {
    let mut normalized = pem.replace("\r\n", "\n").trim().to_owned();
    normalized.push('\n');
    normalized
}

/// Parses a PEM-encoded RSA private key, accepting both PKCS#8
/// (`-----BEGIN PRIVATE KEY-----`) and the legacy PKCS#1
/// (`-----BEGIN RSA PRIVATE KEY-----`) label Mastodon and older Pleroma
/// versions emit.
///
/// # Errors
///
/// [`AppError::BadRequest`] if `pem` parses as neither; the message carries
/// both format-specific causes.
pub fn parse_private_key(pem: &str) -> AppResult<RsaPrivateKey> {
    let pem = normalize_pem(pem);
    match RsaPrivateKey::from_pkcs8_pem(&pem) {
        Ok(key) => Ok(key),
        Err(pkcs8_err) => RsaPrivateKey::from_pkcs1_pem(&pem).map_err(|pkcs1_err| {
            AppError::BadRequest(format!(
                "private key PEM is neither PKCS#8 ({pkcs8_err}) nor PKCS#1 ({pkcs1_err})"
            ))
        }),
    }
}

/// Parses a PEM-encoded RSA public key, accepting both SPKI
/// (`-----BEGIN PUBLIC KEY-----`) and the legacy PKCS#1
/// (`-----BEGIN RSA PUBLIC KEY-----`) label.
///
/// # Errors
///
/// [`AppError::BadRequest`] if `pem` parses as neither; the message carries
/// both format-specific causes.
pub fn parse_public_key(pem: &str) -> AppResult<RsaPublicKey> {
    let pem = normalize_pem(pem);
    match RsaPublicKey::from_public_key_pem(&pem) {
        Ok(key) => Ok(key),
        Err(spki_err) => RsaPublicKey::from_pkcs1_pem(&pem).map_err(|pkcs1_err| {
            AppError::BadRequest(format!(
                "public key PEM is neither SPKI ({spki_err}) nor PKCS#1 ({pkcs1_err})"
            ))
        }),
    }
}

/// Signs `data` with the PEM-encoded private key `private_pem` using
/// RSASSA-PKCS1-v1_5 over a SHA-256 digest, returning the raw signature
/// bytes (HTTP Signatures base64 them into the `signature` parameter).
///
/// # Errors
///
/// [`AppError::BadRequest`] if `private_pem` does not parse (see
/// [`parse_private_key`]), or [`AppError::Internal`] if the signing
/// operation itself fails.
pub fn sign_sha256(private_pem: &str, data: &[u8]) -> AppResult<Vec<u8>> {
    let key = parse_private_key(private_pem)?;
    SigningKey::<Sha256>::new(key)
        .try_sign(data)
        .map(|signature| signature.to_bytes().to_vec())
        .map_err(|err| AppError::Internal(format!("could not sign the data: {err}")))
}

/// Checks an RSASSA-PKCS1-v1_5 SHA-256 `signature` over `data` against the
/// PEM-encoded public key `public_pem`.
///
/// Any failure is `false`, never an error: an unparseable key or signature
/// is the same answer as a mismatch for a caller validating a request, and
/// this keeps untrusted input out of the error path.
#[must_use]
pub fn verify_sha256(public_pem: &str, data: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = parse_public_key(public_pem) else {
        return false;
    };
    let Ok(parsed_signature) = Signature::try_from(signature) else {
        return false;
    };
    VerifyingKey::<Sha256>::new(key)
        .verify(data, &parsed_signature)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use rsa::{BigUint, pkcs1::EncodeRsaPrivateKey as _, traits::PublicKeyParts as _};

    use super::*;

    /// PEM label opening a PKCS#8 private key.
    const PKCS8_PRIVATE_LABEL: &str = "-----BEGIN PRIVATE KEY-----";
    /// PEM label opening a SPKI public key.
    const SPKI_PUBLIC_LABEL: &str = "-----BEGIN PUBLIC KEY-----";
    /// PEM label opening a legacy PKCS#1 private key.
    const PKCS1_PRIVATE_LABEL: &str = "-----BEGIN RSA PRIVATE KEY-----";
    /// PEM label opening a legacy PKCS#1 public key.
    const PKCS1_PUBLIC_LABEL: &str = "-----BEGIN RSA PUBLIC KEY-----";
    /// The exponent [`generate_keypair`] must use.
    const PUBLIC_EXPONENT: u64 = 65537;
    /// Byte width of a signature made by the shared RSA-2048 pair.
    const SIGNATURE_BYTES: usize = 256;
    /// The text every signature in these tests is made over.
    const SIGNED_DATA: &[u8] = b"GET /users/alice HTTP/1.1\nhost: example.com\n";

    /// The module's generated key pair: RSA-2048 generation is slow in debug
    /// builds, so every synchronous test shares this one instance.
    static KEYS: OnceLock<KeyPair> = OnceLock::new();

    /// The shared [`KeyPair`], generated on first use.
    fn keys() -> &'static KeyPair {
        KEYS.get_or_init(|| generate_keypair().unwrap())
    }

    /// The shared key pair's private key re-spelled as legacy PKCS#1 PEM,
    /// made once from the output of [`keys()`].
    fn keys_pkcs1_private() -> &'static str {
        static PKCS1: OnceLock<String> = OnceLock::new();
        PKCS1
            .get_or_init(|| {
                let key = parse_private_key(&keys().private_pem).unwrap();
                String::clone(&key.to_pkcs1_pem(LineEnding::LF).unwrap())
            })
            .as_str()
    }

    #[test]
    fn generated_pems_have_the_expected_headers() {
        let keys = keys();
        assert!(
            keys.private_pem.starts_with(PKCS8_PRIVATE_LABEL),
            "the private key is PKCS#8 PEM"
        );
        assert!(
            keys.private_pem.contains("-----END PRIVATE KEY-----\n"),
            "the private key PEM is terminated"
        );
        assert!(
            keys.public_pem.starts_with(SPKI_PUBLIC_LABEL),
            "the public key is SPKI PEM"
        );
        assert!(
            keys.public_pem.contains("-----END PUBLIC KEY-----\n"),
            "the public key PEM is terminated"
        );
    }

    #[test]
    fn generated_pems_round_trip_through_their_parsers() {
        let keys = keys();
        let private = parse_private_key(&keys.private_pem).unwrap();
        let public = parse_public_key(&keys.public_pem).unwrap();
        assert_eq!(
            private.n(),
            public.n(),
            "both halves parse to the same modulus"
        );
        assert_eq!(private.e(), public.e(), "and the same public exponent");
        assert_eq!(
            public.e(),
            &BigUint::from(PUBLIC_EXPONENT),
            "the public exponent is 65537"
        );
        assert_eq!(
            private.n().bits(),
            RSA_BITS,
            "the generated modulus is RSA-2048"
        );
    }

    #[test]
    fn signature_round_trips() {
        let keys = keys();
        let signature = sign_sha256(&keys.private_pem, SIGNED_DATA).unwrap();
        assert_eq!(
            signature.len(),
            SIGNATURE_BYTES,
            "a PKCS#1 v1.5 signature is as long as the modulus"
        );
        assert!(
            verify_sha256(&keys.public_pem, SIGNED_DATA, &signature),
            "a signature verifies against its own key and data"
        );
    }

    #[test]
    fn tampered_data_fails_to_verify() {
        let keys = keys();
        let signature = sign_sha256(&keys.private_pem, SIGNED_DATA).unwrap();
        let mut tampered = Vec::from(SIGNED_DATA);
        *tampered.first_mut().unwrap() ^= 0xFF;
        assert!(
            !verify_sha256(&keys.public_pem, &tampered, &signature),
            "one flipped byte invalidates the signature"
        );
        assert!(
            !verify_sha256(&keys.public_pem, SIGNED_DATA, b"not a signature"),
            "junk signature bytes never verify"
        );
    }

    #[test]
    fn wrong_key_fails_to_verify() {
        let keys = keys();
        let signature = sign_sha256(&keys.private_pem, SIGNED_DATA).unwrap();
        assert!(
            !verify_sha256(FIXTURE_PUBLIC_PKCS1_PEM, SIGNED_DATA, &signature),
            "the unrelated fixture public key must not accept this signature"
        );
        let fixture_signature = sign_sha256(FIXTURE_PRIVATE_PKCS1_PEM, SIGNED_DATA).unwrap();
        assert!(
            !verify_sha256(&keys.public_pem, SIGNED_DATA, &fixture_signature),
            "and the module key must not accept a signature made with the fixture"
        );
    }

    #[test]
    fn legacy_pkcs1_pems_parse() {
        assert!(
            FIXTURE_PRIVATE_PKCS1_PEM.starts_with(PKCS1_PRIVATE_LABEL),
            "the fixture is really PKCS#1"
        );
        assert!(
            FIXTURE_PUBLIC_PKCS1_PEM.starts_with(PKCS1_PUBLIC_LABEL),
            "the fixture public key is really PKCS#1"
        );
        let openssl_private = parse_private_key(FIXTURE_PRIVATE_PKCS1_PEM).unwrap();
        let openssl_public = parse_public_key(FIXTURE_PUBLIC_PKCS1_PEM).unwrap();
        assert_eq!(
            openssl_private.n(),
            openssl_public.n(),
            "the fixture halves belong together"
        );
        let signature = sign_sha256(FIXTURE_PRIVATE_PKCS1_PEM, SIGNED_DATA).unwrap();
        assert!(
            verify_sha256(FIXTURE_PUBLIC_PKCS1_PEM, SIGNED_DATA, &signature),
            "a signature made with a PKCS#1-parsed key verifies"
        );
    }

    #[test]
    fn pkcs1_spelling_of_the_generated_key_parses() {
        let pkcs1 = keys_pkcs1_private();
        let re_spelled = parse_private_key(pkcs1).unwrap();
        let direct = parse_private_key(&keys().private_pem).unwrap();
        assert_eq!(
            re_spelled.n(),
            direct.n(),
            "the PKCS#1 spelling of the generated key parses to the same modulus"
        );
        let signature = sign_sha256(pkcs1, SIGNED_DATA).unwrap();
        assert!(
            verify_sha256(&keys().public_pem, SIGNED_DATA, &signature),
            "and it signs so the SPKI public key verifies"
        );
    }

    #[test]
    fn normalize_pem_leaves_exactly_one_trailing_newline() {
        let clean = "-----BEGIN PUBLIC KEY-----\nQUJD\n-----END PUBLIC KEY-----\n";
        assert_eq!(normalize_pem(clean), clean, "a clean PEM is unchanged");
        assert_eq!(
            normalize_pem(&format!("{clean}\n")),
            clean,
            "Pleroma's blank trailing line is dropped"
        );
        assert_eq!(
            normalize_pem(&clean.replace('\n', "\r\n")),
            clean,
            "CRLF becomes LF"
        );
        assert_eq!(
            normalize_pem("  \n\t-----BEGIN PUBLIC KEY-----\nQUJD\n-----END PUBLIC KEY-----  \n\n"),
            clean,
            "surrounding whitespace is trimmed"
        );
        assert_eq!(
            normalize_pem("-----BEGIN PUBLIC KEY-----\nQUJD\n-----END PUBLIC KEY-----"),
            clean,
            "a missing final newline is added"
        );
    }

    #[test]
    fn spki_pems_with_odd_whitespace_parse_and_verify() {
        let keys = keys();
        let signature = sign_sha256(&keys.private_pem, SIGNED_DATA).unwrap();
        let blank_lines = format!("{}\n", keys.public_pem);
        let crlf = keys.public_pem.replace('\n', "\r\n");
        let padded = format!("  \n  {}  \n\n", keys.public_pem);
        for (label, pem) in [
            ("trailing blank line", blank_lines),
            ("CRLF line endings", crlf),
            ("leading and trailing spaces", padded),
        ] {
            assert!(parse_public_key(&pem).is_ok(), "{label} parses");
            assert!(
                verify_sha256(&pem, SIGNED_DATA, &signature),
                "{label} still verifies a genuine signature"
            );
        }
    }

    #[test]
    fn pkcs1_pems_with_trailing_blank_lines_parse() {
        let public = format!("{FIXTURE_PUBLIC_PKCS1_PEM}\n\n");
        let private = format!("{FIXTURE_PRIVATE_PKCS1_PEM}\n\n");
        let parsed = parse_public_key(&public).unwrap();
        assert_eq!(
            parsed.n(),
            parse_public_key(FIXTURE_PUBLIC_PKCS1_PEM).unwrap().n(),
            "the same PKCS#1 public key"
        );
        let signature = sign_sha256(&private, SIGNED_DATA).unwrap();
        assert!(
            verify_sha256(&public, SIGNED_DATA, &signature),
            "PKCS#1 with trailing blank lines signs and verifies"
        );
    }

    #[test]
    fn private_pems_with_odd_whitespace_parse() {
        let keys = keys();
        let odd = format!("\n {} \r\n\r\n", keys.private_pem.replace('\n', "\r\n"));
        let parsed = parse_private_key(&odd).unwrap();
        assert_eq!(
            parsed.n(),
            parse_private_key(&keys.private_pem).unwrap().n(),
            "the same private key"
        );
    }

    #[test]
    fn garbage_pems_are_errors() {
        for junk in [
            "",
            "not a key at all",
            "-----BEGIN PRIVATE KEY-----\nnot base64 at all! ?\n-----END PRIVATE KEY-----",
            "-----BEGIN PUBLIC KEY-----\nQUJD\n-----END PUBLIC KEY-----",
            "-----BEGIN CERTIFICATE-----\nQUJD\n-----END CERTIFICATE-----",
        ] {
            assert!(
                parse_private_key(junk).is_err(),
                "a private key must not parse from garbage input"
            );
            assert!(
                parse_public_key(junk).is_err(),
                "a public key must not parse from garbage input"
            );
        }
    }

    #[test]
    fn swapped_key_halves_are_errors() {
        let keys = keys();
        assert!(
            parse_private_key(&keys.public_pem).is_err(),
            "a public key is not a private key"
        );
        assert!(
            parse_public_key(&keys.private_pem).is_err(),
            "a private key is not a public key"
        );
        assert!(
            parse_public_key(FIXTURE_PRIVATE_PKCS1_PEM).is_err(),
            "a PKCS#1 private key is not a public key"
        );
    }

    #[test]
    fn garbage_private_pem_cannot_sign() {
        let outcome = sign_sha256(
            "-----BEGIN PRIVATE KEY-----\nZZZZ\n-----END PRIVATE KEY-----",
            SIGNED_DATA,
        );
        assert!(
            matches!(outcome, Err(AppError::BadRequest(_))),
            "signing with an unparseable key is the parse error, not a panic"
        );
    }

    #[test]
    fn debug_never_shows_the_private_pem() {
        let keys = keys();
        let rendered = format!("{keys:?}");
        assert!(
            !rendered.contains("PRIVATE KEY"),
            "no private-key label or text survives the Debug redaction"
        );
        assert!(
            !rendered.contains(&keys.private_pem),
            "the private PEM itself must not appear"
        );
        assert!(
            rendered.contains(REDACTED),
            "the private field shows the redaction placeholder"
        );
        assert!(
            rendered.contains(SPKI_PUBLIC_LABEL),
            "the public PEM stays visible"
        );
    }

    #[tokio::test]
    async fn async_generation_produces_usable_keys() {
        // The async wrapper runs the same slow generator on the blocking
        // pool; this test pays for one extra generation to prove it.
        let keys = generate_keypair_async().await.unwrap();
        assert!(
            keys.private_pem.starts_with(PKCS8_PRIVATE_LABEL),
            "the blocking-pool variant emits the same PKCS#8 PEM"
        );
        assert!(
            keys.public_pem.starts_with(SPKI_PUBLIC_LABEL),
            "the blocking-pool variant emits the same SPKI PEM"
        );
        let signature = sign_sha256(&keys.private_pem, SIGNED_DATA).unwrap();
        assert!(
            verify_sha256(&keys.public_pem, SIGNED_DATA, &signature),
            "the generated pair signs and verifies"
        );
    }
}
