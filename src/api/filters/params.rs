//! Request-parameter rules the v1 and v2 filter endpoints share: the context
//! list and the expiry, with Mastodon's validation wording.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::{AppError, AppResult};

/// The contexts a filter may name (Mastodon's valid filter contexts).
pub(super) const VALID_CONTEXTS: [&str; 5] =
    ["home", "notifications", "public", "thread", "account"];
/// 422 body for an absent, empty or unknown context (Mastodon's changeset
/// wording).
pub(super) const CONTEXT_INVALID: &str = "Validation failed: Context is invalid";

/// `context` as clients send it: one string or a list (a JSON array, or
/// form `context[]=…` keys).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum ContextValues {
    /// A lone context name.
    Single(String),
    /// A list of context names.
    Multiple(Vec<String>),
}

impl ContextValues {
    /// The names as a list; a lone string becomes the one-element list.
    fn into_list(self) -> Vec<String> {
        match self {
            Self::Single(text) => vec![text],
            Self::Multiple(values) => values,
        }
    }
}

/// The context list, checked: absent, empty or carrying any name outside
/// [`VALID_CONTEXTS`] is 422 [`CONTEXT_INVALID`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
pub(super) fn validated_context(context: Option<ContextValues>) -> AppResult<Vec<String>> {
    let values = context.map_or_else(Vec::new, ContextValues::into_list);
    let known = values
        .iter()
        .all(|value| VALID_CONTEXTS.contains(&value.as_str()));
    if values.is_empty() || !known {
        return Err(AppError::Unprocessable(CONTEXT_INVALID.to_owned()));
    }
    Ok(values)
}

/// The expiry an `expires_in` of `seconds` describes: `None` when absent or
/// `null`, when the value is not positive, or when it names a span too
/// large to represent — anything unrepresentable is "never", not an error.
pub(super) fn expires_at_from(seconds: Option<i64>) -> Option<DateTime<Utc>> {
    seconds
        .filter(|value| *value > 0)
        .and_then(chrono::Duration::try_seconds)
        .and_then(|span| Utc::now().checked_add_signed(span))
}
