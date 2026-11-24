//! The `media_attributes` parameter of `PUT /api/v1/statuses/{id}`.
//!
//! Mastodon clients change alt text and focal points of attached media
//! through the edit, as `media_attributes: [{id, description, focus}]`:
//! JSON's array of objects, or the form's `media_attributes[][id]=…` and
//! `media_attributes[0][id]=…` spellings. [`MediaAttribute`] is one entry as
//! the client wrote it and [`into_changes`] the checked form the write takes.

use serde::{Deserialize, Deserializer, de::Error as _};
use serde_json::Value;

use crate::{
    api::media::{FOCUS_INVALID, check_description, update::parse_focus},
    config::Config,
    error::{AppError, AppResult},
    extract::index_map_to_vec,
    models::status_edit::media_attributes::{
        FocusChange, MediaAttributeChange, UNKNOWN_ATTRIBUTE_MEDIA,
    },
    util::id::parse_id,
};

/// One `media_attributes` entry, as the client wrote it.
///
/// Fields a client sends that this does not name (`thumbnail`) are ignored.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct MediaAttribute {
    /// The attachment's id, as text.
    #[serde(default)]
    id: String,
    /// The replacement alt text; empty clears it.
    #[serde(default)]
    description: Option<String>,
    /// The replacement focal point, `"x,y"`; empty clears it.
    #[serde(default)]
    focus: Option<String>,
}

/// Reads the `media_attributes` parameter: a JSON array of entries, or the
/// index map (`media_attributes[0][id]=…`) a form body arrives as.
///
/// # Errors
///
/// A serde error when an entry is not an object of text fields.
pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<MediaAttribute>>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    index_map_to_vec(value)
        .into_iter()
        .map(|entry| serde_json::from_value(entry).map_err(D::Error::custom))
        .collect::<Result<Vec<MediaAttribute>, D::Error>>()
        .map(Some)
}

/// The checked changes `attributes` ask for.
///
/// Each entry's `id` must be an id at all (the edit then checks it names one
/// of the post's attachments), its `description` within
/// `config.max_media_description` characters, and its `focus` empty (clear)
/// or `"x,y"` with both axes in `[-1, 1]`.
///
/// # Errors
///
/// [`AppError::Unprocessable`]: [`UNKNOWN_ATTRIBUTE_MEDIA`] for an id that
/// is no id, the description wording for alt text over the limit and the
/// media endpoint's focus wording for a bad focal point.
pub fn into_changes(
    config: &Config,
    attributes: &[MediaAttribute],
) -> AppResult<Vec<MediaAttributeChange>> {
    attributes
        .iter()
        .map(|attribute| change_of(config, attribute))
        .collect()
}

/// [`into_changes`] for one entry.
fn change_of(config: &Config, attribute: &MediaAttribute) -> AppResult<MediaAttributeChange> {
    let id = parse_id(&attribute.id)
        .ok_or_else(|| AppError::Unprocessable(UNKNOWN_ATTRIBUTE_MEDIA.to_owned()))?;
    if let Some(text) = &attribute.description {
        check_description(config, text)?;
    }
    let focus = match attribute.focus.as_deref().map(str::trim) {
        None => None,
        Some("") => Some(FocusChange::Clear),
        Some(text) => {
            let (x, y) = parse_focus(text, FOCUS_INVALID)?;
            Some(FocusChange::Set { x, y })
        }
    };
    Ok(MediaAttributeChange {
        id,
        description: attribute.description.clone(),
        focus,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The entries `value` reads as.
    fn read(value: Value) -> Result<Option<Vec<MediaAttribute>>, serde_json::Error> {
        deserialize(value)
    }

    /// An entry of just `id` and `description`.
    fn entry(id: &str, description: &str) -> MediaAttribute {
        MediaAttribute {
            id: id.to_owned(),
            description: Some(description.to_owned()),
            focus: None,
        }
    }

    /// The refusal `attributes` meets, panicking when they pass.
    fn refusal(attributes: &[MediaAttribute]) -> String {
        match into_changes(&Config::for_tests(), attributes) {
            Err(AppError::Unprocessable(message)) => message,
            other => panic!("expected a 422, got {other:?}"),
        }
    }

    #[test]
    fn a_json_array_and_a_form_index_map_read_alike() {
        let wanted = Some(vec![entry("a", "one"), entry("b", "two")]);
        assert_eq!(
            read(json!([
                {"id": "a", "description": "one"},
                {"id": "b", "description": "two", "thumbnail": "ignored"},
            ]))
            .unwrap(),
            wanted,
            "the JSON spelling"
        );
        assert_eq!(
            read(json!({
                "1": {"id": "b", "description": "two"},
                "0": {"id": "a", "description": "one"},
            }))
            .unwrap(),
            wanted,
            "the index map orders by its numeric keys"
        );
        assert_eq!(read(Value::Null).unwrap(), None, "null is absent");
        assert!(
            read(json!(["not an object"])).is_err(),
            "entries are objects"
        );
    }

    #[test]
    fn entries_become_checked_changes() {
        let id = uuid::Uuid::now_v7();
        let changes = into_changes(
            &Config::for_tests(),
            &[MediaAttribute {
                id: id.to_string(),
                description: Some("alt".to_owned()),
                focus: Some(" 0.5 , -1 ".to_owned()),
            }],
        )
        .expect("a valid entry passes");
        assert_eq!(
            changes,
            vec![MediaAttributeChange {
                id,
                description: Some("alt".to_owned()),
                focus: Some(FocusChange::Set { x: 0.5, y: -1.0 }),
            }]
        );
        let cleared = into_changes(
            &Config::for_tests(),
            &[MediaAttribute {
                id: id.to_string(),
                description: None,
                focus: Some(String::new()),
            }],
        )
        .expect("an empty focus clears");
        assert_eq!(cleared[0].focus, Some(FocusChange::Clear));
        assert_eq!(cleared[0].description, None, "an absent text keeps");
    }

    #[test]
    fn bad_entries_are_refused() {
        let id = uuid::Uuid::now_v7().to_string();
        assert_eq!(
            refusal(&[entry("nonsense", "x")]),
            UNKNOWN_ATTRIBUTE_MEDIA,
            "an id that is no id"
        );
        let mut config = Config::for_tests();
        config.max_media_description = 3;
        let long = into_changes(&config, &[entry(&id, "four")]);
        assert!(
            matches!(long, Err(AppError::Unprocessable(_))),
            "alt text over the limit"
        );
        assert_eq!(
            refusal(&[MediaAttribute {
                id,
                description: None,
                focus: Some("2,0".to_owned()),
            }]),
            FOCUS_INVALID,
            "a focal point outside [-1, 1]"
        );
    }
}
