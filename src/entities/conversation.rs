//! The Mastodon `Conversation` entity: one account's side of a
//! direct-message thread, as its conversation listing answers it.
//!
//! The reference shape is Pleroma's
//! `ConversationView.render("participation.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/conversation_view.ex`): the
//! participation's own id, the `unread` flag inverted off the row, the
//! thread's other participants expanded to [`Account`] entities, and the
//! newest status of the thread rendered as the `last_status` (Pleroma's
//! `last_activity`, `null` when the participation points at nothing
//! renderable). The JSON keys go out exactly as the ticket names them, in
//! that order: `id`, `unread`, `accounts`, `last_status`.
//!
//! [`Conversation::from_row`] is pure (decision 3): every read that fills
//! the expansions — the recipient accounts, the rendered last status —
//! happens in the endpoint that calls it, and the rendered status arrives
//! as its serialized [`Value`], the same shape the notification entity
//! carries its status in (one batched render can answer several
//! conversations of one thread, and the [`Status`] entity is not cheap to
//! duplicate).
//!
//! [`Status`]: crate::entities::status::Status

use serde::Serialize;
use serde_json::Value;

use crate::{entities::account::Account, models::conversation::Participation};

/// One conversation as its participant lists it: the Mastodon
/// `Conversation` entity (Pleroma's `"participation.json"`).
///
/// Build one with [`Conversation::from_row`]; its [`Serialize`] impl emits
/// `id`, `unread`, `accounts` and `last_status`.
#[derive(Debug, Serialize)]
pub struct Conversation {
    /// The participation's id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4) — this account's own row, never the thread's and never
    /// another participant's.
    pub id: String,
    /// Whether the account has not seen the thread's newest status yet:
    /// the row's [`read`](Participation::read) flag inverted, Pleroma's
    /// `!participation.read`.
    pub unread: bool,
    /// The thread's other participants — the participation's recipient
    /// list, which never holds its owner — expanded as the viewer sees
    /// them, in the pair key's order.
    pub accounts: Vec<Account>,
    /// The conversation's newest status as the viewer sees it, rendered
    /// through the shared status batch; `null` when the participation
    /// names no status, its last one is gone, or the viewer may not see
    /// it (Pleroma's `last_activity` arm, the ticket's `Status` or null).
    pub last_status: Option<Value>,
}

impl Conversation {
    /// Renders `row` beside its expansions: `accounts` is the
    /// participation's recipient list as the viewer sees it, `last_status`
    /// its rendered [`last_status_id`](Participation::last_status_id)
    /// (`None` when that names no row the viewer may render). Pure
    /// (decision 3): every database read that fills these parts happens
    /// in the caller.
    #[must_use]
    pub fn from_row(
        row: &Participation,
        accounts: Vec<Account>,
        last_status: Option<Value>,
    ) -> Self {
        Self {
            id: row.id.to_string(),
            unread: !row.read,
            accounts,
            last_status,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{DEFAULT_SCOPE, Flag, User, mfa},
    };

    /// The fixture account's id, as stored.
    const ACCOUNT_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

    /// The instant the fixture rows are stamped at.
    fn stamped() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 31, 12, 34, 56).unwrap()
    }

    /// A participation row with its owner's read flag.
    fn row(read: bool) -> Participation {
        Participation {
            id: Uuid::now_v7(),
            user_id: Uuid::now_v7(),
            conversation_id: Uuid::now_v7(),
            read,
            last_status_id: Some(Uuid::now_v7()),
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// A `User` row named `nickname`, active and local, at the column
    /// defaults a freshly registered account carries.
    fn base_user(nickname: &str) -> User {
        User {
            id: Uuid::parse_str(ACCOUNT_ID).expect("fixture id"),
            nickname: nickname.to_owned(),
            ap_id: format!("{TEST_BASE_URL}/users/{nickname}"),
            local: Flag::new(true),
            domain: None,
            actor_type: "Person".to_owned(),
            email: None,
            password_hash: None,
            name: String::new(),
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
            default_scope: DEFAULT_SCOPE.to_owned(),
            sensitive_default: Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
            notification_settings: json!({}),
            mfa_settings: mfa::MfaSettings::default_value(),
            background_image_url: None,
            birthday: None,
            show_birthday: Flag::new(false),
            also_known_as: Vec::new(),
            moved_to_ap_id: None,
            last_move_at: None,
            email_confirmed: Flag::new(true),
            approval_pending: Flag::new(false),
            registration_reason: None,
            confirmation_token: None,
            confirmation_sent_at: None,
            last_active_at: None,
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            inserted_at: stamped(),
            updated_at: stamped(),
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
            email_notifications: Flag::new(true),
            deleted_at: None,
        }
    }

    /// The entity's JSON: the participation id as `UUIDv7` text, `unread`
    /// the inverted row flag, the recipient accounts embedded, and the
    /// rendered status — or `null` where the thread holds nothing
    /// renderable (Pleroma's `last_activity` arm).
    #[test]
    fn the_entity_emits_the_ticket_shape() {
        let config = crate::config::Config::for_tests();
        let participation = row(false);
        let entity = Conversation::from_row(
            &participation,
            vec![Account::from_user(&base_user("bob"), &config)],
            Some(json!({"id": "status-1"})),
        );
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(value["id"], participation.id.to_string());
        assert_eq!(value["unread"], true, "an unread row reads !read");
        assert_eq!(value["accounts"][0]["acct"], "bob");
        assert_eq!(value["last_status"]["id"], "status-1");
        let rendered = serde_json::to_string(&entity).expect("the entity serializes");
        let at = |key: &str| {
            rendered
                .find(&format!("\"{key}\""))
                .unwrap_or_else(|| panic!("`{key}` in {rendered}"))
        };
        assert!(
            at("id") < at("unread")
                && at("unread") < at("accounts")
                && at("accounts") < at("last_status"),
            "the raw JSON spells the ticket's keys in the ticket's order, \
             which a `Value`'s sorted map cannot show: {rendered}"
        );

        let read_row = Participation {
            read: true,
            ..participation
        };
        let empty = Conversation::from_row(&read_row, Vec::new(), None);
        let empty_value = serde_json::to_value(&empty).expect("the empty entity serializes");
        assert_eq!(
            empty_value["unread"], false,
            "a read row answers unread false"
        );
        assert_eq!(
            empty_value["accounts"],
            json!([]),
            "and no recipients embed nothing"
        );
        assert_eq!(
            empty_value["last_status"],
            Value::Null,
            "with no status renderable"
        );
    }
}
