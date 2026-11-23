//! The freshness checks of a received signature: the signed moment, its
//! tolerance and the `(expires)` parameter.

use chrono::{DateTime, NaiveDateTime, Utc};

use super::super::IMF_FIXDATE;
use super::{
    CLOCK_SKEW_MARGIN, DATE_HEADER, DATE_TOLERANCE, MALFORMED_DATE, MALFORMED_TIMESTAMP,
    SignatureError, SignedRequestParts, checks::header_value, parse::ParsedSignature,
};

/// A past `(expires)` parameter is [`SignatureError::Expired`], and the
/// signed moment — the `Date` header when it is signed, else the `created`
/// parameter — must be at most [`DATE_TOLERANCE`] before `now` and at most
/// [`CLOCK_SKEW_MARGIN`] after it.
pub(super) fn check_freshness(
    parsed: &ParsedSignature,
    request: &SignedRequestParts<'_>,
    now: DateTime<Utc>,
) -> Result<(), SignatureError> {
    if parsed
        .expires
        .is_some_and(|expires| expires <= now.timestamp())
    {
        return Err(SignatureError::Expired);
    }
    let signed_at = signed_moment(parsed, request)?;
    if signed_at > now + CLOCK_SKEW_MARGIN || now - signed_at > DATE_TOLERANCE {
        return Err(SignatureError::Expired);
    }
    Ok(())
}

/// The instant the signature claims freshness with: the `Date` header when
/// it is among the signed headers, else the `(created)` parameter. One of
/// the two is in the list [`check_signed_headers`] accepted.
fn signed_moment(
    parsed: &ParsedSignature,
    request: &SignedRequestParts<'_>,
) -> Result<DateTime<Utc>, SignatureError> {
    if !parsed.headers.iter().any(|signed| signed == DATE_HEADER) {
        return created_moment(parsed);
    }
    let text = header_value(&request.headers, DATE_HEADER).ok_or(SignatureError::Missing)?;
    parse_date_text(&text).ok_or(MALFORMED_DATE)
}

/// The `(created)` parameter as a timestamp; only reached where the signed
/// header list promised it, so its absence is malformed.
fn created_moment(parsed: &ParsedSignature) -> Result<DateTime<Utc>, SignatureError> {
    let created = parsed.created.ok_or(MALFORMED_TIMESTAMP)?;
    DateTime::from_timestamp(created, 0).ok_or(MALFORMED_TIMESTAMP)
}

/// The IMF-fixdate ([`super`]'s outbound `Date` spelling) as a timestamp.
fn parse_date_text(text: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(text, IMF_FIXDATE)
        .ok()
        .map(|naive| naive.and_utc())
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, header::DATE};
    use chrono::TimeDelta;

    use super::super::{
        CREATED, HOST_HEADER, REQUEST_TARGET,
        parse::parse_signature_header,
        test_support::{
            HOST, NOW_FIXDATE, NOW_SECONDS, crafted_signature, get_lines, now, received_headers,
            round_trip, verify_now,
        },
    };
    use super::*;

    /// The verdict for a signature dated `offset` from [`now`].
    fn verdict_at(offset: TimeDelta) -> Result<(), SignatureError> {
        let (parsed, request) = round_trip(
            "GET",
            "https://peer.example/users/alice",
            "/users/alice",
            None,
            now() + offset,
        );
        verify_now(&parsed, &request)
    }

    #[test]
    fn a_signature_two_hours_ahead_is_expired() {
        assert_eq!(
            verdict_at(TimeDelta::hours(2)),
            Err(SignatureError::Expired),
            "future dates get only the clock-skew margin"
        );
        assert_eq!(
            verdict_at(TimeDelta::hours(12)),
            Err(SignatureError::Expired),
            "the past window does not apply to the future"
        );
    }

    #[test]
    fn a_signature_thirty_minutes_ahead_passes() {
        for offset in [
            TimeDelta::minutes(30),
            TimeDelta::hours(1),
            TimeDelta::zero(),
        ] {
            assert_eq!(
                verdict_at(offset),
                Ok(()),
                "{offset} ahead is inside the skew margin"
            );
        }
    }

    #[test]
    fn a_signature_eleven_hours_old_passes() {
        assert_eq!(verdict_at(-TimeDelta::hours(11)), Ok(()));
        assert_eq!(verdict_at(-TimeDelta::hours(12)), Ok(()), "the edge");
    }

    #[test]
    fn a_signature_thirteen_hours_old_is_expired() {
        assert_eq!(
            verdict_at(-TimeDelta::hours(13)),
            Err(SignatureError::Expired)
        );
    }

    #[test]
    fn a_signed_date_header_that_is_absent_or_unparsable_fails() {
        let lines = get_lines("get /x");
        let header = crafted_signature("(request-target) host date", &lines, "");
        let parsed = parse_signature_header(&header).unwrap();
        let no_date = SignedRequestParts {
            method: "GET",
            path_and_query: "/x",
            headers: received_headers(&header, None, None),
        };
        assert_eq!(
            verify_now(&parsed, &no_date),
            Err(SignatureError::Missing),
            "a signed Date header must be in the request"
        );
        let mut junk = received_headers(&header, Some(NOW_FIXDATE), None);
        let _junk: Option<HeaderValue> = junk.insert(DATE, HeaderValue::from_static("yesterday"));
        let bad_date = SignedRequestParts {
            method: "GET",
            path_and_query: "/x",
            headers: junk,
        };
        assert_eq!(
            verify_now(&parsed, &bad_date),
            Err(MALFORMED_DATE),
            "and must parse as a timestamp"
        );
    }

    #[test]
    fn a_created_pseudo_header_stands_in_for_the_date() {
        let created_text = NOW_SECONDS.to_string();
        let lines = [
            (REQUEST_TARGET, "get /x"),
            (HOST_HEADER, HOST),
            (CREATED, created_text.as_str()),
        ];
        let header = crafted_signature(
            "(request-target) host (created)",
            &lines,
            &format!(",created={NOW_SECONDS}"),
        );
        let parsed = parse_signature_header(&header).unwrap();
        assert_eq!(parsed.created, Some(NOW_SECONDS), "the parameter parses");
        let request = SignedRequestParts {
            method: "GET",
            path_and_query: "/x",
            headers: received_headers(&header, None, None),
        };
        assert_eq!(
            verify_now(&parsed, &request),
            Ok(()),
            "(created) carries both freshness and a signed line"
        );
        let stale = ParsedSignature {
            created: Some((now() - TimeDelta::hours(13)).timestamp()),
            ..parsed
        };
        assert_eq!(
            verify_now(&stale, &request),
            Err(SignatureError::Expired),
            "a stale (created) is outside the window"
        );
    }

    #[test]
    fn a_past_expires_parameter_is_expired_a_future_one_passes() {
        let lines = get_lines("get /x");
        for (offset, expected) in [(-3600_i64, Err(SignatureError::Expired)), (3600, Ok(()))] {
            let header = crafted_signature(
                "(request-target) host date",
                &lines,
                &format!(
                    ",created={},expires={}",
                    NOW_SECONDS - 60,
                    NOW_SECONDS + offset
                ),
            );
            let parsed = parse_signature_header(&header).unwrap();
            let request = SignedRequestParts {
                method: "GET",
                path_and_query: "/x",
                headers: received_headers(&header, Some(NOW_FIXDATE), None),
            };
            assert_eq!(
                verify_now(&parsed, &request),
                expected,
                "expires at now{offset:+} seconds"
            );
        }
    }
}
