//! The group keys the grouped reads merge notifications on: whether a
//! type groups at all ([`is_groupable`]), the stored key computed at
//! insert ([`grouped_key`]), the `{group_key}` path-segment reader
//! ([`select_group`]) and what it answers ([`GroupSelector`]), the two
//! constants every grouped read spells with ([`UNGROUNDED_PREFIX`] and
//! [`SAMPLE_SIZE`]), and the SQL sample cap built from the latter.

use uuid::Uuid;

use crate::util::id::parse_id;

use super::{GROUPABLE_TYPES, TYPE_EMOJI_REACTION};

/// The prefix of the unique key an ungrouped notification answers to:
/// `ungrouped-<id>`, Pleroma's `ungrouped_group_key/1`.
pub const UNGROUNDED_PREFIX: &str = "ungrouped-";

/// How many of a group's most recent members feed its
/// `sample_account_ids` (Pleroma's `@notification_group_sample_limit`,
/// Mastodon's group sample size).
pub const SAMPLE_SIZE: usize = 8;

/// Whether `notification_type` is one of the status-keyed types the API
/// groups, and so gets a stored [`grouped_key`].
#[must_use]
pub fn is_groupable(notification_type: &str) -> bool {
    GROUPABLE_TYPES.contains(&notification_type)
}

/// The stored group key of `notification_type`, or `None` when the type is
/// not groupable.
///
/// `<type>-<status_id>`, `<type>-none` for a notification that names no
/// status, and `<type>-<status_id>-<emoji>` for a reaction. A reaction row
/// that stored no emoji falls back to the plain status key, so a key never
/// ends in a dangling `-`.
#[must_use]
pub fn grouped_key(
    notification_type: &str,
    status_id: Option<Uuid>,
    emoji: Option<&str>,
) -> Option<String> {
    if !is_groupable(notification_type) {
        return None;
    }
    let subject = status_id.map_or_else(|| "none".to_owned(), |id| id.to_string());
    let mut key = format!("{notification_type}-{subject}");
    if notification_type == TYPE_EMOJI_REACTION
        && let Some(emoji) = emoji
    {
        key.push('-');
        key.push_str(emoji);
    }
    Some(key)
}

/// The [`GroupSelector`] a client's `{group_key}` path segment names: the
/// row behind an `ungrouped-<id>` spelling, or the stored group key itself
/// (Pleroma's two `get_notification_group*` clauses).
#[must_use]
pub fn select_group(group_key: &str) -> GroupSelector {
    group_key
        .strip_prefix(UNGROUNDED_PREFIX)
        .and_then(parse_id)
        .map_or_else(
            || GroupSelector::GroupKey(group_key.to_owned()),
            GroupSelector::Notification,
        )
}

/// Which notifications one `{group_key}` addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupSelector {
    /// The unique key of one notification: `ungrouped-<id>`, matched on the
    /// row id like Pleroma's `"ungrouped-" <> notification_id` clauses.
    Notification(Uuid),
    /// A stored group key, matched on the `group_key` column.
    GroupKey(String),
}

/// The array-subscript slice text that caps a sample array at
/// [`SAMPLE_SIZE`], built from the constant so the SQL and the documented
/// cap cannot drift.
pub(super) fn sample_slice() -> String {
    let size = i64::try_from(SAMPLE_SIZE).unwrap_or(i64::MAX);
    format!("[1:{size}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::notification::{TYPE_FAVOURITE, TYPE_REBLOG};

    /// A `UUIDv7` for key assertions.
    fn id() -> Uuid {
        Uuid::now_v7()
    }

    #[test]
    fn grouped_keys_carry_the_type_and_the_status() {
        let status = id();
        assert_eq!(
            grouped_key(TYPE_FAVOURITE, Some(status), None).as_deref(),
            Some(format!("favourite-{status}").as_str()),
            "a favourite groups on its status"
        );
        assert_eq!(
            grouped_key(TYPE_REBLOG, Some(status), None).as_deref(),
            Some(format!("reblog-{status}").as_str()),
            "and so does a reblog"
        );
        assert_eq!(
            grouped_key(TYPE_FAVOURITE, None, None).as_deref(),
            Some("favourite-none"),
            "a status-less groupable row groups under none"
        );
        assert_eq!(
            grouped_key(TYPE_EMOJI_REACTION, Some(status), Some("🎉")).as_deref(),
            Some(format!("pleroma:emoji_reaction-{status}-🎉").as_str()),
            "the reaction key carries the emoji"
        );
        assert_eq!(
            grouped_key(TYPE_EMOJI_REACTION, Some(status), None).as_deref(),
            Some(format!("pleroma:emoji_reaction-{status}").as_str()),
            "and without an emoji it is the plain status key"
        );
    }

    #[test]
    fn ungroupable_types_have_no_stored_key() {
        for name in [
            "mention",
            "follow",
            "follow_request",
            "move",
            "update",
            "poll",
            "status",
            "quote",
            "quoted_update",
        ] {
            assert!(!is_groupable(name), "{name} is not groupable");
            assert_eq!(
                grouped_key(name, Some(id()), None),
                None,
                "{name} stores no key"
            );
        }
        assert!(is_groupable(TYPE_FAVOURITE), "the ticketed types group");
        assert!(is_groupable(TYPE_REBLOG));
        assert!(is_groupable(TYPE_EMOJI_REACTION));
    }

    #[test]
    fn selectors_split_the_ungrouped_prefix() {
        let notification = id();
        assert_eq!(
            select_group(&format!("{UNGROUNDED_PREFIX}{notification}")),
            GroupSelector::Notification(notification),
            "ungrouped-<id> is the row"
        );
        assert_eq!(
            select_group(&format!("favourite-{notification}")),
            GroupSelector::GroupKey(format!("favourite-{notification}")),
            "a grouped key stays a key"
        );
        assert_eq!(
            select_group("ungrouped-not-an-id"),
            GroupSelector::GroupKey("ungrouped-not-an-id".to_owned()),
            "and a broken prefix matches nothing rather than guessing"
        );
    }
}
