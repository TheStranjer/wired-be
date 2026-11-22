//! Parsing a received `Signature` header into its parameters.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

#[cfg(doc)]
use super::verify;
use super::{
    MALFORMED_BASE64, MALFORMED_NAME, MALFORMED_PAIR, MALFORMED_QUOTE, MALFORMED_TIMESTAMP, SCHEME,
    SignatureError,
};

/// The parameters of one received `Signature` (or `Authorization:
/// Signature`) header value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedSignature {
    /// The `keyId`/`key_id` parameter: which key claims to have signed.
    pub key_id: String,
    /// The `algorithm` parameter as sent, lowercased; `None` when absent,
    /// which [`verify`] reads as the draft default `rsa-sha256`.
    pub algorithm: Option<String>,
    /// The `headers` parameter split on whitespace, lowercased, in the
    /// signing order the sender used.
    pub headers: Vec<String>,
    /// The decoded `signature` bytes.
    pub signature: Vec<u8>,
    /// The `created` parameter as unix seconds, when the sender sent one.
    pub created: Option<i64>,
    /// The `expires` parameter as unix seconds, when the sender sent one.
    pub expires: Option<i64>,
}

/// Reads one `Signature` header value into its parameters — and the same
/// parameters carried in an `Authorization: Signature ...` header.
///
/// Parameters are accepted in any order and separated by optional spaces
/// around the commas; values are quoted (`keyId="..."`) or bare
/// (`created=1717`), and both the `keyId` and `key_id` spellings are read.
/// Unknown parameters are ignored. `keyId`, `headers` and `signature` are
/// required.
///
/// # Errors
///
/// [`SignatureError::Missing`] if a required parameter is absent,
/// [`SignatureError::Malformed`] if any parameter is unparsable.
pub fn parse_signature_header(value: &str) -> Result<ParsedSignature, SignatureError> {
    let params = scan_parameters(strip_scheme(value))?;
    let headers = parameter(&params, "headers").ok_or(SignatureError::Missing)?;
    let signature = parameter(&params, "signature").ok_or(SignatureError::Missing)?;
    Ok(ParsedSignature {
        key_id: parameter(&params, "keyid")
            .ok_or(SignatureError::Missing)?
            .to_owned(),
        algorithm: parameter(&params, "algorithm").map(str::to_lowercase),
        headers: headers.split_whitespace().map(str::to_lowercase).collect(),
        signature: BASE64.decode(signature).map_err(|_err| MALFORMED_BASE64)?,
        created: unix_seconds(parameter(&params, "created"))?,
        expires: unix_seconds(parameter(&params, "expires"))?,
    })
}

/// Drops a `Signature ` scheme prefix (case-insensitive), which the
/// `Authorization` form carries before the parameters.
fn strip_scheme(value: &str) -> &str {
    let trimmed = value.trim_start();
    match trimmed.split_once([' ', '\t']) {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case(SCHEME) => rest,
        Some(_) | None => trimmed,
    }
}

/// One `Signature` parameter as a borrowed `(name, value)` pair.
type Param<'a> = (&'a str, &'a str);

/// Splits a parameter list into `(name, value)` pairs, unquoting quoted
/// values and trimming bare ones; a comma inside a quoted value (legal in
/// a `keyId` URL) never splits.
fn scan_parameters(value: &str) -> Result<Vec<Param<'_>>, SignatureError> {
    let mut params = Vec::new();
    let mut rest = value;
    loop {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        if rest.is_empty() {
            break;
        }
        let (name, after) = rest.split_once('=').ok_or(MALFORMED_PAIR)?;
        let name = name.trim();
        if !is_token(name) {
            return Err(MALFORMED_NAME);
        }
        let (raw, tail) = scan_value(after)?;
        params.push((name, raw.trim()));
        rest = tail;
    }
    Ok(params)
}

/// The value part of one parameter plus the header text following it:
/// through the closing quote when quoted, up to the next comma when bare.
fn scan_value(after: &str) -> Result<Param<'_>, SignatureError> {
    if let Some(quoted) = after.strip_prefix('"') {
        return quoted.split_once('"').ok_or(MALFORMED_QUOTE);
    }
    Ok(after.split_once(',').unwrap_or((after, "")))
}

/// Whether `name` is a legal parameter name: a nonempty token.
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// A parameter name for lookup: lowercased, and `key_id`'s underscore
/// folded away so it reads the same as `keyId`.
fn normalize_name(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "")
}

/// The value of the parameter whose normalized name is `name`; a
/// duplicated parameter keeps its first spelling.
fn parameter<'a>(params: &[Param<'a>], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(key, _)| normalize_name(key) == name)
        .map(|(_, value)| *value)
}

/// An optional `created`/`expires` parameter as unix seconds.
fn unix_seconds(value: Option<&str>) -> Result<Option<i64>, SignatureError> {
    value
        .map(|text| text.parse::<i64>().map_err(|_err| MALFORMED_TIMESTAMP))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{HI_B64, KEY_ID, NOW_SECONDS, fixture_key, now};
    use super::*;
    use crate::federation::signature::sign_request;

    #[test]
    fn parses_quoted_parameters_in_any_order_with_optional_spaces() {
        let value = format!(
            r#"Signature created="{NOW_SECONDS}" , vendor="ignored, with a comma" ,, expires = {} , headers="(request-target) host date" , algorithm="hs2019",key_id="{KEY_ID}",signature="{HI_B64}""#,
            NOW_SECONDS + 3600,
        );
        let parsed = parse_signature_header(&value).unwrap();
        assert_eq!(
            parsed.key_id, KEY_ID,
            "the `key_id` spelling reads as `keyId`"
        );
        assert_eq!(
            parsed.algorithm.as_deref(),
            Some("hs2019"),
            "a padded algorithm parameter"
        );
        assert_eq!(
            parsed.headers,
            ["(request-target)", "host", "date"].map(str::to_owned),
            "the header list keeps its order"
        );
        assert_eq!(
            parsed.signature,
            b"hi".as_slice(),
            "the quoted signature decodes"
        );
        assert_eq!(parsed.created, Some(NOW_SECONDS), "the quoted timestamp");
        assert_eq!(
            parsed.expires,
            Some(NOW_SECONDS + 3600),
            "the padded bare timestamp"
        );
    }

    #[test]
    fn bare_parameters_are_read_too() {
        let parsed =
            parse_signature_header(&format!("keyId={KEY_ID},headers=date,signature={HI_B64}"))
                .unwrap();
        assert_eq!(parsed.key_id, KEY_ID, "an unquoted key id");
        assert_eq!(parsed.algorithm, None, "no algorithm is the draft default");
        assert_eq!(
            parsed.headers,
            ["date"].map(str::to_owned),
            "an unquoted single header"
        );
        assert_eq!(parsed.signature, b"hi".as_slice(), "an unquoted signature");
    }

    #[test]
    fn the_authorization_spelling_parses_like_the_bare_signature_header() {
        let signed = sign_request(
            "GET",
            "https://peer.example/users/alice",
            None,
            &fixture_key(),
            now(),
        )
        .unwrap();
        let bare = parse_signature_header(&signed.signature).unwrap();
        let prefixed = parse_signature_header(&format!("Signature {}", signed.signature)).unwrap();
        assert_eq!(
            prefixed, bare,
            "the scheme prefix is dropped, nothing else changes"
        );
    }

    #[test]
    fn malformed_signature_headers_are_rejected() {
        for (header, expected) in [
            (String::new(), SignatureError::Missing),
            ("garbage".to_owned(), MALFORMED_PAIR),
            (
                format!(r#"keyId="a",signature="{HI_B64}""#),
                SignatureError::Missing,
            ),
            (r#"keyId="unclosed"#.to_owned(), MALFORMED_QUOTE),
            (
                format!(r#"keyId="a",bad name="x",headers="b",signature="{HI_B64}""#),
                MALFORMED_NAME,
            ),
            (
                r#"keyId="a",headers="b",signature="not base64!""#.to_owned(),
                MALFORMED_BASE64,
            ),
            (
                format!(r#"keyId="a",headers="b",signature="{HI_B64}",created="soon""#),
                MALFORMED_TIMESTAMP,
            ),
        ] {
            assert_eq!(
                parse_signature_header(&header).err(),
                Some(expected),
                "`{header}` must not parse"
            );
        }
    }
}
