//! The Mastodon `Relationship` entity, with Pleroma's `subscribing` alias.
//!
//! The reference shape is Pleroma's
//! `AccountView.do_render("relationship.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/account_view.ex`); the JSON keys
//! are emitted exactly as the ticket lists them. Like every entity
//! (decision 3) this struct is pure data: the database work that decides
//! the flags lives in [`crate::models::relationship`], whose single
//! inherent `impl` block provides [`Relationship::load_many`] here so
//! callers say `Relationship::load_many(…)` while the queries
//! stay in the models layer.
//!
//! The flags the `follows` and `account_notes` tables can answer are
//! computed: `following`, `requested`, `followed_by`, `showing_reblogs`,
//! `notifying` and its Pleroma alias `subscribing`, and `note` (the
//! viewer's stored comment, the empty string where they have none).
//! `domain_blocking` is computed from the `user_domain_blocks` table
//! (see [`crate::models::domain_block`]): true exactly when the target is
//! a remote account on a domain the viewer blocks. `endorsed` is computed
//! from the `endorsements` table (see [`crate::models::endorsement`]):
//! true exactly when the viewer pins the target to their profile. Every
//! other flag renders `false` until its own ticket stores the data behind
//! it and adds one batched query to the loader:
//!
//! - `blocking` and `blocked_by`: the blocks ticket.
//! - `muting` and `muting_notifications`: the mutes ticket.

use std::collections::HashMap;

use serde::Serialize;
use uuid::Uuid;

use crate::entities::account::JsonBool;

/// Relationships keyed by target account id.
///
/// The shape
/// [`Account::from_user_for`](crate::entities::account::Account::from_user_for)
/// takes as its optional preload: build one from
/// [`Relationship::load_many`] by zipping the requested ids with the
/// relationships that came back.
pub type RelationshipMap = HashMap<Uuid, Relationship>;

/// The relationship of one viewer to one target account.
///
/// Constructed by the batch loader in [`crate::models::relationship`];
/// [`Relationship::unconnected`] holds the single definition of what a
/// viewer with nothing recorded about a target sees, so the loader starts
/// from it and overwrites only the flags each source query computed.
#[derive(Clone, Debug, Serialize)]
pub struct Relationship {
    /// The target account's id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    pub id: String,
    /// Whether the viewer follows the target: an accepted `viewer → target`
    /// row in `follows`.
    pub following: JsonBool,
    /// Whether the viewer's follow of the target is still waiting to be
    /// answered: a pending `viewer → target` row.
    pub requested: JsonBool,
    /// Whether the viewer endorses the target: an `endorsements` row of
    /// theirs naming it (see [`crate::models::endorsement`]); `false`
    /// while the viewer pins nothing.
    pub endorsed: JsonBool,
    /// Whether the target follows the viewer: an accepted
    /// `target → viewer` row.
    pub followed_by: JsonBool,
    /// Whether the viewer blocks the target: `false` until the blocks
    /// ticket stores blocks.
    pub blocking: JsonBool,
    /// Whether the target blocks the viewer: `false` until the blocks
    /// ticket stores blocks.
    pub blocked_by: JsonBool,
    /// Whether the viewer mutes the target: `false` until the mutes ticket
    /// stores mutes.
    pub muting: JsonBool,
    /// Whether the viewer mutes only the target's notifications: `false`
    /// until the mutes ticket stores notification mutes.
    pub muting_notifications: JsonBool,
    /// Whether the viewer wants notifications for every target post: the
    /// `notify` toggle of the `viewer → target` follow row, `false` with no row.
    pub notifying: JsonBool,
    /// Whether the viewer still sees the target's reblogs: the
    /// `show_reblogs` toggle of the `viewer → target` follow row, `true`
    /// with no row (nothing was ever hidden).
    pub showing_reblogs: JsonBool,
    /// Whether the viewer blocks the target's whole domain: `true` exactly
    /// when the target is a remote account on one of the viewer's blocked
    /// domains (see [`crate::models::domain_block`]); a local account is
    /// never domain-blocked.
    pub domain_blocking: JsonBool,
    /// The viewer's private note on the target: the stored comment of
    /// their `account_notes` row for it, the empty string when they have
    /// none (see [`crate::models::account_note`]).
    pub note: String,
    /// Pleroma's alias of [`notifying`](Self::notifying): the same value,
    /// named as Pleroma's `subscribing` renders it.
    pub subscribing: JsonBool,
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    /// Every JSON key the entity must emit, sorted: the ticket's field list
    /// is the key set, and `Value` maps are content-compared, not
    /// order-compared.
    const EXPECTED_KEYS: [&str; 14] = [
        "blocked_by",
        "blocking",
        "domain_blocking",
        "endorsed",
        "followed_by",
        "following",
        "id",
        "muting",
        "muting_notifications",
        "note",
        "notifying",
        "requested",
        "showing_reblogs",
        "subscribing",
    ];

    /// The target id every fixture relationship is about.
    fn target_id() -> Uuid {
        Uuid::parse_str("01934567-89ab-7cde-f012-3456789abcde").unwrap()
    }

    /// The rendered object's keys, sorted, for exact key-set assertions.
    fn json_keys(value: &Value) -> Vec<&str> {
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
    fn unconnected_relationships_render_the_default_flags() {
        let value = serde_json::to_value(Relationship::unconnected(target_id())).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "id": "01934567-89ab-7cde-f012-3456789abcde",
                "following": false,
                "requested": false,
                "endorsed": false,
                "followed_by": false,
                "blocking": false,
                "blocked_by": false,
                "muting": false,
                "muting_notifications": false,
                "notifying": false,
                "showing_reblogs": true,
                "domain_blocking": false,
                "note": "",
                "subscribing": false,
            }),
            "every flag at its documented default: showing_reblogs the one \
             that starts true, note the empty string"
        );
        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exact key set");
    }

    #[test]
    fn computed_flags_override_the_defaults() {
        let value = serde_json::to_value(Relationship {
            following: JsonBool(true),
            requested: JsonBool(true),
            followed_by: JsonBool(true),
            notifying: JsonBool(true),
            showing_reblogs: JsonBool(false),
            subscribing: JsonBool(true),
            ..Relationship::unconnected(target_id())
        })
        .unwrap();
        assert_eq!(value["following"], true, "JsonBool renders a plain bool");
        assert_eq!(value["requested"], true, "and so does every other flag");
        assert_eq!(value["followed_by"], true, "flag");
        assert_eq!(value["notifying"], true, "flag");
        assert_eq!(value["showing_reblogs"], false, "flag");
        assert_eq!(value["subscribing"], true, "flag");
        assert_eq!(
            value["endorsed"], false,
            "flags the fixture did not set keep their default"
        );
        assert_eq!(
            value["note"], "",
            "and a relationship with no stored note keeps the empty string"
        );
        assert_eq!(json_keys(&value), EXPECTED_KEYS, "same key set");
    }

    #[test]
    fn the_id_is_the_hyphenated_lowercase_uuid_text() {
        let value = serde_json::to_value(Relationship::unconnected(target_id())).unwrap();
        assert_eq!(
            value["id"], "01934567-89ab-7cde-f012-3456789abcde",
            "decision 4: ids serialize as lowercase hyphenated text"
        );
    }
}
