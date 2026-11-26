//! The Mastodon `Tag` entity: one hashtag.
//!
//! It is what the Status entity's `tags` array and the tag endpoints
//! (`GET /api/v1/tags/{name}`, its `follow` and `unfollow`, and
//! `GET /api/v1/followed_tags`) render.
//!
//! The short shape is Pleroma's `StatusView.build_tags`
//! (`lib/pleroma/web/mastodon_api/views/status_view.ex`): the two keys
//! `name` and `url`, and nothing else — `url` is the instance's tag page.
//! [`Tag::from_hashtag`] and [`Tag::new`] build it, so a status render stays
//! two keys. The full shape the tag endpoints answer with adds `history` —
//! the last seven UTC days' usage counts, Mastodon's
//! `TagSerializer#serialize_hashtag_history`, strings and all — and, only
//! when a viewer is present, `following`. Like the
//! [`Mention`](crate::entities::mention::Mention) render it is pure — a
//! [`Hashtag`] row plus the counts becomes the entity without a database or
//! a request.

use serde::Serialize;

use crate::models::hashtag::Hashtag;

/// The `tags` entry of a Status entity — one hashtag, in the two-key shape
/// Mastodon clients read.
///
/// Built by [`Tag::with_history`] it is instead the full form a tag endpoint
/// answers with: the two keys plus `history`, and `following` when a viewer
/// is present. The combined search's v2 hashtag list is the full form with
/// the `history` of a fresh week left empty, which still renders the key as
/// `[]` — see [`history`](Self::history).
#[derive(Debug, Serialize)]
pub struct Tag {
    /// The tag name without the leading `#`: lowercase as stored in a
    /// Status's `tags`, the casing of the tag's first use in the full form.
    pub name: String,
    /// The instance's tag page for this name: `{base_url}/tags/{name}`.
    pub url: String,
    /// The last seven days of usage, newest first; `None` (the status
    /// render, [`Tag::new`] and [`Tag::from_hashtag`]) omits the key, and
    /// `Some` — even an empty `Vec` — renders it, so the v2 hashtag search
    /// can answer `"history": []`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<TagHistory>>,
    /// Whether the requesting user follows this tag; omitted when no viewer
    /// is present (the anonymous render has no following to state).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub following: Option<bool>,
    /// Whether the requesting user features this tag on their profile
    /// (newer Mastodon's `featuring`); omitted when no viewer is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub featuring: Option<bool>,
}

/// One entry of a Tag's `history`: one UTC day of usage, all counts written
/// as strings — Mastodon's `TagHistory` serializer renders integers with
/// `to_s`.
#[derive(Clone, Debug, Serialize)]
pub struct TagHistory {
    /// The unix timestamp (seconds) of the UTC midnight that starts the day,
    /// as text.
    pub day: String,
    /// Statuses using the tag that day, as text.
    pub uses: String,
    /// Distinct accounts using the tag that day, as text.
    pub accounts: String,
}

impl TagHistory {
    /// One day's entry: the seconds-epoch `day` of its UTC midnight with
    /// `uses` statuses by `accounts` distinct authors, all rendered as
    /// strings.
    #[must_use]
    pub fn new(day: i64, uses: i64, accounts: i64) -> Self {
        Self {
            day: day.to_string(),
            uses: uses.to_string(),
            accounts: accounts.to_string(),
        }
    }
}

impl Tag {
    /// Renders `hashtag` as the status render's tag entity, its page under
    /// `base_url` (the instance origin, i.e.
    /// [`Config::base_url`](crate::config::Config::base_url)): the two keys,
    /// no history, no following.
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
            history: None,
            following: None,
            featuring: None,
        }
    }

    /// This tag showing `name` instead of the lowercase one, as Mastodon's
    /// `TagSerializer#name` shows the display name of the tag's first use.
    /// The `url` keeps the lowercase name it was built from.
    #[must_use]
    pub fn with_display_name(mut self, name: &str) -> Self {
        name.clone_into(&mut self.name);
        self
    }

    /// This tag with its `featuring` flag set: `Some` renders the key,
    /// `None` leaves it out.
    #[must_use]
    pub const fn with_featuring(mut self, featuring: Option<bool>) -> Self {
        self.featuring = featuring;
        self
    }

    /// The full form a tag endpoint answers with: the two keys, the given
    /// `history` (newest first), and `following` — `Some` only when a viewer
    /// is present, since an anonymous render omits the key entirely. The
    /// `history` key itself always renders here, an empty `Vec` as `[]`:
    /// that is the shape the v2 combined search's hashtag list uses.
    #[must_use]
    pub fn with_history(
        base_url: &str,
        name: &str,
        history: Vec<TagHistory>,
        following: Option<bool>,
    ) -> Self {
        Self {
            name: name.to_owned(),
            url: format!("{base_url}/tags/{name}"),
            history: Some(history),
            following,
            featuring: None,
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
            display_name: None,
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
        assert_eq!(
            keys,
            ["name", "url"],
            "the status render carries no history or following"
        );
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

    /// The full form for a tag named `name` with one history entry, for the
    /// viewer `following` (`None` standing for no viewer at all).
    fn full(name: &str, following: Option<bool>) -> Value {
        let history = vec![TagHistory::new(1_700_000_000, 12, 3)];
        serde_json::to_value(Tag::with_history(TEST_BASE_URL, name, history, following)).unwrap()
    }

    #[test]
    fn the_full_form_adds_history_and_following() {
        assert_eq!(
            full("rust", Some(true)),
            json!({
                "name": "rust",
                "url": format!("{TEST_BASE_URL}/tags/rust"),
                "history": [{
                    "day": "1700000000",
                    "uses": "12",
                    "accounts": "3",
                }],
                "following": true,
            }),
            "every count is a string, and `following` rides with a viewer"
        );
    }

    #[test]
    fn a_display_name_changes_the_name_and_not_the_url() {
        let value = serde_json::to_value(
            Tag::with_history(TEST_BASE_URL, "r5smoke", Vec::new(), None)
                .with_display_name("R5Smoke"),
        )
        .unwrap();
        assert_eq!(value["name"], "R5Smoke", "the first use's casing");
        assert_eq!(
            value["url"],
            format!("{TEST_BASE_URL}/tags/r5smoke"),
            "the url stays lowercase"
        );
    }

    #[test]
    fn the_status_render_ignores_the_display_name() {
        let hashtag = Hashtag {
            id: uuid::Uuid::now_v7(),
            name: "r5smoke".to_owned(),
            display_name: Some("R5Smoke".to_owned()),
            inserted_at: chrono::Utc::now(),
        };
        let value = serde_json::to_value(Tag::from_hashtag(TEST_BASE_URL, &hashtag)).unwrap();
        assert_eq!(value["name"], "r5smoke", "a status's tags stay lowercase");
    }

    #[test]
    fn featuring_renders_only_when_set() {
        let plain = serde_json::to_value(Tag::new(TEST_BASE_URL, "rust")).unwrap();
        assert!(plain.get("featuring").is_none(), "no viewer, no key");
        let featured =
            serde_json::to_value(Tag::new(TEST_BASE_URL, "rust").with_featuring(Some(true)))
                .unwrap();
        assert_eq!(featured["featuring"], true, "a viewer's flag rides along");
    }

    #[test]
    fn a_viewerless_full_tag_omits_following_but_keeps_history() {
        let value = full("rust", None);
        assert!(
            value.get("following").is_none(),
            "no viewer, no `following` key at all"
        );
        assert_eq!(
            value["history"][0]["uses"], "12",
            "history is rendered whether or not someone is watching"
        );
    }

    /// The v2 combined search's hashtag shape: the full form with no usage
    /// counts still carries the `history` key, as the empty list.
    #[test]
    fn an_empty_full_history_renders_as_the_empty_list() {
        let value =
            serde_json::to_value(Tag::with_history(TEST_BASE_URL, "rust", Vec::new(), None))
                .unwrap();
        assert_eq!(
            value,
            json!({
                "name": "rust",
                "url": format!("{TEST_BASE_URL}/tags/rust"),
                "history": [],
            }),
            "name, url and the empty history — the search render, no `following`"
        );
    }
}
