//! The Mastodon `Announcement` entity: one instance-wide notice as clients
//! render it.
//!
//! The reference shape is Pleroma's `Pleroma.Announcement.render_json/2`
//! (`lib/pleroma/announcement.ex`, reached through
//! `lib/pleroma/web/mastodon_api/views/announcement_view.ex`); the JSON keys
//! are emitted exactly as that function names them, in its order, with the
//! `pleroma` block last. [`Announcement::from_row`] is pure (decision 3): it
//! renders a [`models::announcement`](crate::models::announcement) row and the
//! viewer's dismissal of it, so nothing here touches the database.
//!
//! Two keys are constants here because nothing behind them is implemented.
//! `mentions`, `statuses`, `tags`, `emojis` and `reactions` are the lists
//! Pleroma's renderer hard-codes empty — announcements are plain text, they
//! resolve no accounts, carry no posts, and take no reactions until an admin
//! can add them — so they travel as the empty arrays the ticket names.
//!
//! `read` is the viewer's own state: [`from_row`](Announcement::from_row)
//! takes it as an argument because the row does not know who is looking, and
//! `false` is what a request with no viewer at all (an administration list,
//! which dismissals are not computed for) renders.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::models::announcement::Announcement as AnnouncementRow;

/// The `pleroma` extension object of an [`Announcement`]: the keys of
/// Pleroma's renderer that Mastodon's entity has no place for.
#[derive(Debug, Serialize)]
pub struct PleromaMetadata {
    /// `raw_content`: the notice as its author wrote it, before the sanitizer
    /// produced [`content`](Announcement::content) from it.
    pub raw_content: String,
}

/// The Mastodon `Announcement` entity.
///
/// Build one with [`Announcement::from_row`]; its [`Serialize`] impl emits
/// `id`, `content`, `starts_at`, `ends_at`, `all_day`, `published_at`,
/// `updated_at`, `read`, `mentions`, `statuses`, `tags`, `emojis`,
/// `reactions` and the `pleroma` block.
#[derive(Debug, Serialize)]
pub struct Announcement {
    /// The notice id as the hyphenated lowercase `UUIDv7` string (decision 4).
    pub id: String,
    /// The notice as sanitized HTML, straight from the row.
    pub content: String,
    /// When the notice became active, in the Mastodon time format
    /// (decision 5); `null` when its window has no start.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub starts_at: Option<DateTime<Utc>>,
    /// When it stops being active, same format; `null` when it never does.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub ends_at: Option<DateTime<Utc>>,
    /// Whether clients show the window's dates without their times.
    pub all_day: bool,
    /// When the notice was made, in the Mastodon time format: the row's
    /// [`inserted_at`](AnnouncementRow::inserted_at) under Mastodon's name.
    #[serde(with = "crate::util::time::serde_mastodon_time")]
    pub published_at: DateTime<Utc>,
    /// When it last changed, in the same format.
    #[serde(with = "crate::util::time::serde_mastodon_time")]
    pub updated_at: DateTime<Utc>,
    /// Whether the requesting user dismissed it; `false` when there is no
    /// viewer to dismiss anything for (see the module docs).
    pub read: bool,
    /// Accounts the notice mentions; always `[]` (see the module docs).
    pub mentions: Vec<Value>,
    /// Posts the notice carries; always `[]`.
    pub statuses: Vec<Value>,
    /// Hashtags the notice mentions; always `[]`.
    pub tags: Vec<Value>,
    /// Custom emojis used in the notice; always `[]`.
    pub emojis: Vec<Value>,
    /// Emoji reactions readers have added; always `[]`.
    pub reactions: Vec<Value>,
    /// Pleroma's extension block.
    pub pleroma: PleromaMetadata,
}

impl Announcement {
    /// Renders `row` for a viewer who has dismissed it when `read` is set.
    ///
    /// `read` is an argument rather than a column because dismissal is per
    /// account (see [`active_for_user`](crate::models::announcement::active_for_user));
    /// a caller with no viewer — an administration list — passes `false`.
    #[must_use]
    pub fn from_row(row: &AnnouncementRow, read: bool) -> Self {
        Self {
            id: row.id.to_string(),
            content: row.content.clone(),
            starts_at: row.starts_at,
            ends_at: row.ends_at,
            all_day: row.all_day,
            published_at: row.inserted_at,
            updated_at: row.updated_at,
            read,
            mentions: Vec::new(),
            statuses: Vec::new(),
            tags: Vec::new(),
            emojis: Vec::new(),
            reactions: Vec::new(),
            pleroma: PleromaMetadata {
                raw_content: row.raw_content.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::{test_fixtures::json_keys, util::time::parse_time};

    /// The notice id, as a client sees it.
    const NOTICE_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// When the notice was created, which the entity calls `published_at`.
    const PUBLISHED: &str = "2024-01-31T12:34:56.789Z";
    /// When it last changed.
    const UPDATED: &str = "2024-02-01T08:00:00.000Z";
    /// The first instant it is active.
    const STARTS: &str = "2024-02-01T09:00:00.000Z";
    /// The last instant it is active.
    const ENDS: &str = "2024-02-08T09:00:00.000Z";

    /// Every JSON key the entity must emit, sorted, for an exact key-set check.
    const EXPECTED_KEYS: [&str; 14] = [
        "all_day",
        "content",
        "emojis",
        "ends_at",
        "id",
        "mentions",
        "pleroma",
        "published_at",
        "reactions",
        "read",
        "starts_at",
        "statuses",
        "tags",
        "updated_at",
    ];

    /// An instant from its ISO 8601 text.
    fn at(text: &str) -> DateTime<Utc> {
        parse_time(text).expect("the fixture times are timestamps")
    }

    /// A stored notice with both window bounds set, `all_day` and the given
    /// text in both of its spellings.
    fn row() -> AnnouncementRow {
        AnnouncementRow {
            id: Uuid::parse_str(NOTICE_ID).expect("the fixture id is an id"),
            content: "<p>Scheduled maintenance</p>".to_owned(),
            raw_content: "Scheduled maintenance".to_owned(),
            starts_at: Some(at(STARTS)),
            ends_at: Some(at(ENDS)),
            all_day: true,
            inserted_at: at(PUBLISHED),
            updated_at: at(UPDATED),
        }
    }

    /// `row` rendered with `read`, as a JSON value; these structs always
    /// serialize.
    fn render(row: &AnnouncementRow, read: bool) -> Value {
        serde_json::to_value(Announcement::from_row(row, read)).expect("an entity serializes")
    }

    #[test]
    fn a_notice_renders_the_mastodon_entity() {
        let value = render(&row(), false);

        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exactly the keys");
        assert_eq!(value["id"], NOTICE_ID, "the id as hyphenated text");
        assert_eq!(
            value["content"], "<p>Scheduled maintenance</p>",
            "the sanitized html the row holds"
        );
        assert_eq!(value["starts_at"], STARTS, "the window, in mastodon times");
        assert_eq!(value["ends_at"], ENDS, "at both ends");
        assert_eq!(value["all_day"], true, "the stored date-only flag");
        assert_eq!(
            value["published_at"], PUBLISHED,
            "inserted_at under mastodon's name"
        );
        assert_eq!(value["updated_at"], UPDATED, "and the update stamp");
        assert_eq!(value["read"], false, "a viewer who dismissed nothing");
        assert_eq!(
            value["pleroma"]["raw_content"], "Scheduled maintenance",
            "the source text in the pleroma block"
        );
        assert_eq!(
            json_keys(&value["pleroma"]),
            ["raw_content"],
            "which carries that key alone"
        );
    }

    #[test]
    fn the_five_lookups_are_present_empty_arrays() {
        let value = render(&row(), false);

        for key in ["mentions", "statuses", "tags", "emojis", "reactions"] {
            assert_eq!(value[key], json!([]), "`{key}` is there, and empty");
        }
    }

    #[test]
    fn an_open_window_renders_both_bounds_as_null() {
        let mut stored = row();
        stored.starts_at = None;
        stored.ends_at = None;
        stored.all_day = false;
        let value = render(&stored, false);

        assert_eq!(value["starts_at"], Value::Null, "null, not absent");
        assert_eq!(value["ends_at"], Value::Null, "at both open ends");
        assert_eq!(value["all_day"], false, "and times are shown");
    }

    #[test]
    fn a_half_open_window_renders_the_bound_it_has() {
        let mut started = row();
        started.ends_at = None;
        let value = render(&started, true);

        assert_eq!(value["starts_at"], STARTS, "the bound that is stored");
        assert_eq!(
            value["ends_at"],
            Value::Null,
            "and null for the one that is not"
        );
        assert_eq!(value["read"], true, "the viewer's own dismissal");
    }

    #[test]
    fn read_is_the_argument_and_no_viewer_means_false() {
        // The row itself knows nothing about viewers: two renderings of the
        // same row differ only in the flag the caller passed.
        let unread = render(&row(), false);
        let read = render(&row(), true);

        assert_eq!(unread["read"], false, "no viewer, nothing dismissed");
        assert_eq!(read["read"], true, "the viewer dismissed it");
        assert_eq!(
            json_keys(&unread),
            json_keys(&read),
            "and the key set does not depend on the viewer either way"
        );
    }

    #[test]
    fn empty_text_renders_as_an_empty_string() {
        let mut stored = row();
        stored.content = String::new();
        stored.raw_content = String::new();
        let value = render(&stored, false);

        assert_eq!(value["content"], "", "an empty notice is empty, not null");
        assert_eq!(value["pleroma"]["raw_content"], "", "in both spellings");
    }
}
