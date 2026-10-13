//! The Mastodon `Tag` entity: one hashtag of a status, as the Status
//! entity's `tags` array renders it.
//!
//! The reference shape is Pleroma's `StatusView.build_tags`
//! (`lib/pleroma/web/mastodon_api/views/status_view.ex`): the two keys
//! `name` and `url`, and nothing else — `url` is the instance's tag page.
//! (Mastodon's `history` and Pleroma's `following` belong to the tag
//! endpoints, not to this rendering, and land with their own ticket.) Like
//! the [`Mention`](crate::entities::mention::Mention) render it is pure — a
//! [`Hashtag`] row becomes the entity without a database or a request.

use serde::Serialize;

use crate::models::hashtag::Hashtag;

/// The `tags` entry of a Status entity — one hashtag, in the two-key shape
/// Mastodon clients read.
#[derive(Debug, Serialize)]
pub struct Tag {
    /// The tag name, as stored: lowercase, without the leading `#`.
    pub name: String,
    /// The instance's tag page for this name: `{base_url}/tags/{name}`.
    pub url: String,
}

impl Tag {
    /// Renders `hashtag` as the tag entity, its page under `base_url`
    /// (the instance origin, i.e.
    /// [`Config::base_url`](crate::config::Config::base_url)).
    #[must_use]
    pub fn from_hashtag(base_url: &str, hashtag: &Hashtag) -> Self {
        Self::new(base_url, &hashtag.name)
    }

    /// The entity for one already-normalized tag `name`: the two keys, the
    /// url the same `{base_url}/tags/{name}` shape
    /// [`crate::util::entities::hashtag_html`] links inside the content.
    #[must_use]
    pub fn new(base_url: &str, name: &str) -> Self {
        Self {
            name: name.to_owned(),
            url: format!("{base_url}/tags/{name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::config::TEST_BASE_URL;

    /// The rendered JSON of a stored tag named `name`, at a fixed id and
    /// time — only the two entity fields are read from the row.
    fn rendered(name: &str) -> Value {
        let hashtag = Hashtag {
            id: uuid::Uuid::now_v7(),
            name: name.to_owned(),
            inserted_at: chrono::Utc::now(),
        };
        serde_json::to_value(Tag::from_hashtag(TEST_BASE_URL, &hashtag)).unwrap()
    }

    #[test]
    fn a_tag_renders_its_name_and_tag_page_url() {
        assert_eq!(
            rendered("rust"),
            json!({
                "name": "rust",
                "url": format!("{TEST_BASE_URL}/tags/rust"),
            }),
            "the two keys, the url this instance's tag page"
        );
    }

    #[test]
    fn the_entity_holds_nothing_but_name_and_url() {
        let value = rendered("snake_case_1");
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["name", "url"], "history/following are later");
        assert_eq!(value["name"], "snake_case_1", "the name is as stored");
    }

    #[test]
    fn new_and_from_hashtag_agree() {
        assert_eq!(
            serde_json::to_value(Tag::new(TEST_BASE_URL, "日本語")).unwrap()["url"],
            format!("{TEST_BASE_URL}/tags/日本語"),
            "the raw-name builder makes the same page url, unicode kept"
        );
    }
}
