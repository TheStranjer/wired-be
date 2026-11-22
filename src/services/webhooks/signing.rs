//! Delivery signing and answer handling: the HMAC signature, the event
//! envelope and how an endpoint's answer sorts into complete, discard or
//! retry.

use hmac::{Hmac, KeyInit as _, Mac as _};
use serde_json::json;
use sha2_011::Sha256;

use crate::{http_client::HttpError, jobs::JobError, util::token::to_hex};

use super::DeliverArgs;

/// The HMAC flavour a delivery signature uses: HMAC-SHA256 over
/// `"{timestamp}.{body}"`, as Mastodon's webhook signing spells it.
pub(super) type HmacSha256 = Hmac<Sha256>;

/// Prefix of the `X-Wired-Signature` value, which carries the algorithm
/// beside the digest (Mastodon's `sha256=<hex>` spelling).
pub(super) const SIGNATURE_PREFIX: &str = "sha256=";
/// The lower bound of the success range a delivery is complete on.
const SUCCESS_STATUS: u16 = 200;
/// The first status above the success range.
const STATUS_CEILING: u16 = 300;
/// The first client-error status.
const CLIENT_ERROR: u16 = 400;
/// The first server-error status, and the end of the discard range: a `4xx`
/// is the endpoint refusing the event, and only the two codes below mean
/// anything else.
const SERVER_ERROR: u16 = 500;
/// Request timeout — the endpoint is not ready yet, so try again.
const STATUS_REQUEST_TIMEOUT: u16 = 408;
/// Too Many Requests — the endpoint is throttling us, so try again.
const STATUS_TOO_MANY: u16 = 429;

/// The event envelope the ticket pins: the event's id and type, the moment of
/// the attempt in decision 5's format (an RFC 3339 instant), and the payload
/// as it was queued. The returned text is what both the signature covers and
/// the body bytes are made of, so a receiver can recompute the digest from
/// exactly what it received.
pub(super) fn event_body(args: &DeliverArgs, created_at: &str) -> String {
    json!({
        "id": args.event_id.to_string(),
        "type": args.event,
        "created_at": created_at,
        "data": args.data,
    })
    .to_string()
}

/// The `X-Wired-Signature` value: `sha256=<hex>` of the HMAC-SHA256 of
/// `"{timestamp}.{body}"` keyed by `secret` (Pleroma signs the bare body with
/// its `X-Hub-Signature`; the ticket pins the Mastodon timestamped form).
pub(super) fn signature(secret: &str, timestamp: i64, body: &str) -> Option<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(format!("{timestamp}.{body}").as_bytes());
    let digest = mac.finalize().into_bytes();
    Some(format!("{SIGNATURE_PREFIX}{}", to_hex(&digest)))
}

/// Sorts one endpoint's answer into completed, discarded or retried: `2xx`
/// landed, a `4xx` that is neither `408` nor `429` is a refusal no retry
/// changes, and everything else — `3xx`, `5xx`, a throttle, a timeout — the
/// queue's to try again.
pub(super) fn classify(status: u16) -> Result<(), JobError> {
    if (SUCCESS_STATUS..STATUS_CEILING).contains(&status) {
        return Ok(());
    }
    if matches!(status, STATUS_REQUEST_TIMEOUT | STATUS_TOO_MANY) {
        return Err(JobError::Retry(format!(
            "the webhook endpoint asked to wait ({status})"
        )));
    }
    if (CLIENT_ERROR..SERVER_ERROR).contains(&status) {
        return Err(JobError::Discard(format!(
            "the webhook endpoint refused the delivery ({status})"
        )));
    }
    Err(JobError::Retry(format!(
        "the webhook endpoint answered {status}"
    )))
}

/// A transport failure in job terms: a target this instance will never reach
/// discards, anything else (timeout, closed connection, oversized answer, an
/// answer the caller did not ask to treat as an error) is retried.
pub(super) fn http_failure(err: &HttpError) -> JobError {
    let detail = err.to_string();
    match err {
        HttpError::InvalidUrl | HttpError::Blocked(_) => JobError::Discard(detail),
        HttpError::Timeout | HttpError::TooLarge | HttpError::Connect(_) | HttpError::Status(_) => {
            JobError::Retry(detail)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::super::{
        EVENT_ACCOUNT_CREATED,
        test_support::{discarded, expect_signature, fresh_id, retried},
    };
    use super::*;

    #[test]
    fn the_hmac_hex_of_a_known_vector_is_lowercase_and_full_width() {
        // RFC 4231 case 2: HMAC-SHA256(key "Jefe", data "what do ya want for
        // nothing?"), pinned here so `hex_lower` and the digest are checked
        // against a published value rather than against themselves.
        let mut mac = HmacSha256::new_from_slice(b"Jefe").expect("a key length");
        mac.update(b"what do ya want for nothing?");
        let hex = to_hex(&mac.finalize().into_bytes());
        assert_eq!(
            hex, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            "the published digest, lowercase hex"
        );
        assert_eq!(hex.len(), 64, "a SHA-256 digest is 32 bytes");
    }

    #[test]
    fn the_signature_covers_the_timestamp_and_the_body_and_says_sha256() {
        let body = r#"{"id":"1","type":"report.created"}"#;
        let signed =
            signature("topsecret", 1_700_000_000, body).expect("a secret of any length signs");
        assert_eq!(
            signed,
            expect_signature("topsecret", "1700000000", body.as_bytes()),
            "the delivered digest is the receiver's recomputation of timestamp dot body"
        );
        assert!(
            signed.starts_with(SIGNATURE_PREFIX),
            "the header names the algorithm: {signed}"
        );
        assert_ne!(
            signed,
            signature("topsecret", 1_700_000_001, body).expect("signs"),
            "a different timestamp signs differently"
        );
        assert_ne!(
            signed,
            signature("othersecret", 1_700_000_000, body).expect("signs"),
            "so does a different secret"
        );
    }

    #[test]
    fn the_envelope_is_the_pinned_document() {
        let event_id = fresh_id();
        let args = DeliverArgs {
            webhook_id: fresh_id(),
            event: EVENT_ACCOUNT_CREATED.to_owned(),
            event_id,
            data: json!({"account": {"id": event_id.to_string()}}),
        };
        let body = event_body(&args, "2024-01-31T12:34:56.789Z");
        let parsed: Value = serde_json::from_str(&body).expect("the envelope is JSON");
        assert_eq!(parsed["id"], event_id.to_string(), "the event id");
        assert_eq!(parsed["type"], EVENT_ACCOUNT_CREATED, "the event type");
        assert_eq!(
            parsed["created_at"], "2024-01-31T12:34:56.789Z",
            "the RFC 3339 stamp of the attempt"
        );
        assert_eq!(parsed["data"], args.data, "and the payload verbatim");
        assert_eq!(
            parsed.as_object().expect("an object").len(),
            4,
            "exactly the four keys the ticket pins"
        );
    }

    #[test]
    fn classification_sorts_the_endpoints_answers() {
        for status in [200_u16, 201, 204, 299] {
            assert_eq!(classify(status), Ok(()), "2xx ({status}) completed");
        }
        for status in [400_u16, 401, 403, 404, 410, 422, 499] {
            let detail = discarded(classify(status), &status.to_string());
            assert!(
                detail.contains(&status.to_string()),
                "the discard names the refusal: {detail}"
            );
        }
        for status in [301_u16, 408, 429, 500, 502, 503] {
            let detail = retried(classify(status), &status.to_string());
            assert!(
                detail.contains(&status.to_string()),
                "the retry names the answer: {detail}"
            );
        }
    }

    #[test]
    fn an_unreachable_target_discards_and_a_broken_one_retries() {
        for err in [
            HttpError::InvalidUrl,
            HttpError::Blocked("private address".to_owned()),
        ] {
            let detail = discarded(Err(http_failure(&err)), &err.to_string());
            assert!(!detail.is_empty(), "the discard states the refusal");
        }
        for err in [
            HttpError::Timeout,
            HttpError::Connect("refused".to_owned()),
            HttpError::TooLarge,
        ] {
            let detail = retried(Err(http_failure(&err)), &err.to_string());
            assert!(!detail.is_empty(), "the retry states the failure");
        }
    }
}
