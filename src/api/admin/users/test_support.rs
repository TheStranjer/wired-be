//! Fixtures more than one child's tests build on.

use serde_json::Value;

use super::credentials::CredentialsParams;

/// The credentials body parsed out of one JSON value, panicking on a
/// shape the extractor would have refused.
pub(super) fn params(body: Value) -> CredentialsParams {
    serde_json::from_value::<CredentialsParams>(body).expect("the fixture body parses")
}
