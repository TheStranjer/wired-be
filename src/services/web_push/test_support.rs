//! The fixtures the `web_push` module's unit tests share: the RFC 8291
//! Appendix A vectors and the key helpers that write and read an
//! `aes128gcm` body. Test-only.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::{
    SecretKey,
    elliptic_curve::{Generate as _, sec1::ToSec1Point as _},
};
use rand::RngCore as _;

use super::crypto::AUTH_SECRET_LEN;

/// RFC 8291 Appendix A: the header salt.
pub(super) const RFC_SALT_B64: &str = "DGv6ra1nlYgDCS1FRnbzlw";
/// RFC 8291 Appendix A: the application server's private scalar.
pub(super) const RFC_AS_PRIVATE_B64: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
/// RFC 8291 Appendix A: the application server's public point.
pub(super) const RFC_AS_PUBLIC_B64: &str = "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml\
    mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
/// RFC 8291 Appendix A: the user agent's public point.
pub(super) const RFC_UA_PUBLIC_B64: &str = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx\
    aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
/// RFC 8291 Appendix A: the user agent's private scalar.
pub(super) const RFC_UA_PRIVATE_B64: &str = "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94";
/// RFC 8291 Appendix A: the authentication secret.
pub(super) const RFC_AUTH_B64: &str = "BTBZMqHH6r4Tts7J_aSIgg";
/// RFC 8291 Appendix A: the ECDH shared secret.
pub(super) const RFC_ECDH_SECRET_B64: &str = "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs";
/// RFC 8291 Appendix A: the combined input keying material.
pub(super) const RFC_IKM_B64: &str = "S4lYMb_L0FxCeq0WhDx813KgSYqU26kOyzWUdsXYyrg";
/// RFC 8291 Appendix A: the content encryption key.
pub(super) const RFC_CEK_B64: &str = "oIhVW04MRdy2XN9CiKLxTg";
/// RFC 8291 Appendix A: the record nonce.
pub(super) const RFC_NONCE_B64: &str = "4h_95klXJ5E_qnoN";
/// RFC 8291 §5: the example plaintext.
pub(super) const RFC_PLAINTEXT: &[u8] = b"When I grow up, I want to be a watermelon";
/// RFC 8291 §5: the whole encrypted body (header and record) as sent.
pub(super) const RFC_BODY_B64: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7\
    VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95b\
    Qpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

/// Decodes a URL-safe base64 literal, test shorthand.
pub(super) fn b64(text: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD
        .decode(text)
        .expect("the vector text is base64url")
}

/// A fresh client key pair as the subscription stores it plus the
/// private scalar the test decrypts with: `(p256dh, auth, private)`,
/// all URL-safe base64.
pub(super) fn client_keys() -> (String, String, String) {
    let secret = SecretKey::try_generate().expect("a fresh P-256 client key");
    let mut auth = [0_u8; AUTH_SECRET_LEN];
    rand::rngs::OsRng.fill_bytes(&mut auth);
    (
        URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes()),
        URL_SAFE_NO_PAD.encode(auth),
        URL_SAFE_NO_PAD.encode(secret.to_bytes()),
    )
}
