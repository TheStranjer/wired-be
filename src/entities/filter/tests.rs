//! Tests of the v1 and v2 filter entities.

use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::{
    models::filter::{Filter as FilterRow, FilterAction, keyword::Keyword, status::FilterStatus},
    test_fixtures::json_keys,
    util::time::parse_time,
};

/// The filter id, as a client sees it.
const FILTER_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// The keyword id.
const KEYWORD_ID: &str = "01934567-89ab-7cde-f012-3456789abcdf";
/// The status entry id.
const ENTRY_ID: &str = "01934567-89ab-7cde-f012-3456789abce0";
/// The matched status id.
const STATUS_ID: &str = "01934567-89ab-7cde-f012-3456789abce1";
/// When the filter expires.
const EXPIRES: &str = "2100-06-01T12:00:00.250Z";

/// Every JSON key the v1 entity must emit, sorted.
const V1_KEYS: [&str; 6] = [
    "context",
    "expires_at",
    "id",
    "irreversible",
    "phrase",
    "whole_word",
];

/// The id `text` spells.
fn id(text: &str) -> Uuid {
    Uuid::parse_str(text).expect("the fixture id is an id")
}

/// A stored keyword.
fn keyword() -> Keyword {
    Keyword {
        id: id(KEYWORD_ID),
        filter_id: id(FILTER_ID),
        phrase: "Spoilers".to_owned(),
        whole_word: false,
        inserted_at: parse_time(EXPIRES).expect("the fixture time parses"),
        updated_at: parse_time(EXPIRES).expect("the fixture time parses"),
    }
}

/// A stored filter with one keyword and one status entry.
fn row() -> FilterRow {
    FilterRow {
        id: id(FILTER_ID),
        user_id: Uuid::now_v7(),
        title: "Hide spoilers".to_owned(),
        context: vec!["home".to_owned(), "public".to_owned()],
        action: FilterAction::Hide,
        expires_at: Some(parse_time(EXPIRES).expect("the fixture time parses")),
        inserted_at: parse_time(EXPIRES).expect("the fixture time parses"),
        updated_at: parse_time(EXPIRES).expect("the fixture time parses"),
        keywords: vec![keyword()],
        statuses: vec![FilterStatus {
            id: id(ENTRY_ID),
            filter_id: id(FILTER_ID),
            status_id: id(STATUS_ID),
            inserted_at: parse_time(EXPIRES).expect("the fixture time parses"),
            updated_at: parse_time(EXPIRES).expect("the fixture time parses"),
        }],
    }
}

/// The v1 entity of `stored`'s keyword, as JSON.
fn v1(stored: &FilterRow) -> Value {
    let found = stored.keywords.first().expect("the fixture has a keyword");
    let phrase = Phrase::new(stored, found);
    serde_json::to_value(Filter::from_phrase(&phrase)).expect("an entity serializes")
}

#[test]
fn a_v1_filter_renders_the_mastodon_entity() {
    let value = v1(&row());

    assert_eq!(json_keys(&value), V1_KEYS, "exactly the keys");
    assert_eq!(value["id"], KEYWORD_ID, "the keyword's id, as text");
    assert_eq!(value["phrase"], "Spoilers", "the phrase verbatim");
    assert_eq!(
        value["context"],
        json!(["home", "public"]),
        "the contexts as stored, in order"
    );
    assert_eq!(value["expires_at"], EXPIRES, "the expiry in Mastodon time");
    assert_eq!(value["irreversible"], true, "hide reads as irreversible");
    assert_eq!(value["whole_word"], false, "the word-boundary flag");
}

#[test]
fn a_v1_filter_that_warns_and_never_expires_renders_plainly() {
    let stored = FilterRow {
        expires_at: None,
        action: FilterAction::Blur,
        ..row()
    };
    let value = v1(&stored);

    assert_eq!(value["expires_at"], Value::Null, "null, not absent");
    assert_eq!(value["irreversible"], false, "only hide is irreversible");
    assert_eq!(json_keys(&value), V1_KEYS, "the keys stay");
}

#[test]
fn a_v2_filter_renders_its_keywords_and_statuses() {
    let value = serde_json::to_value(v2::Filter::from_row(&row())).expect("it serializes");

    assert_eq!(
        json_keys(&value),
        [
            "context",
            "expires_at",
            "filter_action",
            "id",
            "keywords",
            "statuses",
            "title"
        ],
        "the entity's keys"
    );
    assert_eq!(value["id"], FILTER_ID, "the filter id");
    assert_eq!(value["title"], "Hide spoilers", "the title");
    assert_eq!(value["filter_action"], "hide", "the action's name");
    assert_eq!(value["expires_at"], EXPIRES, "the Mastodon time");
    assert_eq!(
        value["keywords"],
        json!([{"id": KEYWORD_ID, "keyword": "Spoilers", "whole_word": false}]),
        "the keyword entity"
    );
    assert_eq!(
        value["statuses"],
        json!([{"id": ENTRY_ID, "status_id": STATUS_ID}]),
        "the status entry entity"
    );
}

#[test]
fn a_result_renders_empty_match_lists_as_null() {
    let both = serde_json::to_value(FilterResult::new(
        &row(),
        vec!["Spoilers".to_owned()],
        vec![STATUS_ID.to_owned()],
    ))
    .expect("a result serializes");
    assert_eq!(
        json_keys(&both),
        ["filter", "keyword_matches", "status_matches"],
        "the result's own keys"
    );
    assert_eq!(both["filter"]["id"], FILTER_ID, "the embedded filter");
    assert_eq!(both["keyword_matches"], json!(["Spoilers"]), "the phrases");
    assert_eq!(both["status_matches"], json!([STATUS_ID]), "the status ids");

    let keyword_only = serde_json::to_value(FilterResult::new(
        &row(),
        vec!["Spoilers".to_owned()],
        Vec::new(),
    ))
    .expect("a result serializes");
    assert_eq!(
        keyword_only["status_matches"],
        Value::Null,
        "nothing matched by status is null"
    );
    let status_only = serde_json::to_value(FilterResult::new(
        &row(),
        Vec::new(),
        vec![STATUS_ID.to_owned()],
    ))
    .expect("a result serializes");
    assert_eq!(
        status_only["keyword_matches"],
        Value::Null,
        "nothing matched by keyword is null"
    );
}
