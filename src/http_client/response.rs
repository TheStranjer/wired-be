//! [`HttpResponse`]: an outbound response read in full — status, headers
//! and buffered body — with the JSON parsing and status-checking helpers
//! callers chain onto it.

use bytes::Bytes;
use reqwest::header::HeaderMap;
use serde::de::DeserializeOwned;

use super::{ERROR_STATUS_MIN, HttpError};

/// An outbound response read in full: status, headers and buffered body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    /// HTTP status code of the response.
    pub status: u16,
    /// Response headers.
    pub headers: HeaderMap,
    /// The buffered response body.
    pub body: Bytes,
}

impl HttpResponse {
    /// Parses [`body`](Self::body) as JSON of shape `T`.
    ///
    /// # Errors
    ///
    /// The [`serde_json::Error`] for malformed JSON or a shape mismatch;
    /// this is a pure parsing helper, no HTTP is involved.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    /// Turns a status at or above 400 into [`HttpError::Status`], passing
    /// `self` through unchanged otherwise (mirrors `reqwest`'s helper of
    /// the same name, but reports this module's error type).
    ///
    /// # Errors
    ///
    /// [`HttpError::Status`] carrying [`status`](Self::status) when it is
    /// 400 or greater.
    pub fn error_for_status(self) -> Result<Self, HttpError> {
        if self.status >= ERROR_STATUS_MIN {
            Err(HttpError::Status(self.status))
        } else {
            Ok(self)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn error_for_status_flags_4xx_and_5xx_only() {
        let base = HttpResponse {
            status: 0,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        };
        for (status, failed) in [
            (200_u16, false),
            (204, false),
            (301, false),
            (399, false),
            (400, true),
            (404, true),
            (503, true),
        ] {
            let checked = HttpResponse {
                status,
                ..base.clone()
            }
            .error_for_status();
            assert_eq!(
                matches!(checked, Err(HttpError::Status(_))),
                failed,
                "status {status}"
            );
            if failed {
                assert_eq!(
                    checked.unwrap_err(),
                    HttpError::Status(status),
                    "the status code is carried"
                );
            }
        }
    }

    #[test]
    fn json_parses_the_body() {
        let response = HttpResponse {
            status: 200,
            headers: HeaderMap::new(),
            body: Bytes::from_static(br#"{"name":"wired"}"#),
        };
        let value: Value = response.json().unwrap();
        assert_eq!(value["name"], "wired", "body parsed");
        let broken = HttpResponse {
            body: Bytes::from_static(b"{oops"),
            ..response
        };
        let parse_err = broken.json::<Value>().unwrap_err();
        assert!(
            !parse_err.to_string().is_empty(),
            "malformed JSON reports a parse error"
        );
    }
}
