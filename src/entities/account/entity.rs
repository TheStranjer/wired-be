//! The `Account` entity's own fields: the struct every render of this
//! module tree answers with.
//!
//! Declaration order is emission order, so this file is the entity's shape
//! as clients see it: the core Mastodon keys, the two keys only an owner
//! view or the mutes listing adds, and the `pleroma` extension block last.
//! The pure renders that fill the fields live in `entity_render`, the
//! [`AccountSource`] editing block and its [`MuteExpiresAt`] value in
//! `source`, and the [`PleromaMetadata`] extension in `pleroma`.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

#[cfg(doc)]
use crate::{config::Config, models::user::User};

use super::{AccountSource, Field, JsonBool, MuteExpiresAt, PleromaMetadata};
use crate::entities::custom_emoji::CustomEmoji;

/// The Mastodon `Account` entity (core fields).
///
/// Build one with [`Account::from_user`] or [`Account::from_user_for`]; its
/// [`Serialize`] impl emits exactly the JSON keys listed in
/// `docs/ARCHITECTURE.md` decision 5's image of the Mastodon API, in
/// declaration order, plus the `pleroma` extension block last.
#[derive(Debug, Serialize)]
pub struct Account {
    /// The user id as the hyphenated lowercase `UUIDv7` string (decision 4).
    pub id: String,
    /// The part of the nickname before the `@`, e.g. `alice`.
    pub username: String,
    /// The handle as clients know it: the stored nickname, which already
    /// carries `@domain` for remote users (decision 7).
    pub acct: String,
    /// The fully-qualified `nick@domain` handle (Pleroma's
    /// `User.full_nickname`): a local account gains [`Config::domain`].
    pub fqn: String,
    /// The display name; empty means "same as the nickname" by the model's
    /// convention (decision 7), rendered as stored here.
    pub display_name: String,
    /// Whether follow requests need manual approval.
    pub locked: JsonBool,
    /// Whether the account is flagged as automated.
    pub bot: JsonBool,
    /// Whether the account opted into discovery.
    pub discoverable: JsonBool,
    /// Whether this is a group actor: true when `actor_type` is `Group`.
    pub group: JsonBool,
    /// When the account was created, in the Mastodon time format
    /// (decision 5).
    #[serde(with = "crate::util::time::serde_mastodon_time")]
    pub created_at: DateTime<Utc>,
    /// The profile note as sanitized HTML.
    pub note: String,
    /// The human-facing profile URL: the stored one, or the `ActivityPub`
    /// actor id when the row has neither (Pleroma's `user.uri || user.ap_id`).
    pub url: String,
    /// The `ActivityPub` actor id.
    pub uri: String,
    /// The avatar URL, the instance's `DEFAULT_AVATAR_PATH` fallback when unset.
    pub avatar: String,
    /// The non-animated avatar: the same URL (no animation split yet).
    pub avatar_static: String,
    /// The banner URL, the instance's `DEFAULT_BANNER_PATH` fallback when unset.
    pub header: String,
    /// The non-animated banner: the same URL (no animation split yet).
    pub header_static: String,
    /// Number of followers (stored counter), or `0` for a viewer other
    /// than the owner when the count is hidden
    /// (`visible_followers_count` decides).
    pub followers_count: i32,
    /// Number of follows (stored counter), or `0` for a viewer other than
    /// the owner when the count is hidden
    /// (`visible_following_count` decides).
    pub following_count: i32,
    /// Number of posted statuses (stored counter).
    pub statuses_count: i32,
    /// Date of the newest status as `YYYY-MM-DD`, or `null`.
    pub last_status_at: Option<String>,
    /// The custom emojis the display name, note and profile fields use, as
    /// [`CustomEmoji`] entities. The pure renders leave it `[]`;
    /// [`Account::with_emojis`] fills it from the rows the caller batch-loaded
    /// (see `services::account_emojis`).
    pub emojis: Vec<CustomEmoji>,
    /// Profile metadata, in storage order.
    pub fields: Vec<Field>,
    /// The account this one moved to; `null` until the render resolves
    /// [`moved_to_ap_id`](User::moved_to_ap_id) to a stored row and
    /// [`Account::with_moved`] attaches it (non-recursive: the nested
    /// account is a plain [`Account::from_user`] render, so its own
    /// `moved` is always `null`).
    pub moved: Option<Value>,
    /// The account's `also_known_as` actor ids as stored: the one Pleroma
    /// extension Mastodon clients expect for alias management, present as
    /// a top-level key only on the owner view [`Account::for_owner`]
    /// renders (Pleroma nests it in the `pleroma` block; this instance
    /// exposes it here and keeps the block to its own keys), so for every
    /// other viewer serde omits the key entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub also_known_as: Option<Vec<String>>,
    /// Mastodon's `mute_expires_at`: the expiry of the viewer's mute of this
    /// account, present as a top-level key only on the entries
    /// [`Account::with_mute_expires_at`] built for `GET /api/v1/mutes` and
    /// omitted by every other render. Its value is the mute's
    /// [`expires_at`](crate::models::mute::Mute::expires_at) in the Mastodon
    /// time format (decision 5), or JSON `null` for an indefinite mute —
    /// which is why the key is a [`MuteExpiresAt`] and not a bare
    /// `Option<String>`: the listing has to render "no expiry" as `null`, and
    /// an ordinary render has to render nothing at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mute_expires_at: Option<MuteExpiresAt>,
    /// Whether the account is deactivated: `is_active` inverted.
    pub suspended: JsonBool,
    /// Whether the account is rate-limited by moderation; nothing limits
    /// accounts yet, so this is always false.
    pub limited: JsonBool,
    /// Mastodon's `source` object: the profile as its owner edits it (see
    /// [`AccountSource`]). Only ever set by `verify_credentials` and
    /// `update_credentials` through [`Account::for_owner`]; every other
    /// render leaves it `None`, which serde omits entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<AccountSource>,
    /// Pleroma's extension block: the privacy flags, the role flags and
    /// the account-state keys (see [`PleromaMetadata`]).
    pub pleroma: PleromaMetadata,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config, entities::account::test_support::*, models::user::Flag,
        util::time::parse_time,
    };

    #[test]
    fn local_user_renders_the_full_snapshot() {
        let config = Config::for_tests();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        alice.local = Flag::new(true);
        alice.name = "Alice".to_owned();
        alice.bio = "<p>hello</p>".to_owned();
        alice.avatar_url = Some(format!("{BASE}/avatar.png"));
        alice.banner_url = Some(format!("{BASE}/banner.png"));
        alice.profile_url = Some(format!("{BASE}/@alice"));
        alice.locked = Flag::new(true);
        alice.bot = Flag::new(true);
        alice.discoverable = Flag::new(true);
        alice.fields = serde_json::json!([
            {"name": "site", "value": "<a href=\"https://alice.example\">alice.example</a>"}
        ]);
        alice.followers_count = 5;
        alice.following_count = 6;
        alice.statuses_count = 7;
        alice.last_status_at = Some(parse_time("2024-02-01T09:08:07.500Z").unwrap());

        let value = render(&alice, &config);
        // The two `json!` blocks stay one level deep each: nesting the
        // extension inside the core snapshot overflows the macro
        // recursion limit.
        let mut snapshot = serde_json::json!({
            "id": ACCOUNT_ID,
            "username": "alice",
            "acct": "alice",
            "fqn": "alice@test.example",
            "display_name": "Alice",
            "locked": true,
            "bot": true,
            "discoverable": true,
            "group": false,
            "created_at": "2024-01-31T12:34:56.789Z",
            "note": "<p>hello</p>",
            "url": "https://test.example/@alice",
            "uri": "https://test.example/users/alice",
            "avatar": "https://test.example/avatar.png",
            "avatar_static": "https://test.example/avatar.png",
            "header": "https://test.example/banner.png",
            "header_static": "https://test.example/banner.png",
            "followers_count": 5,
            "following_count": 6,
            "statuses_count": 7,
            "last_status_at": "2024-02-01",
            "emojis": [],
            "fields": [{
                "name": "site",
                "value": "<a href=\"https://alice.example\">alice.example</a>",
                "verified_at": null,
            }],
            "moved": null,
            "suspended": false,
            "limited": false,
        });
        snapshot["pleroma"] = pleroma_block("https://test.example/users/alice", Some(true));
        assert_eq!(
            value, snapshot,
            "local account snapshot, with the pleroma block an anonymous \
             viewer sees: the column defaults as stored, no owner-only keys"
        );
        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exact key set");
        assert_eq!(
            json_keys(&pleroma_of(&value)),
            PLEROMA_KEYS_FOR_OTHERS,
            "an anonymous viewer sees no owner-only pleroma key"
        );
    }

    #[test]
    fn remote_user_renders_from_the_stored_row() {
        let config = Config::for_tests();
        let mut bob = base_user("bob@remote.example", "https://remote.example/users/bob");
        bob.domain = Some("remote.example".to_owned());
        bob.name = "Bob".to_owned();
        bob.followers_count = 12;
        bob.following_count = 3;
        bob.statuses_count = 48;

        let value = render(&bob, &config);
        let mut snapshot = serde_json::json!({
            "id": ACCOUNT_ID,
            "username": "bob",
            "acct": "bob@remote.example",
            "fqn": "bob@remote.example",
            "display_name": "Bob",
            "locked": false,
            "bot": false,
            "discoverable": false,
            "group": false,
            "created_at": "2024-01-31T12:34:56.789Z",
            "note": "",
            "url": "https://remote.example/users/bob",
            "uri": "https://remote.example/users/bob",
            "avatar": "https://test.example/media/default/avi.png",
            "avatar_static": "https://test.example/media/default/avi.png",
            "header": "https://test.example/media/default/banner.png",
            "header_static": "https://test.example/media/default/banner.png",
            "followers_count": 12,
            "following_count": 3,
            "statuses_count": 48,
            "last_status_at": null,
            "emojis": [],
            "fields": [],
            "moved": null,
            "suspended": false,
            "limited": false,
        });
        snapshot["pleroma"] = pleroma_block("https://remote.example/users/bob", None);
        assert_eq!(
            value, snapshot,
            "remote account snapshot: counts come from the stored counters, \
             the handle already carries the host, url falls back to ap_id, \
             and the chat capability the actor never advertised is null"
        );
    }

    #[test]
    fn group_flag_follows_the_actor_type() {
        let config = Config::for_tests();
        let mut club = base_user("club@remote.example", "https://remote.example/groups/club");
        club.actor_type = "Group".to_owned();
        let value = render(&club, &config);
        assert_eq!(value["group"], true, "a Group actor renders group: true");
        assert_eq!(
            value["bot"], false,
            "group is its own flag, not the stored bot one"
        );

        club.actor_type = "Service".to_owned();
        let service = render(&club, &config);
        assert_eq!(service["group"], false, "other actor types are not groups");
    }

    #[test]
    fn timestamps_render_in_the_mastodon_formats() {
        let config = Config::for_tests();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        assert_eq!(
            render(&alice, &config)["created_at"],
            "2024-01-31T12:34:56.789Z",
            "created_at carries milliseconds and Z"
        );

        alice.inserted_at = parse_time("2024-01-31T12:34:56Z").unwrap();
        alice.last_status_at = Some(parse_time("2025-12-31T23:59:59.999Z").unwrap());
        let value = render(&alice, &config);
        assert_eq!(
            value["created_at"], "2024-01-31T12:34:56.000Z",
            "a whole second is padded to .000Z"
        );
        assert_eq!(
            value["last_status_at"], "2025-12-31",
            "last_status_at is a bare date"
        );

        alice.last_status_at = None;
        assert_eq!(
            render(&alice, &config)["last_status_at"],
            Value::Null,
            "no statuses means JSON null"
        );
    }

    #[test]
    fn deactivated_users_are_suspended() {
        let config = Config::for_tests();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        assert_eq!(
            render(&alice, &config)["suspended"],
            false,
            "an active account is not suspended"
        );

        alice.is_active = Flag::new(false);
        let value = render(&alice, &config);
        assert_eq!(
            value["suspended"], true,
            "a deactivated account renders suspended: true"
        );
        assert_eq!(value["limited"], false, "limited stays false either way");
    }
}
