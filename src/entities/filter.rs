//! The Mastodon v1 `Filter` entity: one account's phrase filter as clients
//! render it.
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

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::models::filter::Filter as FilterRow;

/// The Mastodon v1 `Filter` entity.
///
/// Build one with [`Filter::from_row`]; its [`Serialize`] impl emits `id`,
/// `phrase`, `context`, `expires_at`, `irreversible` and `whole_word`.
#[derive(Debug, Serialize)]
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
}
