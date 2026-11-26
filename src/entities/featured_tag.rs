//! The Mastodon `FeaturedTag` entity: one hashtag an account features on
//! its profile.
//!
//! Mastodon's `REST::FeaturedTagSerializer`: `id`, `name`, `url` (the
//! account's page of its posts under the tag), `statuses_count` (a string)
//! and `last_status_at` (a `YYYY-MM-DD` date, `null` before the first use).
//! The render is pure: a stored [`Entry`] plus the account's nickname and the
//! instance origin.

use serde::Serialize;

use crate::models::featured_tag::Entry;

/// One featured tag as the API renders it.
#[derive(Debug, Serialize)]
pub struct FeaturedTag {
    /// The `featured_tags` row id, as text.
    pub id: String,
    /// The tag name in the casing its owner wrote, without the leading `#`.
    pub name: String,
    /// The account's page of posts under this tag.
    pub url: String,
    /// How many public posts use the tag, as text (Mastodon's `to_s`).
    pub statuses_count: String,
    /// The date of the last post under the tag, `null` when there is none.
    pub last_status_at: Option<String>,
}

impl FeaturedTag {
    /// Renders `entry` for the account `nickname` on the instance at
    /// `base_url`.
    #[must_use]
    pub fn new(base_url: &str, nickname: &str, entry: &Entry) -> Self {
        Self {
            id: entry.id.to_string(),
            name: entry.name.clone(),
            url: format!("{base_url}/@{nickname}/tagged/{}", entry.tag_name),
            statuses_count: entry.statuses_count.to_string(),
            last_status_at: entry.last_status_at.map(|at| at.date_naive().to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::config::TEST_BASE_URL;

    /// An entry of `count` uses, the last on 2024-03-05, for tag `name`.
    fn entry(name: &str, count: i64, used: bool) -> Entry {
        let at = Utc.with_ymd_and_hms(2024, 3, 5, 23, 59, 0).unwrap();
        Entry {
            id: Uuid::nil(),
            user_id: Uuid::nil(),
            tag_id: Uuid::nil(),
            name: name.to_owned(),
            tag_name: name.to_lowercase(),
            statuses_count: count,
            last_status_at: used.then_some(at),
            inserted_at: at,
        }
    }

    #[test]
    fn the_entity_has_the_mastodon_shape() {
        let rendered = FeaturedTag::new(TEST_BASE_URL, "alice", &entry("rust", 3, true));
        assert_eq!(
            serde_json::to_value(rendered).unwrap(),
            json!({
                "id": Uuid::nil().to_string(),
                "name": "rust",
                "url": format!("{TEST_BASE_URL}/@alice/tagged/rust"),
                "statuses_count": "3",
                "last_status_at": "2024-03-05",
            }),
            "counts are strings, the last use a plain date"
        );
    }

    #[test]
    fn the_name_keeps_its_casing_and_the_url_slug_does_not() {
        let rendered = FeaturedTag::new(TEST_BASE_URL, "alice", &entry("FedLabR4", 1, true));
        let value = serde_json::to_value(rendered).unwrap();
        assert_eq!(value["name"], "FedLabR4", "the owner's casing");
        assert_eq!(
            value["url"],
            format!("{TEST_BASE_URL}/@alice/tagged/fedlabr4"),
            "the slug is the lowercase tag"
        );
    }

    #[test]
    fn an_unused_tag_has_a_null_date() {
        let rendered = FeaturedTag::new(TEST_BASE_URL, "alice", &entry("rust", 0, false));
        let value = serde_json::to_value(rendered).unwrap();
        assert_eq!(value["last_status_at"], json!(null), "no use yet");
        assert_eq!(value["statuses_count"], "0");
    }
}
