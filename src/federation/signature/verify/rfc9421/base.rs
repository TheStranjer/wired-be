//! The RFC 9421 signature base: the text a signature is computed over.
//!
//! One `"component": value` line per covered component, in the order the
//! `Signature-Input` member lists them, then the `"@signature-params"` line,
//! joined with `\n` and without a trailing newline (RFC 9421 section 2.5).

use super::super::{SignatureError, SignedRequestParts, checks::header_value};
use super::parse::Input;

/// Pseudo component naming the signature parameters; it is always the last
/// line and never a covered component.
const SIGNATURE_PARAMS: &str = "@signature-params";
/// [`SignatureError::Malformed`] for a component this verifier cannot read.
const UNSUPPORTED_COMPONENT: SignatureError =
    SignatureError::Malformed("an unsupported covered component");
/// [`SignatureError::Malformed`] for a header component that is not lowercase.
const MALFORMED_COMPONENT: SignatureError =
    SignatureError::Malformed("a covered header name that is not lowercase");

/// Rebuilds the signature base of `input` from the received `request`.
///
/// `scheme` is the scheme the request was made with (`https`), which a
/// received request does not carry; the authority is the `Host` header.
///
/// # Errors
///
/// [`SignatureError::Missing`] when a covered header (or the `Host` the
/// authority is read from) is absent, [`SignatureError::Malformed`] for a
/// component name this verifier does not support.
pub(super) fn signature_base(
    input: &Input,
    request: &SignedRequestParts<'_>,
    scheme: &str,
) -> Result<String, SignatureError> {
    let mut lines = Vec::with_capacity(input.components.len() + 1);
    for name in &input.components {
        let value = component_value(name, request, scheme)?;
        lines.push(format!("\"{name}\": {value}"));
    }
    lines.push(format!("\"{SIGNATURE_PARAMS}\": {}", input.params_text()));
    Ok(lines.join("\n"))
}

/// The value of one covered component of `request`.
fn component_value(
    name: &str,
    request: &SignedRequestParts<'_>,
    scheme: &str,
) -> Result<String, SignatureError> {
    if name.starts_with('@') {
        return derived_value(name, request, scheme);
    }
    if name.chars().any(|c| c.is_ascii_uppercase()) {
        return Err(MALFORMED_COMPONENT);
    }
    header_value(&request.headers, name).ok_or(SignatureError::Missing)
}

/// The value of derived component `name` (RFC 9421 section 2.2).
fn derived_value(
    name: &str,
    request: &SignedRequestParts<'_>,
    scheme: &str,
) -> Result<String, SignatureError> {
    let target = request.path_and_query;
    let (path, query) = target
        .split_once('?')
        .map_or((target, None), |(path, query)| (path, Some(query)));
    match name {
        "@method" => Ok(request.method.to_ascii_uppercase()),
        "@scheme" => Ok(scheme.to_ascii_lowercase()),
        "@authority" => authority(request, scheme),
        "@target-uri" => Ok(format!(
            "{}://{}{target}",
            scheme.to_ascii_lowercase(),
            authority(request, scheme)?
        )),
        "@request-target" => Ok(target.to_owned()),
        "@path" if path.is_empty() => Ok("/".to_owned()),
        "@path" => Ok(path.to_owned()),
        "@query" => Ok(format!("?{}", query.unwrap_or_default())),
        _ => Err(UNSUPPORTED_COMPONENT),
    }
}

/// The `@authority` value: the `Host` header lowercased, without the port
/// when it is the default one of `scheme`.
fn authority(request: &SignedRequestParts<'_>, scheme: &str) -> Result<String, SignatureError> {
    let host = header_value(&request.headers, "host")
        .ok_or(SignatureError::Missing)?
        .to_ascii_lowercase();
    let default_port = if scheme.eq_ignore_ascii_case("http") {
        ":80"
    } else {
        ":443"
    };
    Ok(host.strip_suffix(default_port).unwrap_or(&host).to_owned())
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::super::parse::parse_signature_input;
    use super::*;

    /// A request for `path` at `host` with a repeated `x-a` header.
    fn received<'a>(path: &'a str, host: &str) -> SignedRequestParts<'a> {
        let mut headers = HeaderMap::new();
        let previous = headers.insert("host", HeaderValue::from_str(host).unwrap());
        drop(previous);
        assert!(
            !headers.append("x-a", HeaderValue::from_static("1")),
            "the first x-a is a new header"
        );
        assert!(
            headers.append("x-a", HeaderValue::from_static("2")),
            "the second x-a joins the first"
        );
        SignedRequestParts {
            method: "post",
            path_and_query: path,
            headers,
        }
    }

    /// The base of the one member of `input` over `request`.
    fn base_of(input: &str, request: &SignedRequestParts<'_>) -> Result<String, SignatureError> {
        let parsed = parse_signature_input(input)?;
        let member = parsed.first().ok_or(SignatureError::Missing)?;
        signature_base(member, request, "HTTPS")
    }

    #[test]
    fn every_supported_component_has_its_rfc_9421_value() {
        let request = received("/a/b?x=1&y=2", "Example.COM:443");
        let all = r#"s=("@method" "@scheme" "@authority" "@path" "@query" "@request-target" "@target-uri" "x-a");created=1"#;
        assert_eq!(
            base_of(all, &request).unwrap(),
            "\"@method\": POST\n\
             \"@scheme\": https\n\
             \"@authority\": example.com\n\
             \"@path\": /a/b\n\
             \"@query\": ?x=1&y=2\n\
             \"@request-target\": /a/b?x=1&y=2\n\
             \"@target-uri\": https://example.com/a/b?x=1&y=2\n\
             \"x-a\": 1, 2\n\
             \"@signature-params\": (\"@method\" \"@scheme\" \"@authority\" \"@path\" \"@query\" \"@request-target\" \"@target-uri\" \"x-a\");created=1",
            "the method uppercased, the authority lowercased without its default \
             port, repeated headers joined, the params line last"
        );
    }

    #[test]
    fn an_empty_path_and_query_read_as_the_rfc_says() {
        let request = received("", "example.com:8443");
        let base = base_of(r#"s=("@path" "@query" "@authority");created=1"#, &request).unwrap();
        assert!(
            base.starts_with("\"@path\": /\n\"@query\": ?\n\"@authority\": example.com:8443\n"),
            "an empty path is `/`, an absent query `?`, a custom port is kept: {base}"
        );
    }

    #[test]
    fn components_this_verifier_cannot_read_are_refused() {
        let request = received("/", "example.com");
        for component in [r#""@query-param""#, r#""@signature-params""#, r#""X-A""#] {
            let input = format!("s=({component});created=1");
            assert!(
                matches!(base_of(&input, &request), Err(SignatureError::Malformed(_))),
                "{component} is unsupported"
            );
        }
        let absent = base_of(r#"s=("x-absent");created=1"#, &request);
        assert_eq!(absent, Err(SignatureError::Missing), "an absent header");
    }
}
