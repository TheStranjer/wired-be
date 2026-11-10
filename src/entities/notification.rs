//! The notification entities: the flat view and the grouped view.
//!
//! The flat [`Notification`] is what the v1 and Pleroma-API reads answer
//! with; [`NotificationGroup`] is one entry of the grouped listing, which
//! the `{accounts, statuses, notification_groups}` [`GroupedNotifications`]
//! envelope carries.
//!
//! The reference shape of the flat entity is Pleroma's
//! `NotificationView.render("show.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/notification_view.ex`): the row's
//! `id`, `group_key` (stored key or derived `ungrouped-<id>`), `type` and
//! `created_at`, the actor expanded to a full [`Account`], the `pleroma`
//! block with its `is_muted` and `is_seen` flags, and the named status and
//! reaction emoji when there is one to render. [`Notification::from_row`]
//! is pure (decision 3): the caller runs every read that fills the
//! expansions.
//!
//! The reference shape of the grouped entries is Pleroma's
//! `NotificationView.render("grouped_index.json", …)`
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
use serde_json::Value;

use crate::{
    entities::{
        account::{Account, JsonBool},
        status::Status,
    },
    models::notification::{GroupInfo, Notification as NotificationRow, UNGROUNDED_PREFIX},
    util::time::mastodon_time,
};

/// One notification as the flat (v1 and Pleroma-API) reads render it.
///
/// Pleroma's `NotificationView.render("show.json", …)` — the row's `id`,
/// `group_key` (the stored key, or the derived `ungrouped-<id>`), `type`
/// and Mastodon-format `created_at`, the actor expanded to a full
/// [`Account`], the `pleroma` block with its `is_muted` and `is_seen`
/// flags, and, when the notification names one, the named status rendered
/// (and the reaction's `emoji`). A status the reader may no longer see
/// leaves the [`status`](Self::status) key out entirely, like Pleroma's
/// `put_status` of a nothing render.
#[derive(Debug, Serialize)]
pub struct Notification {
    /// The notification's id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    pub id: String,
    /// The group this notification reads under: its stored `group_key`,
    /// or the derived `ungrouped-<id>` (Pleroma's `group_key/1`).
    pub group_key: String,
    /// The Mastodon notification type as stored; the model row spells the
    /// same field `event_type` for the same reason this one may not read
    /// as `notification_type`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// When the notification was created, in the Mastodon time format
    /// (decision 5).
    pub created_at: String,
    /// The account that caused the notification, expanded as the reader
    /// sees it.
    pub account: Account,
    /// The status the notification is about, rendered as the reader sees
    /// it; absent when it names none or the reader may not see it. The
    /// [`Status`] entity is not cheap to duplicate and several
    /// notifications of one read set can name the same status, so this is
    /// its serialized shape.
    ///
    /// [`Status`]: crate::entities::status::Status
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Value>,
    /// The emoji of a `pleroma:emoji_reaction`; absent for every other
    /// type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    /// Pleroma's extension block (see [`NotificationPleroma`]).
    pub pleroma: NotificationPleroma,
}

/// The `pleroma` block of a [`Notification`]: Pleroma's two per-notification
/// flags.
#[derive(Debug, Serialize)]
pub struct NotificationPleroma {
    /// Whether the reader mutes the actor (Pleroma's `User.mutes?`).
    pub is_muted: JsonBool,
    /// Whether the reader has read past this notification (the row's
    /// `seen` flag).
    pub is_seen: JsonBool,
}

impl Notification {
    /// Renders `row` beside its expansions: `account` is the actor as the
    /// reader sees it, `status` the rendered named status (`None` when the
    /// row names none or the reader may not see it), and `is_muted`
    /// whether the reader mutes the actor. Pure (decision 3): every
    /// database read that fills these parts happens in the caller.
    #[must_use]
    pub fn from_row(
        row: &NotificationRow,
        account: Account,
        status: Option<Value>,
        is_muted: bool,
    ) -> Self {
        let group_key = row
            .group_key
            .clone()
            .unwrap_or_else(|| format!("{UNGROUNDED_PREFIX}{}", row.id));
        Self {
            id: row.id.to_string(),
            group_key,
            event_type: row.event_type.clone(),
            created_at: mastodon_time(&row.inserted_at),
            account,
            status,
            emoji: row.emoji.clone(),
            pleroma: NotificationPleroma {
                is_muted: JsonBool(is_muted),
                is_seen: JsonBool(row.seen),
            },
        }
    }
}

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
    use crate::{
        config::Config,
        models::notification::GroupInfo,
        models::user::{Flag, User},
    };

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

    /// The notification's own id.
    const NOTIFICATION_ID: &str = "01890b9a-2c1e-7a5b-8c9d-1e2f3a4b5c6d";
    /// The actor's id.
    const ACTOR_ID: &str = "01890b9a-2c1d-7f3a-9e4b-7c2d5a8b1e3f";
    /// The favourited status's id.
    const STATUS_ID: &str = "01890b9a-2c1f-7d7e-9a0b-3c4d5e6f7a8b";

    /// A fixed `UUIDv7` from its text.
    fn id(text: &str) -> Uuid {
        Uuid::parse_str(text).expect("fixture id")
    }

    /// The instant every fixture row in these tests is stamped with.
    fn stamped() -> chrono::DateTime<chrono::Utc> {
        chrono::NaiveDate::from_ymd_opt(2026, 1, 2)
            .expect("date")
            .and_hms_opt(3, 4, 5)
            .expect("time")
            .and_utc()
    }

    /// A favourite of [`STATUS_ID`] by [`ACTOR_ID`]: stored key, unseen,
    /// undismissed.
    fn notification_row() -> NotificationRow {
        NotificationRow {
            id: id(NOTIFICATION_ID),
            user_id: Uuid::nil(),
            from_account_id: id(ACTOR_ID),
            event_type: "favourite".to_owned(),
            status_id: Some(id(STATUS_ID)),
            emoji: None,
            report_id: None,
            group_key: Some(format!("favourite-{}", id(STATUS_ID))),
            seen: false,
            dismissed_at: None,
            inserted_at: stamped(),
        }
    }

    /// The actor: a remote `Person` at the field defaults a freshly
    /// federated mirror carries.
    fn actor() -> User {
        User {
            id: id(ACTOR_ID),
            nickname: "bob@remote.example".to_owned(),
            ap_id: "https://remote.example/users/bob".to_owned(),
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            actor_type: "Person".to_owned(),
            email: None,
            password_hash: None,
            name: "Bob".to_owned(),
            bio: String::new(),
            raw_bio: String::new(),
            avatar_url: None,
            banner_url: None,
            profile_url: None,
            locked: Flag::new(false),
            bot: Flag::new(false),
            discoverable: Flag::new(false),
            fields: json!([]),
            raw_fields: json!([]),
            inbox: None,
            shared_inbox: None,
            outbox: None,
            followers_url: None,
            following_url: None,
            featured_url: None,
            public_key: None,
            private_key: None,
            is_admin: Flag::new(false),
            is_moderator: Flag::new(false),
            is_active: Flag::new(true),
            hide_followers: Flag::new(false),
            hide_follows: Flag::new(false),
            hide_followers_count: Flag::new(false),
            hide_follows_count: Flag::new(false),
            hide_favorites: Flag::new(true),
            allow_following_move: Flag::new(true),
            skip_thread_containment: Flag::new(false),
            accepts_chat_messages: Flag::new(true),
            show_role: Flag::new(true),
            no_rich_text: Flag::new(false),
            default_scope: "public".to_owned(),
            sensitive_default: Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
            notification_settings: json!({}),
            mfa_settings: crate::models::user::mfa::MfaSettings::default_value(),
            background_image_url: None,
            birthday: None,
            show_birthday: Flag::new(false),
            also_known_as: Vec::new(),
            moved_to_ap_id: None,
            last_move_at: None,
            email_confirmed: Flag::new(true),
            approval_pending: Flag::new(false),
            confirmation_token: None,
            confirmation_sent_at: None,
            registration_reason: None,
            last_active_at: None,
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            last_fetched_at: None,
            inserted_at: stamped(),
            updated_at: stamped(),
            preferred_frontend: None,
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
            email_notifications: Flag::new(true),
            deleted_at: None,
        }
    }

    /// `row` rendered with the fixture actor and the given status and mute
    /// flag, to a JSON value; these structs always serialize.
    fn render_flat(row: &NotificationRow, is_muted: bool) -> Value {
        let account = Account::from_user(&actor(), &Config::for_tests());
        serde_json::to_value(Notification::from_row(row, account, None, is_muted))
            .expect("an entity serializes")
    }

    /// The rendered object's keys, sorted, for exact key-set assertions.
    fn json_keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("entities render objects")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn a_flat_notification_renders_the_show_view() {
        let value = render_flat(&notification_row(), false);
        assert_eq!(
            json_keys(&value),
            [
                "account",
                "created_at",
                "group_key",
                "id",
                "pleroma",
                "type"
            ],
            "the status-less, emoji-less key set"
        );
        assert_eq!(value["id"], NOTIFICATION_ID, "the id as hyphenated text");
        assert_eq!(value["type"], "favourite", "the stored type");
        assert_eq!(
            value["group_key"],
            format!("favourite-{STATUS_ID}"),
            "the stored key"
        );
        assert_eq!(
            value["created_at"], "2026-01-02T03:04:05.000Z",
            "the Mastodon time format (decision 5)"
        );
        assert_eq!(
            value["account"]["id"], ACTOR_ID,
            "the actor expanded to a full account"
        );
        assert_eq!(value["pleroma"]["is_seen"], false, "an unseen row");
        assert_eq!(value["pleroma"]["is_muted"], false, "an unmuted actor");
        assert!(value.get("status").is_none(), "no status was given");
        assert!(value.get("emoji").is_none(), "and no emoji");
    }

    #[test]
    fn an_ungrouped_row_derives_its_key_and_the_flags_arrive_as_stored() {
        let mut row = notification_row();
        row.event_type = "follow".to_owned();
        row.group_key = None;
        row.status_id = None;
        row.seen = true;
        let value = render_flat(&row, true);
        assert_eq!(
            value["group_key"],
            format!("ungrouped-{NOTIFICATION_ID}"),
            "the derived `ungrouped-<id>` key (Pleroma's `group_key/1`)"
        );
        assert_eq!(value["pleroma"]["is_seen"], true, "the stored seen flag");
        assert_eq!(value["pleroma"]["is_muted"], true, "the caller's mute");
    }

    #[test]
    fn a_reaction_notification_carries_its_emoji() {
        let mut row = notification_row();
        row.event_type = "pleroma:emoji_reaction".to_owned();
        row.emoji = Some("🎉".to_owned());
        let value = render_flat(&row, false);
        assert_eq!(value["emoji"], "🎉", "the emoji the reaction stored");
        assert_eq!(
            value["group_key"],
            format!("favourite-{STATUS_ID}"),
            "the key is the stored one whatever the type spelled"
        );
    }
}
