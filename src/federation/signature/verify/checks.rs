//! The structural checks of a received signature: the algorithm, the signed
//! header list and the signing text rebuilt for each request target.

use axum::http::{HeaderMap, HeaderName};

use super::super::{ALGORITHM, signing_string};
use super::{
    CREATED, DATE_HEADER, DIGEST_HEADER, EXPIRES, HOST_HEADER, HS2019, POST_METHOD, REQUEST_TARGET,
    SignatureError, SignedRequestParts, parse::ParsedSignature,
};

/// The algorithms this module verifies: the outbound [`ALGORITHM`] and
/// `hs2019`, which negotiates the algorithm from the key — for every RSA
/// fediverse key that is the same RSA-SHA256. An absent parameter is the
/// draft default.
pub(super) fn check_algorithm(algorithm: Option<&str>) -> Result<(), SignatureError> {
    if algorithm.is_none_or(|name| {
        name.eq_ignore_ascii_case(ALGORITHM) || name.eq_ignore_ascii_case(HS2019)
    }) {
        return Ok(());
    }
    Err(SignatureError::UnsupportedAlgorithm)
}

/// `(request-target)`, `host` and one of `date`/`(created)` must be
/// signed, and a `POST` the `digest` too.
pub(super) fn check_signed_headers(
    parsed: &ParsedSignature,
    request: &SignedRequestParts<'_>,
) -> Result<(), SignatureError> {
    let signs = |name: &str| parsed.headers.iter().any(|signed| signed == name);
    let digest_needed = request.method.eq_ignore_ascii_case(POST_METHOD) && !signs(DIGEST_HEADER);
    if signs(REQUEST_TARGET)
        && signs(HOST_HEADER)
        && (signs(DATE_HEADER) || signs(CREATED))
        && !digest_needed
    {
        return Ok(());
    }
    Err(SignatureError::Missing)
}

/// The request targets to try, in order: the target as received, then the
/// path without its query (Pleroma signs and verifies some requests
/// query-less).
pub(super) fn target_candidates(path_and_query: &str) -> Vec<&str> {
    match path_and_query.split_once('?') {
        Some((path, _)) => vec![path_and_query, path],
        None => vec![path_and_query],
    }
}

/// The signing text of `request` for the request target `target`: one
/// line per signed header, in the sender's order.
pub(super) fn signing_text(
    parsed: &ParsedSignature,
    request: &SignedRequestParts<'_>,
    target: &str,
) -> Result<String, SignatureError> {
    let mut lines = Vec::with_capacity(parsed.headers.len());
    for name in &parsed.headers {
        let line = signing_line(parsed, request, name, target).ok_or(SignatureError::Missing)?;
        lines.push(line);
    }
    let borrowed = lines
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<(&str, &str)>>();
    Ok(signing_string(&borrowed))
}

/// The `(name, value)` line one signed header contributes, `None` when
/// neither the request nor the signature parameters carry a value for it.
fn signing_line(
    parsed: &ParsedSignature,
    request: &SignedRequestParts<'_>,
    name: &str,
    target: &str,
) -> Option<(String, String)> {
    let value = match name {
        REQUEST_TARGET => Some(format!("{} {target}", request.method.to_lowercase())),
        CREATED => parsed.created.map(|seconds| seconds.to_string()),
        EXPIRES => parsed.expires.map(|seconds| seconds.to_string()),
        other => header_value(&request.headers, other),
    }?;
    Some((name.to_owned(), value))
}

/// Every value of header `name` as one string — multiple ones joined with
/// `", "`, the way Mastodon's verifier reads them. `None` when the name is
/// not a header name, the request carries no such header, or a value is
/// not visible ASCII.
pub(super) fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let Ok(name) = HeaderName::try_from(name) else {
        return None;
    };
    let values = headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<&str>>();
    (!values.is_empty()).then(|| values.join(", "))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::super::{
        parse::parse_signature_header,
        test_support::{
            HELLO_DIGEST, HOST, NOW_FIXDATE, crafted_signature, get_lines, now, received_headers,
            round_trip, verify_now,
        },
    };
    use super::*;

    #[test]
    fn a_signed_get_with_a_query_verifies_on_the_first_candidate() {
        let (parsed, request) = round_trip(
            "GET",
            "https://peer.example/users/alice?page=2&x=1",
            "/users/alice?page=2&x=1",
            None,
            now(),
        );
        assert_eq!(
            verify_now(&parsed, &request),
            Ok(()),
            "the target is signed with its query"
        );
    }

    #[test]
    fn a_signature_made_without_the_query_verifies_by_the_fallback() {
        let lines = get_lines("get /inbox");
        let header = crafted_signature("(request-target) host date", &lines, "");
        let parsed = parse_signature_header(&header).unwrap();
        let request = SignedRequestParts {
            method: "GET",
            path_and_query: "/inbox?page=2",
            headers: received_headers(&header, Some(NOW_FIXDATE), None),
        };
        assert_eq!(
            verify_now(&parsed, &request),
            Ok(()),
            "the query-less spelling is the second candidate"
        );
    }

    #[test]
    fn a_path_matching_neither_candidate_fails_to_verify() {
        let lines = get_lines("get /other");
        let header = crafted_signature("(request-target) host date", &lines, "");
        let parsed = parse_signature_header(&header).unwrap();
        let request = SignedRequestParts {
            method: "GET",
            path_and_query: "/inbox?page=2",
            headers: received_headers(&header, Some(NOW_FIXDATE), None),
        };
        assert_eq!(
            verify_now(&parsed, &request),
            Err(SignatureError::BadSignature),
            "with or without the query the target differs"
        );
    }

    #[test]
    fn signed_headers_can_be_listed_in_any_order() {
        let lines = [
            ("digest", HELLO_DIGEST),
            (REQUEST_TARGET, "post /inbox"),
            (DATE_HEADER, NOW_FIXDATE),
            (HOST_HEADER, HOST),
        ];
        let header = crafted_signature("digest (request-target) date host", &lines, "");
        let parsed = parse_signature_header(&header).unwrap();
        let request = SignedRequestParts {
            method: "POST",
            path_and_query: "/inbox",
            headers: received_headers(&header, Some(NOW_FIXDATE), Some(HELLO_DIGEST)),
        };
        assert_eq!(
            verify_now(&parsed, &request),
            Ok(()),
            "the text is rebuilt in the sender's order, not the canonical one"
        );
    }

    #[test]
    fn a_tampered_header_fails_to_verify() {
        let (parsed, mut request) = round_trip(
            "GET",
            "https://peer.example/users/alice",
            "/users/alice",
            None,
            now(),
        );
        let _host: Option<HeaderValue> = request
            .headers
            .insert("host", HeaderValue::from_static("evil.example"));
        assert_eq!(
            verify_now(&parsed, &request),
            Err(SignatureError::BadSignature),
            "the host is signed"
        );
    }

    #[test]
    fn a_tampered_method_or_path_fails_to_verify() {
        let (parsed, request) = round_trip(
            "GET",
            "https://peer.example/users/alice",
            "/users/alice",
            None,
            now(),
        );
        let other_method = SignedRequestParts {
            method: "DELETE",
            ..request.clone()
        };
        assert_eq!(
            verify_now(&parsed, &other_method),
            Err(SignatureError::BadSignature),
            "the method rides in (request-target)"
        );
        let other_path = SignedRequestParts {
            path_and_query: "/users/bob",
            ..request
        };
        assert_eq!(
            verify_now(&parsed, &other_path),
            Err(SignatureError::BadSignature),
            "and so does the path"
        );
    }

    #[test]
    fn the_mandatory_signed_headers_are_enforced() {
        let no_target = [(HOST_HEADER, HOST), (DATE_HEADER, NOW_FIXDATE)];
        let no_host = [(REQUEST_TARGET, "get /x"), (DATE_HEADER, NOW_FIXDATE)];
        let no_moment = [(REQUEST_TARGET, "get /x"), (HOST_HEADER, HOST)];
        let plain_get = get_lines("get /x");
        for (headers_param, lines, method) in [
            ("host date", no_target.as_slice(), "GET"),
            ("(request-target) date", no_host.as_slice(), "GET"),
            ("(request-target) host", no_moment.as_slice(), "GET"),
            ("(request-target) host date", plain_get.as_slice(), "POST"),
        ] {
            let header = crafted_signature(headers_param, lines, "");
            let parsed = parse_signature_header(&header).unwrap();
            let request = SignedRequestParts {
                method,
                path_and_query: "/x",
                headers: received_headers(&header, Some(NOW_FIXDATE), None),
            };
            assert_eq!(
                verify_now(&parsed, &request),
                Err(SignatureError::Missing),
                "`{headers_param}` signed by a {method} lacks a mandatory header"
            );
        }
    }

    #[test]
    fn other_signature_algorithms_are_unsupported() {
        let lines = get_lines("get /x");
        for algorithm in ["hmac-sha256", "rsa-sha512", "ecdsa-p256-sha256"] {
            let header = crafted_signature(
                "(request-target) host date",
                &lines,
                &format!(r#",algorithm="{algorithm}""#),
            );
            let parsed = parse_signature_header(&header).unwrap();
            let request = SignedRequestParts {
                method: "GET",
                path_and_query: "/x",
                headers: received_headers(&header, Some(NOW_FIXDATE), None),
            };
            assert_eq!(
                verify_now(&parsed, &request),
                Err(SignatureError::UnsupportedAlgorithm),
                "`{algorithm}` is neither rsa-sha256 nor hs2019"
            );
        }
    }

    #[test]
    fn hs2019_mixed_case_and_the_absent_algorithm_verify_as_rsa_sha256() {
        let lines = get_lines("get /x");
        for params in [r#",algorithm="hs2019""#, r#",algorithm="RSA-SHA256""#, ""] {
            let header = crafted_signature("(request-target) host date", &lines, params);
            let parsed = parse_signature_header(&header).unwrap();
            let request = SignedRequestParts {
                method: "GET",
                path_and_query: "/x",
                headers: received_headers(&header, Some(NOW_FIXDATE), None),
            };
            assert_eq!(
                verify_now(&parsed, &request),
                Ok(()),
                "algorithm parameter `{params:?}` accepts the RSA-SHA256 text"
            );
        }
    }
}
