//! The grouped-notification entities: one `NotificationGroup` and the
//! `{accounts, statuses, notification_groups}` envelope that carries it.
//!
//! The reference shape is Pleroma's `NotificationView.render("grouped_index.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/notification_view.ex`) and the
//! ticket's field list, which the two agree on: each group answers its
//! `group_key`, the whole-group `notifications_count`, `type`,
//! `most_recent_notification_id`, `page_min_id`, `page_max_id`,
//! `latest_page_notification_at`, up to eight deduplicated
//! `sample_account_ids`, and `status_id` only when the group names a
//! status. The ids are `UUIDv7` text (decision 4) and the time is the
//! Mastodon format (decision 5).
//!
//! Like Pleroma's view, the envelope expands each group's sample actors
//! into full [`Account`] entities and each group's status into a full
//! [`Status`] entity, both collected once for the whole page; the group
//! entries themselves carry only ids. [`NotificationGroup::from_group`] is
//! pure (decision 3): it renders a [`models::notification`](crate::models::notification)
//! aggregate, deduplicating the sample exactly where Pleroma's view runs
//! its `Enum.uniq_by` — over the up-to-eight newest members' actors, in
//! that order.

use serde::Serialize;

use crate::{
    entities::{account::Account, status::Status},
    models::notification::GroupInfo,
    util::time::mastodon_time,
};

/// One `notification_groups` entry of the grouped listing and the
/// single-group read.
#[derive(Debug, Serialize)]
pub struct NotificationGroup {
    /// The group's key: a stored `group_key` (`<type>-<status_id>`, the
    /// emoji appended for a reaction) or the derived `ungrouped-<id>`.
    pub group_key: String,
    /// How many undismissed notifications the group holds.
    pub notifications_count: i64,
    /// The group's notification type.
    #[serde(rename = "type")]
    pub notification_type: String,
    /// The group's newest notification id, the cursor the listing pages on.
    pub most_recent_notification_id: String,
    /// The id of the oldest notification the page has for this group.
    pub page_min_id: String,
    /// The id of the newest notification the page has for this group:
    /// one grouped page carries a whole group, so this is
    /// [`Self::most_recent_notification_id`] (Pleroma's bounds select
    /// `max(n.id)` twice for exactly that reason).
    pub page_max_id: String,
    /// When the group's newest notification was created, in the Mastodon
    /// time format (decision 5).
    pub latest_page_notification_at: String,
    /// The ids of up to eight distinct actors among the group's newest
    /// notifications, newest first; each names an entry of the envelope's
    /// [`accounts`](GroupedNotifications::accounts).
    pub sample_account_ids: Vec<String>,
    /// The status the group is about, as id text; absent when the
    /// notifications name none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_id: Option<String>,
}

impl NotificationGroup {
    /// Renders one group: the aggregate `info` under the key `group_key`
    /// (the request's own spelling, echoed like Pleroma's view echoes the
    /// computed key). The sample deduplicates actors while keeping the
    /// newest-first order, and stays capped at eight because the model
    /// sampled no more.
    #[must_use]
    pub fn from_group(group_key: &str, info: &GroupInfo) -> Self {
        let mut sample_account_ids: Vec<String> = Vec::new();
        for actor in &info.sample_account_ids {
            let text = actor.to_string();
            if !sample_account_ids.contains(&text) {
                sample_account_ids.push(text);
            }
        }
        let most_recent_notification_id = info.most_recent_id.to_string();
        Self {
            group_key: group_key.to_owned(),
            notifications_count: info.notifications_count,
            notification_type: info.notification_type.clone(),
            page_min_id: info.page_min_id.to_string(),
            page_max_id: most_recent_notification_id.clone(),
            latest_page_notification_at: mastodon_time(&info.latest_page_notification_at),
            most_recent_notification_id,
            sample_account_ids,
            status_id: info.status_id.map(|id| id.to_string()),
        }
    }
}

/// The `{accounts, statuses, notification_groups}` envelope both grouped
/// reads answer with.
///
/// It carries the groups themselves, plus every sample account and group
/// status expanded once for the whole response (Pleroma's
/// `grouped_index.json`, which collects `accounts` and `statuses` beside
/// `notification_groups`).
#[derive(Debug, Serialize)]
pub struct GroupedNotifications {
    /// The sample actors of every group in the response, deduplicated and
    /// in group order.
    pub accounts: Vec<Account>,
    /// The statuses every group names, deduplicated and rendered for the
    /// reader; a status the reader may no longer see is simply not here,
    /// while the group keeps its `status_id`.
    pub statuses: Vec<Status>,
    /// The page's groups, newest first.
    pub notification_groups: Vec<NotificationGroup>,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::models::notification::GroupInfo;

    /// A group aggregate of two favouriting actors, for the renders.
    fn info() -> GroupInfo {
        GroupInfo {
            notifications_count: 2,
            notification_type: "favourite".to_owned(),
            most_recent_id: Uuid::parse_str("01890b9a-2c1e-7a5b-8c9d-1e2f3a4b5c6d")
                .expect("fixture id"),
            page_min_id: Uuid::parse_str("01890b9a-2c1d-7f3a-9e4b-7c2d5a8b1e3f")
                .expect("fixture id"),
            latest_page_notification_at: chrono::NaiveDate::from_ymd_opt(2026, 1, 2)
                .expect("date")
                .and_hms_opt(3, 4, 5)
                .expect("time")
                .and_utc(),
            status_id: Some(
                Uuid::parse_str("01890b9a-2c1f-7d7e-9a0b-3c4d5e6f7a8b").expect("fixture id"),
            ),
            sample_account_ids: vec![Uuid::nil(), Uuid::nil(), Uuid::max()],
        }
    }

    #[test]
    fn a_group_renders_every_ticket_field() {
        let value = json!(NotificationGroup::from_group("favourite-1", &info()));
        // `serde_json` objects are key-sorted without `preserve_order`, so
        // the assertion names the set of keys the ticket lists.
        let expected: Vec<&str> = [
            "group_key",
            "latest_page_notification_at",
            "most_recent_notification_id",
            "notifications_count",
            "page_max_id",
            "page_min_id",
            "sample_account_ids",
            "status_id",
            "type",
        ]
        .to_vec();
        let keys: Vec<&String> = value.as_object().expect("an object").keys().collect();
        assert_eq!(keys, expected, "exactly the ticket's fields");
        assert_eq!(value["type"], "favourite", "the type key");
        assert_eq!(value["notifications_count"], 2, "a whole-number count");
        assert_eq!(
            value["most_recent_notification_id"], "01890b9a-2c1e-7a5b-8c9d-1e2f3a4b5c6d",
            "ids as hyphenated lowercase text (decision 4)"
        );
        assert_eq!(
            value["page_max_id"], value["most_recent_notification_id"],
            "the page extreme is the group's newest id"
        );
        assert_eq!(
            value["latest_page_notification_at"], "2026-01-02T03:04:05.000Z",
            "the Mastodon time format (decision 5)"
        );
        assert_eq!(
            value["status_id"], "01890b9a-2c1f-7d7e-9a0b-3c4d5e6f7a8b",
            "the named status"
        );
    }

    #[test]
    fn samples_deduplicate_and_the_status_key_hides_when_absent() {
        let group = NotificationGroup::from_group("favourite-none", &info());
        assert_eq!(
            group.sample_account_ids.len(),
            2,
            "the repeated actor is named once, newest order kept"
        );
        assert_eq!(
            group.sample_account_ids.first().map(String::as_str),
            Some(Uuid::nil().to_string().as_str()),
            "and the first is the newest member's"
        );

        let mut statusless = info();
        statusless.status_id = None;
        let value: Value = json!(NotificationGroup::from_group("favourite-none", &statusless));
        assert!(
            value.get("status_id").is_none(),
            "a status-less group omits the key entirely"
        );
    }

    #[test]
    fn the_envelope_carries_groups_beside_accounts_and_statuses() {
        let envelope = GroupedNotifications {
            accounts: Vec::new(),
            statuses: Vec::new(),
            notification_groups: vec![NotificationGroup::from_group("k", &info())],
        };
        let value = json!(envelope);
        assert_eq!(
            value["accounts"],
            json!([]),
            "the expansion lists serialize even when empty"
        );
        assert_eq!(value["statuses"], json!([]), "both of them");
        assert_eq!(
            value["notification_groups"]
                .as_array()
                .expect("a list")
                .len(),
            1,
            "and the groups ride along"
        );
    }
}
