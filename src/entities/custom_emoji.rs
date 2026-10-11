//! The Mastodon `CustomEmoji` entity: one shortcode rendered for clients.
//!
//! The reference shape is Pleroma's `CustomEmojiView.render("show.json")`
//! (`lib/pleroma/web/mastodon_api/views/custom_emoji_view.ex`); the JSON
//! keys are emitted exactly as that view names them, plus Pleroma's
//! `tags` extension. [`CustomEmoji::from_model`] is pure (decision 3): it
//! renders a [`custom_emojis`](crate::models::custom_emoji) row with no
//! database and no config, because both URLs the entity carries are
//! already stored on the row.
//!
//! Differences from the Pleroma view, per the ticket: file-backed pack
//! emojis always know their url, while a row here may have no still
//! variant, so `static_url` is `null` when the column is (clients fall
//! back to `url`), and `category` is the stored pack name or `null`, not
//! the tag list joined with commas.

use serde::Serialize;

use crate::models::custom_emoji::CustomEmoji as EmojiRow;

/// The Mastodon `CustomEmoji` entity.
///
/// Build one with [`CustomEmoji::from_model`]; its [`Serialize`] impl
/// emits `shortcode`, `url`, `static_url`, `visible_in_picker`,
/// `category` and the Pleroma extension's `tags`.
#[derive(Debug, Serialize)]
pub struct CustomEmoji {
    /// The shortcode, without surrounding colons.
    pub shortcode: String,
    /// The emoji image url: the stored [`image_url`](EmojiRow::image_url).
    pub url: String,
    /// The still (non-animated) variant; `null` when the row stores none
    /// and clients fall back to [`url`](Self::url).
    pub static_url: Option<String>,
    /// Whether clients list the emoji in their pickers.
    pub visible_in_picker: bool,
    /// The pack name; `null` when the emoji is ungrouped.
    pub category: Option<String>,
    /// Picker tags (Pleroma's extension), empty when the emoji has none.
    pub tags: Vec<String>,
}

impl CustomEmoji {
    /// Renders `row` as a `CustomEmoji` entity: every field passes
    /// through from the stored row, `url` taking the image url.
    #[must_use]
    pub fn from_model(row: &EmojiRow) -> Self {
        Self {
            shortcode: row.shortcode.clone(),
            url: row.image_url.clone(),
            static_url: row.static_url.clone(),
            visible_in_picker: row.visible_in_picker,
            category: row.category.clone(),
            tags: row.tags.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::util::time::parse_time;

    /// The hyphenated lowercase id every fixture row carries.
    const EMOJI_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// The instance origin fixture URLs point at.
    const BASE: &str = "https://test.example";

    /// Every JSON key the entity must emit, sorted.
    const EXPECTED_KEYS: [&str; 6] = [
        "category",
        "shortcode",
        "static_url",
        "tags",
        "url",
        "visible_in_picker",
    ];

    /// 2024-01-31T12:34:56.789Z, the instant every fixture row is
    /// inserted and last updated at.
    fn stamped() -> DateTime<Utc> {
        parse_time("2024-01-31T12:34:56.789Z").unwrap()
    }

    /// A local [`EmojiRow`] named `shortcode` in the `custom` pack:
    /// visible, enabled, with a still url and one tag.
    fn base_row(shortcode: &str) -> EmojiRow {
        EmojiRow {
            id: Uuid::parse_str(EMOJI_ID).unwrap(),
            shortcode: shortcode.to_owned(),
            domain: None,
            image_url: format!("{BASE}/emoji/custom/{shortcode}.gif"),
            static_url: Some(format!("{BASE}/emoji/custom/{shortcode}.png")),
            category: Some("custom".to_owned()),
            tags: vec![shortcode.to_owned()],
            visible_in_picker: true,
            disabled: false,
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// `row` rendered to a JSON value; these structs always serialize.
    fn render(row: &EmojiRow) -> serde_json::Value {
        serde_json::to_value(CustomEmoji::from_model(row)).unwrap()
    }

    /// The rendered object's keys, sorted, for exact key-set assertions.
    fn json_keys(value: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn a_full_row_renders_the_entity_snapshot() {
        let value = render(&base_row("blobcat"));
        assert_eq!(
            value,
            serde_json::json!({
                "shortcode": "blobcat",
                "url": "https://test.example/emoji/custom/blobcat.gif",
                "static_url": "https://test.example/emoji/custom/blobcat.png",
                "visible_in_picker": true,
                "category": "custom",
                "tags": ["blobcat"],
            }),
            "complete emoji snapshot"
        );
        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exact key set");
    }

    #[test]
    fn absent_pack_still_and_tags_render_as_null_and_empty() {
        let mut row = base_row("plain");
        row.static_url = None;
        row.category = None;
        row.tags = Vec::new();
        row.visible_in_picker = false;

        let value = render(&row);
        assert_eq!(
            value,
            serde_json::json!({
                "shortcode": "plain",
                "url": "https://test.example/emoji/custom/plain.gif",
                "static_url": null,
                "visible_in_picker": false,
                "category": null,
                "tags": [],
            }),
            "the ungrouped emoji keeps every key"
        );
        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exact key set");
    }

    #[test]
    fn the_image_url_is_the_entity_url() {
        let mut row = base_row("cat");
        row.image_url = "https://remote.example/emoji/cat.gif".to_owned();
        let value = render(&row);
        assert_eq!(
            value["url"], "https://remote.example/emoji/cat.gif",
            "url is the stored image url, not a derived one"
        );
        assert_eq!(
            value["static_url"], "https://test.example/emoji/custom/cat.png",
            "static_url stays the stored still url, even on another host"
        );
    }
}
