//! [`HttpError`]: how an outbound HTTP request can fail, with its display
//! text and the mapping of `reqwest` transport outcomes — including the
//! [`ssrf`](super::ssrf) guard's own refusals — onto its variants.

use std::{error::Error, fmt};

use super::ssrf;

/// How an outbound HTTP request can fail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HttpError {
    /// The URL is unparsable, or not an `http`/`https` URL.
    InvalidUrl,
    /// A connect or total timeout elapsed before the response was read.
    Timeout,
    /// The response body passed the `max_bytes` cap of
    /// [`HttpClient::get_limited`](super::HttpClient::get_limited).
    TooLarge,
    /// The request never completed: a transport failure, a peer that
    /// closed the exchange, or a header HTTP cannot represent. The payload
    /// is the underlying client's message.
    Connect(String),
    /// The peer answered 4xx/5xx and the caller opted into treating that
    /// as an error (see
    /// [`HttpResponse::error_for_status`](super::HttpResponse::error_for_status));
    /// the payload is the status code.
    Status(u16),
    /// A local policy refused the request: today the SSRF guard (see
    /// [`ssrf`]), which rejects targets whose address is loopback,
    /// private, or otherwise reserved; the payload states the reason.
    Blocked(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl => f.write_str("URL is unparsable or not an http(s) URL"),
            Self::Timeout => f.write_str("HTTP request timed out"),
            Self::TooLarge => f.write_str("HTTP response exceeded the body size limit"),
            Self::Connect(detail) => write!(f, "HTTP request failed: {detail}"),
            Self::Status(code) => write!(f, "HTTP peer answered {code}"),
            Self::Blocked(reason) => write!(f, "HTTP request blocked: {reason}"),
        }
    }
}

impl Error for HttpError {}

/// Transport outcomes map onto the variants that describe them; timeouts
/// keep their own variant because callers treat them as retryable, and an
/// SSRF refusal surfacing through the guard's resolver or redirect policy
/// keeps [`HttpError::Blocked`] with its reason.
impl From<reqwest::Error> for HttpError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            return Self::Timeout;
        }
        if let Some(reason) = ssrf::blocked_reason(&err) {
            return Self::Blocked(reason);
        }
        Self::Connect(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_error_display_names_the_cause() {
        assert_eq!(
            HttpError::InvalidUrl.to_string(),
            "URL is unparsable or not an http(s) URL",
            "InvalidUrl"
        );
        assert_eq!(
            HttpError::Timeout.to_string(),
            "HTTP request timed out",
            "Timeout"
        );
        assert_eq!(
            HttpError::TooLarge.to_string(),
            "HTTP response exceeded the body size limit",
            "TooLarge"
        );
        assert_eq!(
            HttpError::Connect("refused".to_owned()).to_string(),
            "HTTP request failed: refused",
            "Connect keeps its detail"
        );
        assert_eq!(
            HttpError::Status(503).to_string(),
            "HTTP peer answered 503",
            "Status carries the code"
        );
        assert_eq!(
            HttpError::Blocked("private address".to_owned()).to_string(),
            "HTTP request blocked: private address",
            "Blocked keeps its reason"
        );
        assert!(HttpError::Timeout.source().is_none(), "no wrapped source");
    }
}
