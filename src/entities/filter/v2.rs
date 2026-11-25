//! The Mastodon v2 `Filter`, `FilterKeyword`, `FilterStatus` and
//! `FilterResult` entities.
//!
//! Reference: Mastodon's `REST::FilterSerializer`,
//! `REST::FilterKeywordSerializer`, `REST::FilterStatusSerializer` and
//! `REST::FilterResultSerializer`. Ids are the rows' `UUIDv7`s as
//! lowercase hyphenated text (decision 4), times are in the Mastodon time
//! format (decision 5).

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::models::filter::{Filter as FilterRow, keyword::Keyword, status::FilterStatus as Entry};

/// The v2 `Filter` entity: a titled rule with its keywords and statuses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Filter {
    /// The filter id.
    pub id: String,
    /// The filter's title.
    pub title: String,
    /// The contexts the filter applies in, as stored.
    pub context: Vec<String>,
    /// When the filter stops being active; `null` when it never does.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub expires_at: Option<DateTime<Utc>>,
    /// What a match does: `warn`, `hide` or `blur`.
    #[serde(rename = "filter_action")]
    pub action: &'static str,
    /// The phrases the filter looks for.
    pub keywords: Vec<FilterKeyword>,
    /// The single statuses the filter matches.
    pub statuses: Vec<FilterStatus>,
}

impl Filter {
    /// Renders `row` with everything it holds.
    #[must_use]
    pub fn from_row(row: &FilterRow) -> Self {
        Self {
            id: row.id.to_string(),
            title: row.title.clone(),
            context: row.context.clone(),
            expires_at: row.expires_at,
            action: row.action.as_str(),
            keywords: row.keywords.iter().map(FilterKeyword::from_row).collect(),
            statuses: row.statuses.iter().map(FilterStatus::from_row).collect(),
        }
    }
}

/// The `FilterKeyword` entity: one phrase of a filter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FilterKeyword {
    /// The keyword id.
    pub id: String,
    /// The phrase, verbatim as its owner wrote it.
    pub keyword: String,
    /// Whether the phrase must appear as a whole word.
    pub whole_word: bool,
}

impl FilterKeyword {
    /// Renders `row`.
    #[must_use]
    pub fn from_row(row: &Keyword) -> Self {
        Self {
            id: row.id.to_string(),
            keyword: row.phrase.clone(),
            whole_word: row.whole_word,
        }
    }
}

/// The `FilterStatus` entity: one status a filter matches whole.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FilterStatus {
    /// The entry id.
    pub id: String,
    /// The id of the matched status.
    pub status_id: String,
}

impl FilterStatus {
    /// Renders `row`.
    #[must_use]
    pub fn from_row(row: &Entry) -> Self {
        Self {
            id: row.id.to_string(),
            status_id: row.status_id.to_string(),
        }
    }
}

/// One entry of a rendered status's `filtered` array.
///
/// It holds the filter that matched the status, the keywords that matched
/// and the status ids that matched. A list with nothing in it is `null`, as
/// Mastodon renders it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FilterResult {
    /// The matched filter.
    pub filter: Filter,
    /// The filter's phrases the status text contained; `null` when only a
    /// status entry matched.
    pub keyword_matches: Option<Vec<String>>,
    /// The ids of the filter's status entries the status (or the original
    /// it boosts) is; `null` when only a keyword matched.
    pub status_matches: Option<Vec<String>>,
}

impl FilterResult {
    /// The result of `row` matching on `keyword_matches` and
    /// `status_matches`; a list that came up empty renders `null`.
    #[must_use]
    pub fn new(row: &FilterRow, keyword_matches: Vec<String>, status_matches: Vec<String>) -> Self {
        Self {
            filter: Filter::from_row(row),
            keyword_matches: (!keyword_matches.is_empty()).then_some(keyword_matches),
            status_matches: (!status_matches.is_empty()).then_some(status_matches),
        }
    }
}
