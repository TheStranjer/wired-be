//! The request parameters of the v2 filter writes and their validation.
//!
//! Mastodon's wording and rules (`CustomFilter`'s changeset): a blank title
//! or keyword, and an absent or unknown context, are 422s of the form
//! `{"error":"Validation failed: …"}`. `expires_in` is the number of seconds
//! until the filter stops applying; sent blank it clears the expiry, left
//! out it keeps it. `keywords_attributes` is Rails' nested-attributes list:
//! entries with an `id` change or (with `_destroy`) remove that keyword,
//! entries without one add a keyword.

use serde::{Deserialize, Deserializer, de::Error as _};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    api::filters::params::{ContextValues, expires_at_from, validated_context},
    error::{AppError, AppResult},
    extract::{flex, index_map_to_vec},
    models::filter::{
        ExpiryChange, FilterAction, FilterPatch, KeywordChange, NewFilter, keyword::NewKeyword,
    },
    util::id::parse_id,
};

/// 422 body for an absent or blank title.
const TITLE_BLANK: &str = "Validation failed: Title can't be blank";
/// 422 body for a blank keyword.
const KEYWORD_BLANK: &str = "Validation failed: Keyword can't be blank";
/// 422 body for an action other than `warn`, `hide` or `blur`.
const ACTION_INVALID: &str = "Validation failed: Filter action is invalid";

/// What `expires_in` said: nothing, "never", or a number of seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::api::filters) enum ExpiresIn {
    /// The parameter was not sent.
    #[default]
    Absent,
    /// `null` or a blank value: the filter does not expire.
    Never,
    /// Seconds from now.
    After(i64),
}

impl<'de> Deserialize<'de> for ExpiresIn {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Value::deserialize(deserializer)? {
            Value::Null => Ok(Self::Never),
            Value::String(text) if text.trim().is_empty() => Ok(Self::Never),
            Value::String(text) => text
                .trim()
                .parse::<i64>()
                .map(Self::After)
                .map_err(D::Error::custom),
            Value::Number(number) => number
                .as_i64()
                .map(Self::After)
                .ok_or_else(|| D::Error::custom("expected a whole number of seconds")),
            Value::Bool(_) | Value::Array(_) | Value::Object(_) => {
                Err(D::Error::custom("expected a number of seconds"))
            }
        }
    }
}

/// One entry of `keywords_attributes`.
#[derive(Debug, Deserialize)]
struct KeywordAttributes {
    /// The keyword to change or remove; absent adds a new one.
    #[serde(default)]
    id: Option<String>,
    /// The phrase.
    #[serde(default)]
    keyword: Option<String>,
    /// Whole-word matching.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    whole_word: Option<bool>,
    /// Remove the keyword `id` names.
    #[serde(default, rename = "_destroy", deserialize_with = "flex::opt_bool")]
    destroy: Option<bool>,
}

/// The parameters of `POST` and `PUT /api/v2/filters`.
#[derive(Debug, Default, Deserialize)]
pub(in crate::api::filters) struct FilterParams {
    /// The title; required on create.
    #[serde(default)]
    title: Option<String>,
    /// The contexts; required on create.
    #[serde(default)]
    context: Option<ContextValues>,
    /// `warn`, `hide` or `blur`; `warn` on create when absent.
    #[serde(default, alias = "action")]
    filter_action: Option<String>,
    /// Seconds until expiry.
    #[serde(default)]
    expires_in: ExpiresIn,
    /// The keyword list, as a JSON array or a form's indexed object.
    #[serde(default)]
    keywords_attributes: Option<Value>,
}

impl FilterParams {
    /// The filter and keywords a `POST` creates, validated.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] with Mastodon's changeset wording.
    pub(in crate::api::filters) fn into_new(self) -> AppResult<(NewFilter, Vec<NewKeyword>)> {
        let keywords = keyword_attributes(self.keywords_attributes)?
            .into_iter()
            .filter(|entry| !entry.destroy.unwrap_or_default())
            .map(KeywordAttributes::into_new)
            .collect::<AppResult<Vec<NewKeyword>>>()?;
        let new = NewFilter {
            title: validated_title(self.title)?,
            context: validated_context(self.context)?,
            action: self
                .filter_action
                .as_deref()
                .map_or(Ok(FilterAction::Warn), validated_action)?,
            expires_at: match self.expires_in {
                ExpiresIn::After(seconds) => expires_at_from(Some(seconds)),
                ExpiresIn::Absent | ExpiresIn::Never => None,
            },
        };
        Ok((new, keywords))
    }

    /// The patch and keyword changes a `PUT` applies: what was sent, each
    /// part validated like a create's.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] for a bad value, [`AppError::NotFound`]
    /// for a keyword id that is not an id.
    pub(in crate::api::filters) fn into_patch(
        self,
    ) -> AppResult<(FilterPatch, Vec<KeywordChange>)> {
        let changes = keyword_attributes(self.keywords_attributes)?
            .into_iter()
            .map(KeywordAttributes::into_change)
            .collect::<AppResult<Vec<KeywordChange>>>()?;
        let patch = FilterPatch {
            title: self
                .title
                .map(|title| validated_title(Some(title)))
                .transpose()?,
            context: self
                .context
                .map(|context| validated_context(Some(context)))
                .transpose()?,
            action: self
                .filter_action
                .as_deref()
                .map(validated_action)
                .transpose()?,
            expires_at: match self.expires_in {
                ExpiresIn::Absent => ExpiryChange::Keep,
                ExpiresIn::Never => ExpiryChange::Clear,
                ExpiresIn::After(seconds) => {
                    expires_at_from(Some(seconds)).map_or(ExpiryChange::Clear, ExpiryChange::Set)
                }
            },
        };
        Ok((patch, changes))
    }
}

impl KeywordAttributes {
    /// The keyword a create adds.
    fn into_new(self) -> AppResult<NewKeyword> {
        Ok(NewKeyword {
            keyword: validated_keyword(self.keyword)?,
            whole_word: self.whole_word.unwrap_or(true),
        })
    }

    /// The change an update applies: an edit or removal of the keyword `id`
    /// names, or the addition of a new one.
    fn into_change(self) -> AppResult<KeywordChange> {
        let destroy = self.destroy.unwrap_or_default();
        let id = self.id.as_deref().map(keyword_id).transpose()?;
        let keyword = match (id, destroy, self.keyword) {
            (_, true, _) | (Some(_), false, None) => None,
            (_, false, given) => Some(validated_keyword(given)?),
        };
        Ok(KeywordChange {
            id,
            keyword,
            whole_word: self.whole_word,
            destroy,
        })
    }
}

/// The id a keyword entry names.
///
/// # Errors
///
/// [`AppError::NotFound`] when the text is not an id.
fn keyword_id(text: &str) -> AppResult<Uuid> {
    parse_id(text).ok_or(AppError::NotFound)
}

/// The entries of `keywords_attributes`, whatever shape the client sent.
fn keyword_attributes(value: Option<Value>) -> AppResult<Vec<KeywordAttributes>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    index_map_to_vec(value)
        .into_iter()
        .map(|entry| {
            serde_json::from_value::<KeywordAttributes>(entry)
                .map_err(|err| AppError::Unprocessable(format!("Validation failed: {err}")))
        })
        .collect()
}

/// The title, checked: absent or blank is 422 [`TITLE_BLANK`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
pub(in crate::api::filters) fn validated_title(title: Option<String>) -> AppResult<String> {
    let text = title.unwrap_or_default();
    if text.trim().is_empty() {
        return Err(AppError::Unprocessable(TITLE_BLANK.to_owned()));
    }
    Ok(text)
}

/// The keyword phrase, checked: absent or blank is 422 [`KEYWORD_BLANK`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
pub(in crate::api::filters) fn validated_keyword(keyword: Option<String>) -> AppResult<String> {
    let text = keyword.unwrap_or_default();
    if text.trim().is_empty() {
        return Err(AppError::Unprocessable(KEYWORD_BLANK.to_owned()));
    }
    Ok(text)
}

/// The action `name` spells.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for anything but `warn`, `hide` or `blur`.
fn validated_action(name: &str) -> AppResult<FilterAction> {
    FilterAction::from_name(name).ok_or_else(|| AppError::Unprocessable(ACTION_INVALID.to_owned()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Parses `value` as the endpoint's parameters.
    fn params(value: Value) -> FilterParams {
        crate::extract::into_params(value).expect("the fixture parses")
    }

    /// The 422 message `value` is rejected with on create.
    fn create_rejection(value: Value) -> String {
        params(value)
            .into_new()
            .expect_err("the fixture is invalid")
            .to_string()
    }

    #[test]
    fn a_minimal_create_applies_the_defaults() {
        let (new, keywords) = params(json!({"title": "t", "context": ["home"]}))
            .into_new()
            .expect("the minimal body validates");
        assert_eq!(new.title, "t", "the title is kept");
        assert_eq!(new.context, ["home"], "the contexts go through");
        assert_eq!(new.action, FilterAction::Warn, "warn is the default action");
        assert!(new.expires_at.is_none(), "and absent means never");
        assert!(keywords.is_empty(), "no keywords were sent");
    }

    #[test]
    fn a_create_reads_the_action_expiry_and_keywords() {
        let (new, keywords) = params(json!({
            "title": "t",
            "context": ["home", "public"],
            "filter_action": "hide",
            "expires_in": "3600",
            "keywords_attributes": [
                {"keyword": "one"},
                {"keyword": "two", "whole_word": "false"},
                {"keyword": "gone", "_destroy": "1"},
            ],
        }))
        .into_new()
        .expect("the body validates");
        assert_eq!(new.action, FilterAction::Hide, "the action");
        assert!(new.expires_at.is_some(), "a positive expiry stamps a time");
        let words: Vec<(&str, bool)> = keywords
            .iter()
            .map(|found| (found.keyword.as_str(), found.whole_word))
            .collect();
        assert_eq!(
            words,
            [("one", true), ("two", false)],
            "the keywords, destroyed ones skipped"
        );
    }

    #[test]
    fn indexed_form_keywords_are_read_in_index_order() {
        let (_new, keywords) = params(json!({
            "title": "t",
            "context": ["home"],
            "keywords_attributes": {"1": {"keyword": "b"}, "0": {"keyword": "a"}},
        }))
        .into_new()
        .expect("the body validates");
        let words: Vec<&str> = keywords
            .iter()
            .map(|found| found.keyword.as_str())
            .collect();
        assert_eq!(words, ["a", "b"], "a form's index map is a list");
    }

    #[test]
    fn bad_creates_are_rejected_with_mastodon_wording() {
        assert_eq!(
            create_rejection(json!({"context": ["home"]})),
            TITLE_BLANK,
            "an absent title"
        );
        assert_eq!(
            create_rejection(json!({"title": "  ", "context": ["home"]})),
            TITLE_BLANK,
            "a blank title"
        );
        assert_eq!(
            create_rejection(json!({"title": "t"})),
            "Validation failed: Context is invalid",
            "an absent context"
        );
        assert_eq!(
            create_rejection(json!({"title": "t", "context": ["home"], "filter_action": "mute"})),
            ACTION_INVALID,
            "an unknown action"
        );
        assert_eq!(
            create_rejection(json!({
                "title": "t",
                "context": ["home"],
                "keywords_attributes": [{"keyword": " "}],
            })),
            KEYWORD_BLANK,
            "a blank keyword"
        );
    }

    #[test]
    fn expires_in_distinguishes_absent_blank_and_a_number() {
        for (body, expected) in [
            (json!({}), ExpiresIn::Absent),
            (json!({"expires_in": null}), ExpiresIn::Never),
            (json!({"expires_in": ""}), ExpiresIn::Never),
            (json!({"expires_in": 60}), ExpiresIn::After(60)),
            (json!({"expires_in": " 60 "}), ExpiresIn::After(60)),
        ] {
            assert_eq!(params(body.clone()).expires_in, expected, "`{body}`");
        }
        assert!(
            crate::extract::into_params::<FilterParams>(json!({"expires_in": "soon"})).is_err(),
            "text that is no number is a parameter error"
        );
        assert!(
            crate::extract::into_params::<FilterParams>(json!({"expires_in": true})).is_err(),
            "so is a boolean"
        );
    }

    #[test]
    fn a_patch_carries_only_what_was_sent() {
        let (empty, none) = params(json!({}))
            .into_patch()
            .expect("an empty patch is valid");
        assert!(
            empty.title.is_none() && empty.context.is_none(),
            "no columns"
        );
        assert!(empty.action.is_none(), "no action");
        assert_eq!(empty.expires_at, ExpiryChange::Keep, "the expiry is kept");
        assert!(none.is_empty(), "no keyword changes");

        let cleared = params(json!({"expires_in": ""}))
            .into_patch()
            .expect("a blank expiry is valid")
            .0;
        assert_eq!(cleared.expires_at, ExpiryChange::Clear, "blank clears it");

        let set = params(json!({"title": "n", "expires_in": 30, "filter_action": "blur"}))
            .into_patch()
            .expect("the patch is valid")
            .0;
        assert_eq!(set.title.as_deref(), Some("n"), "the new title");
        assert_eq!(set.action, Some(FilterAction::Blur), "the new action");
        assert!(
            matches!(set.expires_at, ExpiryChange::Set(_)),
            "a number sets the expiry"
        );
    }

    #[test]
    fn a_patch_validates_what_it_carries() {
        for body in [
            json!({"title": ""}),
            json!({"context": ["bogus"]}),
            json!({"filter_action": "mute"}),
            json!({"keywords_attributes": [{"keyword": ""}]}),
        ] {
            assert!(
                matches!(
                    params(body.clone()).into_patch(),
                    Err(AppError::Unprocessable(_))
                ),
                "`{body}` is a 422"
            );
        }
        assert!(
            matches!(
                params(json!({"keywords_attributes": [{"id": "nope", "_destroy": true}]}))
                    .into_patch(),
                Err(AppError::NotFound)
            ),
            "a keyword id that is no id names nothing"
        );
    }

    #[test]
    fn keyword_changes_are_edits_removals_and_additions() {
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let (_patch, changes) = params(json!({
            "keywords_attributes": [
                {"id": first.to_string(), "keyword": "renamed", "whole_word": false},
                {"id": second.to_string(), "_destroy": "true"},
                {"keyword": "added"},
                {"id": first.to_string(), "whole_word": true},
            ],
        }))
        .into_patch()
        .expect("the changes validate");
        let [edit, removal, addition, flag] = changes.as_slice() else {
            panic!("four changes were sent");
        };
        assert_eq!(edit.id, Some(first), "the edit names its keyword");
        assert_eq!(edit.keyword.as_deref(), Some("renamed"), "and the phrase");
        assert_eq!(edit.whole_word, Some(false), "and the flag");
        assert!(removal.destroy && removal.id == Some(second), "the removal");
        assert_eq!(addition.id, None, "an addition has no id");
        assert_eq!(addition.keyword.as_deref(), Some("added"), "just a phrase");
        assert!(flag.keyword.is_none(), "a flag-only edit keeps the phrase");
    }
}
