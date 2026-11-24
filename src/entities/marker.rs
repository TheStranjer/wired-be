//! The Mastodon v1 `Marker` entity: one timeline's read position as clients
//! render it.
//!
//! The reference shape is Pleroma's `MarkerView.render("markers.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/marker_view.ex`): `last_read_id`,
//! `version`, `updated_at` and the Pleroma-only `pleroma` object holding
//! `unread_count`. `version` is Pleroma's `lock_version`, and `updated_at` is
//! written in the Mastodon time format of `docs/ARCHITECTURE.md` decision 5
//! rather than Pleroma's offset-less `NaiveDateTime` text.
//!
//! [`Marker::from_row`] is pure (decision 3): it renders a
//! [`models::marker`](crate::models/marker) row plus the unread count the
//! handler computed — the marker's owner, its timeline name and the row's
//! version are the model's, and the count is the notifications model's, so
//! neither is this shape's business. The response object that keys these
//! entities by timeline name is assembled by
//! [`crate::api::markers`], which is what decides *which* markers appear.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::models::marker::Marker as MarkerRow;

/// The `pleroma` half of a marker: the counts Pleroma adds to Mastodon's
/// entity.
#[derive(Debug, Serialize)]
pub struct MarkerPleroma {
    /// How many entries of the timeline its account has not read yet; `0`
    /// for `home`, and for `notifications` until the notifications ticket
    /// fills the count in.
    pub unread_count: i64,
}

/// The Mastodon v1 `Marker` entity, the value side of the `{timeline: marker}`
/// object both `/api/v1/markers` endpoints answer with.
#[derive(Debug, Serialize)]
pub struct Marker {
    /// Id text of the newest entry the account has read, verbatim as stored.
    pub last_read_id: String,
    /// How many times the marker has been written (Pleroma's `lock_version`).
    pub version: i32,
    /// When the position last moved, in the Mastodon time format
    /// (decision 5).
    #[serde(with = "crate::util::time::serde_mastodon_time")]
    pub updated_at: DateTime<Utc>,
    /// Pleroma's extension object, this entity's only non-Mastodon key.
    pub pleroma: MarkerPleroma,
}

impl Marker {
    /// Renders `row` as the entity, with `unread_count` in the `pleroma` half.
    #[must_use]
    pub fn from_row(row: &MarkerRow, unread_count: i64) -> Self {
        Self {
            last_read_id: row.last_read_id.clone(),
            version: row.version,
            updated_at: row.updated_at,
            pleroma: MarkerPleroma { unread_count },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::{test_fixtures::json_keys, util::time::parse_time};

    /// The instant the marker last moved, in the wire format.
    const UPDATED: &str = "2024-01-31T12:34:56.789Z";
    /// Every JSON key the entity must emit, sorted, for an exact key-set
    /// check.
    const EXPECTED_KEYS: [&str; 4] = ["last_read_id", "pleroma", "updated_at", "version"];

    /// A stored `home` marker at `UPDATED`, never rewritten.
    fn row() -> MarkerRow {
        MarkerRow {
            id: Uuid::now_v7(),
            user_id: Uuid::now_v7(),
            timeline: crate::models::marker::HOME.to_owned(),
            last_read_id: "01934567-89ab-7cde-f012-3456789abcde".to_owned(),
            version: 0,
            updated_at: parse_time(UPDATED).expect("the fixture time parses"),
        }
    }

    /// `row` rendered with `unread_count`; entities always serialize.
    fn render(unread_count: i64) -> Value {
        serde_json::to_value(Marker::from_row(&row(), unread_count)).expect("an entity serializes")
    }

    #[test]
    fn a_marker_renders_the_mastodon_entity_with_pleroma_s_count() {
        let value = render(5);
        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exactly the keys");
        assert_eq!(
            value["last_read_id"], "01934567-89ab-7cde-f012-3456789abcde",
            "the position verbatim"
        );
        assert_eq!(value["version"], 0, "the stored counter");
        assert_eq!(value["updated_at"], UPDATED, "the time in Mastodon format");
        assert_eq!(
            value["pleroma"],
            json!({"unread_count": 5}),
            "pleroma holds only the count"
        );
    }

    #[test]
    fn a_rewritten_marker_renders_its_version_and_new_stamp() {
        let stored = MarkerRow {
            version: 37,
            last_read_id: String::new(),
            updated_at: parse_time("2024-02-01T00:00:00Z").expect("the fixture time parses"),
            ..row()
        };
        let value = serde_json::to_value(Marker::from_row(&stored, 0))
            .expect("an empty position serializes");
        assert_eq!(value["version"], 37, "the counter as often as it moved");
        assert_eq!(value["last_read_id"], "", "an empty position is empty");
        assert_eq!(
            value["updated_at"], "2024-02-01T00:00:00.000Z",
            "whole seconds are padded to milliseconds"
        );
        assert_eq!(value["pleroma"]["unread_count"], 0, "and a zero count");
    }
}
