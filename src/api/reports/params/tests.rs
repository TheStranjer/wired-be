//! Tests for the report parameters and their checks.

use serde_json::{Value, json};

use super::*;
use crate::config::DEFAULT_MAX_REPORT_COMMENT;

/// Parses `value` as the endpoint's parameters and validates the body-only
/// ones, as the handler would before it looks anything up.
fn body(value: Value) -> AppResult<(String, Category)> {
    let params: ReportParams = crate::extract::into_params(value)?;
    Ok((
        validated_comment(DEFAULT_MAX_REPORT_COMMENT, params.comment.as_deref())?,
        validated_category(params.category.as_deref())?,
    ))
}

/// The minimal valid parameter object.
fn minimal() -> Value {
    json!({"account_id": "01934567-89ab-7cde-f012-3456789abcde"})
}

/// The minimal body with parameter `key` set to `value`.
fn body_with(key: &str, value: Value) -> Value {
    let mut body = minimal();
    let _replaced: Option<Value> = body
        .as_object_mut()
        .and_then(|map| map.insert(key.to_owned(), value));
    body
}

/// The 422 message `value` is rejected with.
fn rejection(value: Value) -> String {
    body(value)
        .expect_err("the fixture body is invalid")
        .to_string()
}

/// The ids `value` yields under the `status_ids` spelling, as the text the
/// parameter layer reads them, validated with no database behind it.
fn ids(value: Value, refusal: &str) -> AppResult<Vec<String>> {
    let params: ReportParams = crate::extract::into_params(value)?;
    Ok(id_list(params.status_ids, refusal)?
        .into_iter()
        .map(|id| id.to_string())
        .collect())
}

#[test]
fn a_minimal_body_applies_every_default() {
    let (comment, category) = body(minimal()).expect("the minimal body validates");
    assert_eq!(comment, "", "no comment is the empty comment");
    assert_eq!(category, Category::Other, "and no category is `other`");
}

#[test]
fn the_comment_cap_is_the_configured_number_in_characters() {
    let at_the_cap = body_with("comment", json!("x".repeat(DEFAULT_MAX_REPORT_COMMENT)));
    let (comment, _) = body(at_the_cap).expect("a comment of exactly the cap fits");
    assert_eq!(
        comment.chars().count(),
        DEFAULT_MAX_REPORT_COMMENT,
        "the cap itself is still valid"
    );

    let message = rejection(body_with("comment", json!("x".repeat(1_001))));
    assert_eq!(
        message, "Validation failed: Comment is too long (maximum is 1000 characters)",
        "one over the cap is Mastodon's wording"
    );

    // The cap counts characters, not bytes: a multi-byte comment of the
    // same length is as valid as an ascii one.
    let unicode = body_with("comment", json!("é".repeat(DEFAULT_MAX_REPORT_COMMENT)));
    assert!(body(unicode).is_ok(), "a codepoint is one character");
}

#[test]
fn every_known_category_parses() {
    for category in Category::ALL {
        let (_, parsed) = body(body_with("category", json!(category.as_str())))
            .unwrap_or_else(|err| panic!("`{category}` is a category: {err}"));
        assert_eq!(parsed, category, "the name round-trips");
    }
}

#[test]
fn an_unknown_category_is_refused() {
    for value in [
        json!(""),
        json!("Spam"),
        json!("harassment"),
        json!("oth er"),
    ] {
        assert_eq!(
            rejection(body_with("category", value.clone())),
            CATEGORY_INVALID,
            "`{value}` is not one of the four"
        );
    }
}

#[test]
fn the_forward_flag_takes_the_form_spellings() {
    let params: ReportParams =
        crate::extract::into_params(body_with("forward", json!("true"))).expect("a form bool");
    assert_eq!(params.forward, Some(true), "the string `true`");

    let absent: ReportParams = crate::extract::into_params(minimal()).expect("no forward key");
    assert_eq!(absent.forward, None, "absent is None at the serde layer");
    assert!(
        !absent.forward.unwrap_or_default(),
        "and the handler defaults it to false"
    );
}

#[test]
fn an_array_or_a_lone_value_is_the_same_id_list() {
    let one = "01934567-89ab-7cde-f012-3456789abcde";
    let two = "01934567-89ab-7cde-f012-3456789abcd6";

    let listed =
        ids(body_with("status_ids", json!([one, two])), "bad").expect("a json array is a list");
    assert_eq!(listed, [one, two].map(str::to_owned), "in the order sent");

    let lone = ids(body_with("status_ids", json!(one)), "bad").expect("a lone value");
    assert_eq!(lone, [one].map(str::to_owned), "is the one-element list");

    let repeated =
        ids(body_with("status_ids", json!([one, two, one])), "bad").expect("a repeat is no error");
    assert_eq!(
        repeated,
        [one, two].map(str::to_owned),
        "and answers once, in order"
    );

    let empty = ids(body_with("status_ids", json!([])), "bad").expect("an empty list");
    assert!(empty.is_empty(), "is the empty list");

    let absent = ids(minimal(), "bad").expect("no key at all");
    assert!(absent.is_empty(), "and an absent parameter is the same");
}

#[test]
fn an_id_that_is_not_id_text_is_the_parameter_s_own_refusal() {
    assert_eq!(
        rejection_min("status_ids", json!(["not-an-id"])),
        "bad",
        "a malformed id is refused with the message the caller names"
    );
    assert_eq!(
        ids(
            body_with(
                "status_ids",
                json!(["01934567-89ab-7cde-f012-3456789abcde", "x"])
            ),
            INVALID_STATUS_IDS
        )
        .expect_err("one bad entry sinks the array")
        .to_string(),
        INVALID_STATUS_IDS,
        "and the endpoint's wording is the one the ticket pins"
    );
}

/// The refusal text a `status_ids` body of `value` is answered with.
fn rejection_min(key: &str, value: Value) -> String {
    ids(body_with(key, value), "bad")
        .expect_err("the fixture ids are invalid")
        .to_string()
}
