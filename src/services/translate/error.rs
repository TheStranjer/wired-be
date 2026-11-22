//! The error a translation attempt fails with.

use std::{error::Error, fmt};

/// How one translation attempt failed. Both kinds are the endpoints' 503;
/// what differs is only what the log says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranslateError {
    /// The provider could not be reached, or refused the call. The payload is
    /// the transport failure or the status code it answered with.
    Unavailable(String),
    /// The provider answered success with something this service cannot read.
    /// The payload is the parse failure, or what was missing.
    InvalidResponse(String),
}

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(f, "translation provider unavailable: {detail}"),
            Self::InvalidResponse(detail) => {
                write!(f, "translation provider answered unreadably: {detail}")
            }
        }
    }
}

impl Error for TranslateError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_errors_state_why_the_provider_failed() {
        assert_eq!(
            TranslateError::Unavailable("timeout".to_owned()).to_string(),
            "translation provider unavailable: timeout",
            "the transport kind"
        );
        assert_eq!(
            TranslateError::InvalidResponse("not json".to_owned()).to_string(),
            "translation provider answered unreadably: not json",
            "the shape kind"
        );
        assert!(
            TranslateError::InvalidResponse(String::new())
                .source()
                .is_none(),
            "the reason is text, so nothing wraps it"
        );
    }
}
