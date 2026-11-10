//! The Mastodon `StatusEdit` entity: one version of an edited status.
//!
//! The reference shape is Pleroma's `history_item.json`
//! (`lib/pleroma/web/mastodon_api/views/status_view.ex`): the content,
//! warning and sensitive flag one version carried, the moment it was
//! authored, its author as the [`Account`] entity, and the media it wore.
//! `media_attachments` here renders the stored ids of a
//! [`models::status_edit`](crate::models::status_edit) row through the same
//! [`MediaAttachment`] entity the status renderer uses, and this instance
//! answers the two keys its status renderer still leaves empty — `emojis`
//! as `[]` and `poll` as `null` (decision 3: one placeholder per part no
//! ticket has stored yet).
//!
//! Pure (decision 3): [`StatusEdit::from_source`] consumes the parts the
//! endpoint batch-loaded — the revision row's own fields, the author's
//! [`Account`] and the resolved attachments — and nothing here touches the
//! database.
//!
//! [`models::status_edit`]: crate::models::status_edit

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::{
    entities::{
        account::{Account, JsonBool},
        media_attachment::MediaAttachment,
    },
    util::time::serde_mastodon_time,
};

/// The parts one [`StatusEdit`] renders from, gathered by the endpoint: a
/// revision's stored facts, the author as an entity and the attachments its
/// media ids named, already in order.
#[derive(Debug)]
pub struct StatusEditSource<'a> {
    /// The version's content, as sanitized HTML.
    pub content: &'a str,
    /// The version's content warning; empty for none.
    pub spoiler_text: &'a str,
    /// Whether the version folded behind its warning.
    pub sensitive: bool,
    /// When the version was authored: the revision row's `created_at`, or,
    /// for the current version, the status's `edited_at` — its creation
    /// before any edit.
    pub created_at: DateTime<Utc>,
    /// The author, as the [`Account`] entity. The same account renders every
    /// element of one answer, so the endpoint builds it once per entry.
    pub account: Account,
    /// The attachments the version wore, in order.
    pub media: Vec<MediaAttachment>,
}

/// One element of `GET /api/v1/statuses/{id}/history`: one version of the
/// status, superseded or current.
#[derive(Debug, Serialize)]
pub struct StatusEdit {
    /// The version's content, as sanitized HTML.
    pub content: String,
    /// The version's content warning, `""` when there is none.
    pub spoiler_text: String,
    /// Whether [`content`](Self::content) folds behind
    /// [`spoiler_text`](Self::spoiler_text) until the reader opens it.
    pub sensitive: JsonBool,
    /// When this version was authored, in the Mastodon time format
    /// (decision 5).
    #[serde(with = "serde_mastodon_time")]
    pub created_at: DateTime<Utc>,
    /// The author, as the [`Account`] entity.
    pub account: Account,
    /// The media attachments, in order.
    pub media_attachments: Vec<MediaAttachment>,
    /// Custom emojis used in the version; `[]` — the same placeholder the
    /// [`Status`](crate::entities::status::Status) entity carries until the
    /// emoji-in-statuses ticket fills it.
    pub emojis: Vec<Value>,
    /// The poll attached to the version; `null` until the polls ticket
    /// renders it, and `null` for this instance's statuses regardless.
    pub poll: Option<Value>,
}

impl StatusEdit {
    /// Renders `source` as the entity.
    #[must_use]
    pub fn from_source(source: StatusEditSource<'_>) -> Self {
        let StatusEditSource {
            content,
            spoiler_text,
            sensitive,
            created_at,
            account,
            media,
        } = source;
        Self {
            content: content.to_owned(),
            spoiler_text: spoiler_text.to_owned(),
            sensitive: JsonBool(sensitive),
            created_at,
            account,
            media_attachments: media,
            emojis: Vec::new(),
            poll: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, to_value};
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::Config,
        entities::media_attachment::MediaAttachment as MediaEntity,
        models::{
            media_attachment::MediaAttachment as AttachmentRow, user::Flag, user::User, user::mfa,
        },
        util::time::parse_time,
    };

    /// The instant every fixture is stamped with, in and out.
    fn stamped() -> DateTime<Utc> {
        parse_time("2024-01-31T12:34:56.789Z").expect("the fixture timestamp parses")
    }

    /// A [`User`] row bare enough to render: the entity reads the profile
    /// columns, so the fixture only has to carry those.
    fn author() -> User {
        User {
            id: Uuid::nil(),
            nickname: "alice".to_owned(),
            ap_id: "https://test.example/users/alice".to_owned(),
            local: Flag::new(true),
            domain: None,
            actor_type: "Person".to_owned(),
            email: None,
            password_hash: None,
            name: "Alice".to_owned(),
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
            mfa_settings: mfa::MfaSettings::default_value(),
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

    /// A stored attachment row bare enough to render.
    fn attachment(id: Uuid) -> AttachmentRow {
        AttachmentRow {
            id,
            user_id: Some(Uuid::nil()),
            status_id: Some(Uuid::nil()),
            position: 0,
            storage_key: Some("media/x.png".to_owned()),
            remote_url: None,
            url: "https://test.example/media/x.png".to_owned(),
            preview_url: None,
            content_type: "image/png".to_owned(),
            description: None,
            blurhash: None,
            width: None,
            height: None,
            preview_width: None,
            preview_height: None,
            duration: None,
            focus_x: None,
            focus_y: None,
            file_size: None,
            sensitive_hint: false,
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// The entity's JSON, rendered from one hand-built source.
    fn rendered(media_ids: &[Uuid]) -> Value {
        let config = Config::for_tests();
        let account = Account::from_user_for(None, &author(), &config, None);
        let media = media_ids
            .iter()
            .map(|id| MediaEntity::from_model(&attachment(*id), &config))
            .collect();
        to_value(StatusEdit::from_source(StatusEditSource {
            content: "<p>new words</p>",
            spoiler_text: "careful",
            sensitive: true,
            created_at: stamped(),
            account,
            media,
        }))
        .expect("the entity serializes")
    }

    /// The rendered object's keys, sorted, for exact key-set assertions.
    fn json_keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("the entity is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn the_history_item_emits_exactly_the_ticket_keys() {
        let value = rendered(&[]);
        assert_eq!(
            json_keys(&value),
            vec![
                "account",
                "content",
                "created_at",
                "emojis",
                "media_attachments",
                "poll",
                "sensitive",
                "spoiler_text",
            ],
            "the fixed shape Pleroma's `history_item.json` answers, and \
             nothing else"
        );
        assert_eq!(value["content"], "<p>new words</p>");
        assert_eq!(value["spoiler_text"], "careful");
        assert_eq!(value["sensitive"], true, "a plain JSON boolean");
        assert_eq!(value["created_at"], "2024-01-31T12:34:56.789Z");
        assert_eq!(value["account"]["acct"], "alice");
        assert_eq!(value["media_attachments"], json!([]));
        assert_eq!(value["emojis"], json!([]), "emojis are a later ticket");
        assert_eq!(value["poll"], Value::Null, "polls are a later ticket");
    }

    #[test]
    fn the_history_item_lists_the_media_it_wore() {
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let value = rendered(&[first, second]);
        let media = value["media_attachments"]
            .as_array()
            .expect("media is an array");
        assert_eq!(media.len(), 2, "one entry per stored id");
        assert_eq!(media[0]["id"], first.to_string(), "in the stored order");
        assert_eq!(media[1]["id"], second.to_string());
    }
}
