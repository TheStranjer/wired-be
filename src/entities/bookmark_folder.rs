//! The Pleroma `BookmarkFolder` entity: one account's bookmark folder as
//! `/api/v1/pleroma/bookmark_folders` renders it.
//!
//! The reference shape is Pleroma's `BookmarkFolderView.render("show.json", …)`
//! (`lib/pleroma/web/pleroma_api/views/bookmark_folder_view.ex`) and its
//! `BookmarkFolder` schema
//! (`lib/pleroma/web/api_spec/schemas/bookmark_folder.ex`): the four keys
//! `id`, `name`, `emoji` and `emoji_url`. `id` is Pleroma's flake id
//! replaced by the row's `UUIDv7` as lowercase hyphenated text (decision 4).
//!
//! `emoji` is the folder's emoji as its owner stored it — one unicode emoji
//! or a `:shortcode:` naming a local custom emoji — or `null` for a folder
//! wearing none. `emoji_url` is the custom emoji's image URL when the
//! stored `:shortcode:` matches one this instance has, `null` otherwise;
//! deciding that is a `custom_emojis` read, so the URL arrives as a
//! parameter of [`BookmarkFolder::from_row`] and this entity stays pure
//! (`docs/ARCHITECTURE.md` decision 3) — see
//! [`crate::api::pleroma_bookmark_folders`] for the lookup.

use serde::Serialize;

use crate::models::bookmark_folder::BookmarkFolder as BookmarkFolderRow;

/// The Pleroma `BookmarkFolder` entity.
///
/// Build one with [`BookmarkFolder::from_row`]; its [`Serialize`] impl
/// emits `id`, `name`, `emoji` and `emoji_url`.
#[derive(Debug, Serialize)]
pub struct BookmarkFolder {
    /// The folder id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    pub id: String,
    /// The user-defined name, verbatim as its owner wrote it.
    pub name: String,
    /// The folder's emoji as stored — a unicode emoji or a `:shortcode:` —
    /// or `null`.
    pub emoji: Option<String>,
    /// The image URL of the local custom emoji the `emoji` shortcode names,
    /// or `null` for a unicode emoji, an unmatched shortcode or no emoji.
    pub emoji_url: Option<String>,
}

impl BookmarkFolder {
    /// Renders `row` as the entity, with `emoji_url` — the image URL of
    /// the custom emoji the row's emoji names, `None` when it matches
    /// none (see the [module docs](self) for who looks that up).
    #[must_use]
    pub fn from_row(row: &BookmarkFolderRow, emoji_url: Option<&str>) -> Self {
        Self {
            id: row.id.to_string(),
            name: row.name.clone(),
            emoji: row.emoji.clone(),
            emoji_url: emoji_url.map(str::to_owned),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use uuid::Uuid;

    use super::*;

    /// The folder id, as a client sees it.
    const FOLDER_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// The image a matching custom emoji contributes.
    const CAT_URL: &str = "https://test.example/emoji/cats/cat.png";

    /// Every JSON key the entity must emit, sorted, for an exact key-set
    /// check.
    const EXPECTED_KEYS: [&str; 4] = ["emoji", "emoji_url", "id", "name"];

    /// A stored folder named `name` wearing `emoji`.
    fn row(emoji: Option<&str>) -> BookmarkFolderRow {
        BookmarkFolderRow {
            id: Uuid::parse_str(FOLDER_ID).expect("the fixture id is an id"),
            user_id: Uuid::now_v7(),
            name: name(),
            emoji: emoji.map(str::to_owned),
            inserted_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// The folder's name.
    fn name() -> String {
        "Read later".to_owned()
    }

    /// `row` rendered as a JSON value; entities always serialize.
    fn render(row: &BookmarkFolderRow, emoji_url: Option<&str>) -> Value {
        serde_json::to_value(BookmarkFolder::from_row(row, emoji_url))
            .expect("an entity serializes")
    }

    /// The rendered object's keys, sorted, for exact key-set assertions.
    fn json_keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("folders render objects")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn a_plain_folder_renders_the_four_keys_with_two_nulls() {
        let value = render(&row(None), None);

        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exactly the keys");
        assert_eq!(value["id"], FOLDER_ID, "the id as hyphenated text");
        assert_eq!(value["name"], "Read later", "the name verbatim");
        assert_eq!(value["emoji"], Value::Null, "a folder may wear none");
        assert_eq!(value["emoji_url"], Value::Null, "and then has no image");
    }

    #[test]
    fn a_unicode_emoji_carries_no_url() {
        let value = render(&row(Some("📚")), None);

        assert_eq!(value["emoji"], "📚", "the emoji as stored");
        assert_eq!(value["emoji_url"], Value::Null, "unicode has no image");
    }

    #[test]
    fn a_matched_shortcode_carries_its_image_url() {
        let value = render(&row(Some(":cat:")), Some(CAT_URL));

        assert_eq!(value["emoji"], ":cat:", "the shortcode as stored");
        assert_eq!(value["emoji_url"], CAT_URL, "and the looked-up image");
    }

    #[test]
    fn an_unmatched_shortcode_keeps_its_text_and_loses_its_url() {
        let value = render(&row(Some(":gone:")), None);

        assert_eq!(
            value["emoji"], ":gone:",
            "the stored text is never rewritten"
        );
        assert_eq!(
            value["emoji_url"],
            Value::Null,
            "and no match means no image"
        );
    }
}
