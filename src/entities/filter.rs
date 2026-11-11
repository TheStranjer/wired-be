//! The Mastodon v1 `Filter` entity, and the v2 `FilterResult` a filtered
//! status carries.
//!
//! The reference shape is Pleroma's `FilterView.render("show.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/filter_view.ex`): `id`, `phrase`,
//! `context`, `expires_at`, `irreversible` and `whole_word`, in that order.
//! Two keys read differently here than there. `id` is Pleroma's per-user
//! integer counter; per `docs/ARCHITECTURE.md` decision 4 this entity
//! carries the row's `UUIDv7` as lowercase hyphenated text instead.
//! `irreversible` is Pleroma's `hide` column renamed: the API has always
//! called it `irreversible`, and the model keeps it under that name.
//!
//! [`Filter::from_row`] is pure (decision 3): it renders a
//! [`models::filter`](crate::models::filter) row and nothing else — the
//! owner, the row's timestamps and the expiry rule live behind it, not in
//! this shape. `expires_at` is `null` for a filter that never expires, in
//! the Mastodon time format otherwise (decision 5).
//!
//! [`FilterResult`] is the Mastodon v2 shape of one filter that warned
//! over a rendered status — the entry of the `Status` entity's `filtered`
//! array. v1 semantics keep the phrase inside the filter row, so the
//! embedded filter names its phrase twice (`title` and its one `keyword`)
//! and the match lists read off it; the actions are the ticket's: a
//! matching `irreversible` filter removes the status from the response
//! instead of annotating it, so every emitted result is a `warn`.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::models::filter::Filter as FilterRow;

/// The Mastodon v1 `Filter` entity.
///
/// Build one with [`Filter::from_row`]; its [`Serialize`] impl emits `id`,
/// `phrase`, `context`, `expires_at`, `irreversible` and `whole_word`.
#[derive(Clone, Debug, Serialize)]
pub struct Filter {
    /// The filter id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    pub id: String,
    /// The phrase to look for, verbatim as its owner wrote it.
    pub phrase: String,
    /// The contexts the filter applies in, as stored.
    pub context: Vec<String>,
    /// When the filter stops being active, in the Mastodon time format
    /// (decision 5); `null` when it never does.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether clients hide matching posts instead of warning.
    pub irreversible: bool,
    /// Whether the phrase must appear as a whole word.
    pub whole_word: bool,
}

impl Filter {
    /// Renders `row` as the entity.
    #[must_use]
    pub fn from_row(row: &FilterRow) -> Self {
        Self {
            id: row.id.to_string(),
            phrase: row.phrase.clone(),
            context: row.context.clone(),
            expires_at: row.expires_at,
            irreversible: row.irreversible,
            whole_word: row.whole_word,
        }
    }
}

/// One entry of a rendered status's `filtered` array (Mastodon v2
/// `FilterResult`): the filter that warned over the status, the phrases
/// that matched and — always `null` here — the whole-status entries.
///
/// [`FilterResult::warn`] builds it; [`crate::services::filter`] decides
/// when one rides a status.
#[derive(Clone, Debug, Serialize)]
pub struct FilterResult {
    /// The matched filter, as the v2 summary [`FilteredFilter`] renders.
    pub filter: FilteredFilter,
    /// The filter's phrases the status text contained — for a v1 filter,
    /// exactly its one phrase.
    pub keyword_matches: Vec<String>,
    /// The whole-status entries of the filter the status is one of; v1
    /// filters store none, so this is always JSON `null`.
    pub status_matches: Option<Vec<String>>,
}

/// The `filter` object of a [`FilterResult`]: the Mastodon v2 summary of
/// the matched filter, built off the v1 row.
///
/// v1 has no separate keyword rows, so `keywords` carries the filter's own
/// phrase as its one entry (sharing the filter's id), and `statuses` is
/// always empty.
#[derive(Clone, Debug, Serialize)]
pub struct FilteredFilter {
    /// The filter id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    pub id: String,
    /// The filter's phrase — v2 calls a filter's one keyword its title.
    pub title: String,
    /// The contexts the filter applies in, as stored.
    pub context: Vec<String>,
    /// When the filter stops being active, in the Mastodon time format
    /// (decision 5); `null` when it never does.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub expires_at: Option<DateTime<Utc>>,
    /// What clients do with a matching status: `warn` renders it under a
    /// warning. A matching `irreversible` filter is never annotated —
    /// [`crate::services::filter`] removes its statuses from the response
    /// — so this is always `warn`.
    pub filter_action: &'static str,
    /// The filter's keywords: its one phrase, carrying the filter's own
    /// id (v1 keeps no keyword rows).
    pub keywords: Vec<FilteredKeyword>,
    /// The status ids the filter hides whole: v1 filters name none, so
    /// this is always the empty array.
    pub statuses: Vec<String>,
}

/// One `keywords` entry of a [`FilteredFilter`]: the filter's phrase as
/// Mastodon v2 spells a keyword.
#[derive(Clone, Debug, Serialize)]
pub struct FilteredKeyword {
    /// The keyword id: the filter's own, since v1 stores one phrase per
    /// filter and no keyword rows.
    pub id: String,
    /// The phrase, verbatim as its owner wrote it.
    pub keyword: String,
    /// Whether the phrase must appear as a whole word.
    pub whole_word: bool,
}

impl FilterResult {
    /// The `warn` result of `row` matching a rendered status: the row's
    /// one phrase as title, keyword and match, the row's own contexts and
    /// expiry, and none of the whole-status machinery v1 lacks.
    #[must_use]
    pub fn warn(row: &FilterRow) -> Self {
        let id = row.id.to_string();
        Self {
            filter: FilteredFilter {
                id: id.clone(),
                title: row.phrase.clone(),
                context: row.context.clone(),
                expires_at: row.expires_at,
                filter_action: "warn",
                keywords: vec![FilteredKeyword {
                    id,
                    keyword: row.phrase.clone(),
                    whole_word: row.whole_word,
                }],
                statuses: Vec::new(),
            },
            keyword_matches: vec![row.phrase.clone()],
            status_matches: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::util::time::parse_time;

    /// The filter id, as a client sees it.
    const FILTER_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// When the filter expires.
    const EXPIRES: &str = "2100-06-01T12:00:00.250Z";

    /// Every JSON key the entity must emit, sorted, for an exact key-set
    /// check.
    const EXPECTED_KEYS: [&str; 6] = [
        "context",
        "expires_at",
        "id",
        "irreversible",
        "phrase",
        "whole_word",
    ];

    /// A stored filter with both flags set and an expiry.
    fn row() -> FilterRow {
        FilterRow {
            id: Uuid::parse_str(FILTER_ID).expect("the fixture id is an id"),
            user_id: Uuid::now_v7(),
            phrase: "Spoilers".to_owned(),
            context: vec!["home".to_owned(), "public".to_owned()],
            whole_word: false,
            irreversible: true,
            expires_at: Some(parse_time(EXPIRES).expect("the fixture time parses")),
            inserted_at: parse_time(EXPIRES).expect("the fixture time parses"),
            updated_at: parse_time(EXPIRES).expect("the fixture time parses"),
        }
    }

    /// `row` rendered as a JSON value; entities always serialize.
    fn render(row: &FilterRow) -> Value {
        serde_json::to_value(Filter::from_row(row)).expect("an entity serializes")
    }

    /// The rendered object's keys, sorted, for exact key-set assertions.
    fn json_keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("filters render objects")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn a_filter_renders_the_mastodon_entity() {
        let value = render(&row());

        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exactly the keys");
        assert_eq!(value["id"], FILTER_ID, "the id as hyphenated text");
        assert_eq!(value["phrase"], "Spoilers", "the phrase verbatim");
        assert_eq!(
            value["context"],
            json!(["home", "public"]),
            "the contexts as stored, in order"
        );
        assert_eq!(value["expires_at"], EXPIRES, "the expiry in Mastodon time");
        assert_eq!(value["irreversible"], true, "the stored hide flag");
        assert_eq!(value["whole_word"], false, "and the word-boundary flag");
    }

    #[test]
    fn a_filter_that_never_expires_renders_null() {
        let mut stored = row();
        stored.expires_at = None;
        let value = render(&stored);

        assert_eq!(value["expires_at"], Value::Null, "null, not absent");
        assert_eq!(json_keys(&value), EXPECTED_KEYS, "the key stays");
    }

    #[test]
    fn the_defaults_render_unchanged() {
        let stored = FilterRow {
            expires_at: None,
            whole_word: true,
            irreversible: false,
            phrase: String::new(),
            context: Vec::new(),
            ..row()
        };
        let value = render(&stored);

        assert_eq!(value["phrase"], "", "an empty phrase is empty, not null");
        assert_eq!(value["context"], json!([]), "and so is an empty list");
        assert_eq!(value["whole_word"], true, "the column defaults");
        assert_eq!(value["irreversible"], false, "render through");
    }

    /// A warn result rendered to JSON, for the shape assertions.
    fn warn_value(stored: &FilterRow) -> Value {
        serde_json::to_value(FilterResult::warn(stored)).expect("a result serializes")
    }

    #[test]
    fn a_warn_result_renders_the_v2_filtered_entity() {
        let stored = FilterRow {
            irreversible: false,
            whole_word: true,
            phrase: "spoilers".to_owned(),
            context: vec!["home".to_owned(), "public".to_owned()],
            expires_at: None,
            ..row()
        };
        let value = warn_value(&stored);
        assert_eq!(
            json_keys(&value),
            ["filter", "keyword_matches", "status_matches"],
            "the result's own keys"
        );
        assert_eq!(
            json_keys(&value["filter"]),
            [
                "context",
                "expires_at",
                "filter_action",
                "id",
                "keywords",
                "statuses",
                "title"
            ],
            "the embedded v2 filter's keys"
        );
        assert_eq!(value["filter"]["id"], FILTER_ID, "the filter's own id");
        assert_eq!(value["filter"]["title"], "spoilers", "the phrase as title");
        assert_eq!(
            value["filter"]["context"],
            json!(["home", "public"]),
            "the contexts as stored"
        );
        assert_eq!(value["filter"]["expires_at"], Value::Null, "never expires");
        assert_eq!(value["filter"]["filter_action"], "warn", "the v1 warn arm");
        assert_eq!(
            value["filter"]["keywords"],
            json!([{
                "id": FILTER_ID,
                "keyword": "spoilers",
                "whole_word": true,
            }]),
            "the one phrase, keyed by the filter's id"
        );
        assert_eq!(
            value["filter"]["statuses"],
            json!([]),
            "v1 filters name no whole statuses"
        );
        assert_eq!(
            value["keyword_matches"],
            json!(["spoilers"]),
            "the matched phrase"
        );
        assert_eq!(value["status_matches"], Value::Null, "and no status match");
    }

    #[test]
    fn a_warn_result_carries_the_expiry_and_the_whole_word_flag() {
        let stored = FilterRow {
            whole_word: false,
            expires_at: Some(parse_time(EXPIRES).expect("the fixture time parses")),
            ..row()
        };
        let value = warn_value(&stored);
        assert_eq!(value["filter"]["expires_at"], EXPIRES, "the Mastodon time");
        assert_eq!(
            value["filter"]["keywords"][0]["whole_word"], false,
            "the flag rides through"
        );
    }
}
