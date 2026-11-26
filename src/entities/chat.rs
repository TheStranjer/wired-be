//! The Pleroma chat entities: a chat in somebody's list, and one of its
//! messages.
//!
//! The reference shapes are Pleroma's `ChatView.render("show.json")` and
//! `Chat.MessageReferenceView.render("show.json")`
//! (`lib/pleroma/web/pleroma_api/views/chat_view.ex`,
//! `lib/pleroma/web/pleroma_api/views/chat/message_reference_view.ex`); the
//! JSON keys are emitted exactly as those views name them, in their order.
//! [`Chat::from_row`] and [`ChatMessage::from_reference`] are pure
//! (decision 3): they render rows the caller read, plus the instance
//! [`Config`] where the `Account` inside a chat needs it, so both are testable
//! with `serde_json::to_value` alone.
//!
//! What those views look up, these take as arguments: the unread number is
//! [`unread_count`](crate::models::chat::unread_count)'s answer, the last
//! message is the newest row of
//! [`messages_page`](crate::models::chat::messages_page), and a message's media
//! is a [`media_attachment`](crate::models::media_attachment) row looked up by
//! the reference's
//! [`attachment_media_id`](crate::models::chat::ChatMessageReference::attachment_media_id)
//! — `None` when the message carries none or its file has since been deleted.
//!
//! One key is a constant here because nothing behind it is implemented:
//! `card`, Pleroma's rich-media preview, always `null` because this instance
//! collects no cards. `emojis` starts empty and [`ChatMessage::with_emojis`]
//! fills it from the `custom_emojis` rows the caller found for the message
//! text under its sender's domain, the way Pleroma builds it from the
//! shortcodes in the text. `idempotency_key` is a former constant, retired by
//! the message posting ticket: the key a client sent is stored on the message
//! row and renders from there, absent from the JSON only when the message
//! carries no key.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::{
    config::Config,
    entities::{account::Account, custom_emoji::CustomEmoji, media_attachment::MediaAttachment},
    models::{
        chat::{Chat as ChatRow, ChatMessageReference},
        custom_emoji::CustomEmoji as EmojiRow,
        media_attachment::MediaAttachment as AttachmentRow,
        user::User,
    },
};

/// One chat as its owner lists it: Pleroma's `ChatView` `"show.json"`.
///
/// Build one with [`Chat::from_row`]; its [`Serialize`] impl emits `id`,
/// `account`, `unread`, `last_message`, `updated_at` and `pinned`.
#[derive(Debug, Serialize)]
pub struct Chat {
    /// The chat id as the hyphenated lowercase `UUIDv7` string (decision 4) —
    /// this participant's own row, never the other side's.
    pub id: String,
    /// The other participant, as the Mastodon `Account` entity.
    pub account: Account,
    /// How many messages of this chat its owner has not read.
    pub unread: i64,
    /// The newest message its owner can see; `null` when the chat holds none.
    pub last_message: Option<ChatMessage>,
    /// When the chat last saw activity, in the Mastodon time format
    /// (decision 5): Pleroma's `Utils.to_masto_date` of `chat.updated_at`.
    #[serde(with = "crate::util::time::serde_mastodon_time")]
    pub updated_at: DateTime<Utc>,
    /// Whether the owner pinned this chat to the top of their list.
    pub pinned: bool,
}

impl Chat {
    /// Renders `row` — one participant's chat of `recipient`, the other side of
    /// the conversation — as a `Chat` entity.
    ///
    /// `unread` and `last_message` are read from the reference table by the
    /// caller (see the module docs); `config` is what [`Account::from_user`]
    /// renders `recipient` against. Pure, like every entity: nothing here looks
    /// anything up.
    #[must_use]
    pub fn from_row(
        row: &ChatRow,
        recipient: &User,
        config: &Config,
        unread: i64,
        last_message: Option<ChatMessage>,
    ) -> Self {
        Self {
            id: row.id.to_string(),
            account: Account::from_user(recipient, config),
            unread,
            last_message,
            updated_at: row.updated_at,
            pinned: row.pinned,
        }
    }
}

/// One message of one chat, as its owner sees it: Pleroma's
/// `Chat.MessageReferenceView` `"show.json"`.
///
/// Build one with [`ChatMessage::from_reference`]; its [`Serialize`] impl emits
/// `id`, `content`, `chat_id`, `account_id`, `created_at`, `emojis`,
/// `attachment`, `unread`, `card` and — only when it is set —
/// `idempotency_key`.
#[derive(Debug, Serialize)]
pub struct ChatMessage {
    /// The reference id, as the hyphenated lowercase `UUIDv7` string
    /// (decision 4): the id of this message *in this chat*, which is what the
    /// read markers and this chat's pagination speak.
    pub id: String,
    /// The message as HTML, from the row's
    /// [`content`](ChatMessageReference::content).
    pub content: String,
    /// The chat this message belongs to, as an id string.
    pub chat_id: String,
    /// Who wrote it, as an account id string.
    pub account_id: String,
    /// When it was sent, in the Mastodon time format (decision 5).
    #[serde(with = "crate::util::time::serde_mastodon_time")]
    pub created_at: DateTime<Utc>,
    /// Custom emojis the message text uses; `[]` until
    /// [`ChatMessage::with_emojis`] adds the rows found for it.
    pub emojis: Vec<CustomEmoji>,
    /// The media this message carries; `null` when it carries none.
    pub attachment: Option<MediaAttachment>,
    /// Whether the owner of [`chat_id`](Self::chat_id) has still to read this
    /// message.
    pub unread: bool,
    /// Pleroma's rich-media card for the message's links; `null`, because this
    /// instance collects no cards.
    pub card: Option<Value>,
    /// The `Idempotency-Key` the message was created with, read off the
    /// message row ([`ChatMessageReference::idempotency_key`]); the key is
    /// absent from the JSON for a message posted without one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl ChatMessage {
    /// Renders `row` — one side's reference to a message — as a `ChatMessage`
    /// entity, with `attachment` as its media when the message carries one.
    ///
    /// `config` renders the attachment's URLs (see
    /// [`MediaAttachment::from_model`], the one media proxy entry point).
    ///
    /// The entity's `id` is the reference's id, not
    /// [`message_id`](ChatMessageReference::message_id) — the row the
    /// attachment and the `ActivityPub` id belong to — exactly as Pleroma
    /// renders `cm_ref.id`.
    #[must_use]
    pub fn from_reference(
        row: &ChatMessageReference,
        attachment: Option<&AttachmentRow>,
        config: &Config,
    ) -> Self {
        Self {
            id: row.id.to_string(),
            content: row.content.clone(),
            chat_id: row.chat_id.to_string(),
            account_id: row.sender_id.to_string(),
            created_at: row.message_inserted_at,
            emojis: Vec::new(),
            attachment: attachment.map(|media| MediaAttachment::from_model(media, config)),
            unread: row.unread,
            card: None,
            idempotency_key: row.idempotency_key.clone(),
        }
    }

    /// This message with `emojis` — the `custom_emojis` rows its text uses —
    /// rendered as its `emojis`.
    ///
    /// `config` renders the emoji URLs through the media proxy, as a status'
    /// emojis are.
    #[must_use]
    pub fn with_emojis(mut self, emojis: &[EmojiRow], config: &Config) -> Self {
        self.emojis = emojis
            .iter()
            .map(|row| CustomEmoji::from_model(row, config))
            .collect();
        self
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::{
        models::{
            chat::ChatMessage as MessageRow,
            user::{DEFAULT_SCOPE, Flag},
        },
        test_fixtures::json_keys,
        util::id::parse_id,
    };

    /// The chat row's id, as a client sees it.
    const CHAT_ID: &str = "01890b9a-2c1d-7f3a-9e4b-7c2d5a8b1e3f";
    /// The reference row's id, which is the message entity's `id`.
    const REFERENCE_ID: &str = "01890b9a-2c1e-7a5b-8c9d-1e2f3a4b5c6d";
    /// The message row's id.
    const MESSAGE_ID: &str = "01890b9a-2c1f-7d7e-9a0b-3c4d5e6f7a8b";
    /// The chat owner's account id.
    const OWNER_ID: &str = "01890b9a-2c20-7e21-b4c8-9d0e1f2a3b4c";
    /// The recipient's account id, rendered as the chat's `account`.
    const RECIPIENT_ID: &str = "01890b9a-2c21-7a35-8f6a-b7c8d9e0f1a2";
    /// The attachment row's id.
    const ATTACHMENT_ID: &str = "01890b9a-2c22-7b4d-9e5c-0d1e2f3a4b5c";
    /// The instant the message was sent.
    const SENT: &str = "2024-01-31T12:34:56.789Z";
    /// The instant the chat last saw activity.
    const BUMPED: &str = "2024-02-01T08:00:00.000Z";
    /// A timestamp with no fractional part, to check how it is spelled out.
    const WHOLE_SECOND: &str = "2024-01-31T12:34:56Z";

    /// The keys of the `Chat` entity, sorted, for an exact key-set check.
    const CHAT_KEYS: [&str; 6] = [
        "account",
        "id",
        "last_message",
        "pinned",
        "unread",
        "updated_at",
    ];

    /// The keys of a `ChatMessage` entity with no idempotency key, sorted.
    const MESSAGE_KEYS: [&str; 9] = [
        "account_id",
        "attachment",
        "card",
        "chat_id",
        "content",
        "created_at",
        "emojis",
        "id",
        "unread",
    ];

    /// The keys of the `Attachment` entity inside a message, sorted.
    const ATTACHMENT_KEYS: [&str; 10] = [
        "blurhash",
        "description",
        "id",
        "meta",
        "pleroma",
        "preview_url",
        "remote_url",
        "text_url",
        "type",
        "url",
    ];

    /// An instant from its ISO 8601 text.
    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("the fixture times are timestamps")
            .with_timezone(&Utc)
    }

    /// An id from its hyphenated text.
    fn id(text: &str) -> Uuid {
        parse_id(text).expect("the fixture ids are ids")
    }

    /// A [`ChatRow`]: `OWNER_ID`'s unpinned chat of `RECIPIENT_ID`, last bumped
    /// at [`BUMPED`].
    fn chat_row() -> ChatRow {
        ChatRow {
            id: id(CHAT_ID),
            user_id: id(OWNER_ID),
            recipient_id: id(RECIPIENT_ID),
            pinned: false,
            inserted_at: at(SENT),
            updated_at: at(BUMPED),
        }
    }

    /// A [`ChatMessageReference`] for [`MESSAGE_ID`] in [`CHAT_ID`], unread,
    /// written by [`RECIPIENT_ID`]: the chat owner's view of a message the
    /// other side sent.
    fn reference_row() -> ChatMessageReference {
        ChatMessageReference {
            id: id(REFERENCE_ID),
            chat_id: id(CHAT_ID),
            message_id: id(MESSAGE_ID),
            unread: true,
            inserted_at: at(SENT),
            sender_id: id(RECIPIENT_ID),
            content: "<p>hi there</p>".to_owned(),
            raw_text: "hi there".to_owned(),
            attachment_media_id: None,
            ap_id: Some(format!("https://remote.example/chat/{MESSAGE_ID}")),
            idempotency_key: None,
            message_inserted_at: at(SENT),
        }
    }

    /// A media row: the image one message carries.
    fn attachment_row() -> AttachmentRow {
        AttachmentRow {
            id: id(ATTACHMENT_ID),
            user_id: Some(id(RECIPIENT_ID)),
            status_id: None,
            position: 0,
            storage_key: Some("9b3c1d0e4f5a6b7c".to_owned()),
            remote_url: None,
            url: format!("https://test.example/media/{ATTACHMENT_ID}"),
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
            inserted_at: at(SENT),
            updated_at: at(SENT),
        }
    }

    /// The recipient, rendered inside a chat: a remote `Person` at the field
    /// defaults a freshly federated mirror carries.
    fn recipient() -> User {
        User {
            id: id(RECIPIENT_ID),
            nickname: "bob@remote.example".to_owned(),
            ap_id: "https://remote.example/users/bob".to_owned(),
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
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
            mfa_settings: crate::models::user::mfa::MfaSettings::default_value(),
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
            inserted_at: at(SENT),
            updated_at: at(SENT),
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
            email_notifications: Flag::new(true),
            deleted_at: None,
            suspended_at: None,
            disabled_at: None,
            remote_suspended: Flag::new(false),
        }
    }

    /// The `Chat` entity for [`chat_row`], `unread` as given and
    /// `last_message` as given, rendered to JSON.
    fn render_chat(unread: i64, last_message: Option<ChatMessage>) -> Value {
        let config = Config::for_tests();
        serde_json::to_value(Chat::from_row(
            &chat_row(),
            &recipient(),
            &config,
            unread,
            last_message,
        ))
        .unwrap()
    }

    /// `message` rendered to a JSON value; these structs always serialize.
    fn render(message: &ChatMessage) -> Value {
        serde_json::to_value(message).unwrap()
    }

    /// Whether `value` carries `key` at all, whatever its value.
    fn has_key(value: &Value, key: &str) -> bool {
        value
            .as_object()
            .expect("entities render objects")
            .contains_key(key)
    }

    #[test]
    fn a_message_renders_the_reference_view() {
        let row = reference_row();
        let value = render(&ChatMessage::from_reference(
            &row,
            None,
            &Config::for_tests(),
        ));

        assert_eq!(json_keys(&value), MESSAGE_KEYS, "exactly the view's keys");
        assert_eq!(
            value["id"], REFERENCE_ID,
            "the reference id, not the message's"
        );
        assert_eq!(value["content"], "<p>hi there</p>", "the stored HTML");
        assert_eq!(value["chat_id"], CHAT_ID, "the chat it belongs to");
        assert_eq!(value["account_id"], RECIPIENT_ID, "the writer's account id");
        assert_eq!(value["created_at"], SENT, "the Mastodon timestamp");
        assert_eq!(value["emojis"], json!([]), "no emoji mapping yet");
        assert_eq!(value["attachment"], Value::Null, "and no attachment");
        assert_eq!(value["unread"], true, "the reference's own flag");
        assert_eq!(value["card"], Value::Null, "no rich-media cards here");
        assert!(
            !has_key(&value, "idempotency_key"),
            "the idempotency key is absent when it is not set"
        );
        assert!(
            !has_key(&value, "raw_text"),
            "the source text is not part of the entity"
        );
    }

    /// A stored emoji row of the sender's domain.
    fn emoji_row(shortcode: &str) -> EmojiRow {
        EmojiRow {
            id: id(MESSAGE_ID),
            shortcode: shortcode.to_owned(),
            domain: Some("remote.example".to_owned()),
            image_url: format!("https://remote.example/emoji/{shortcode}.png"),
            static_url: None,
            category: None,
            tags: Vec::new(),
            visible_in_picker: true,
            disabled: false,
            inserted_at: at(SENT),
            updated_at: at(SENT),
        }
    }

    #[test]
    fn a_message_renders_the_emojis_it_is_given() {
        let config = Config::for_tests();
        let value = render(
            &ChatMessage::from_reference(&reference_row(), None, &config)
                .with_emojis(&[emoji_row("plerofox")], &config),
        );

        assert_eq!(json_keys(&value), MESSAGE_KEYS, "the key set is unchanged");
        assert_eq!(
            value["emojis"],
            json!([{
                "shortcode": "plerofox",
                "url": "https://remote.example/emoji/plerofox.png",
                "static_url": "https://remote.example/emoji/plerofox.png",
                "visible_in_picker": true,
                "category": null,
                "tags": [],
            }]),
            "the found row renders as a CustomEmoji entity"
        );
    }

    #[test]
    fn a_message_attachment_renders_the_mastodon_attachment() {
        let mut row = reference_row();
        row.attachment_media_id = Some(id(ATTACHMENT_ID));
        row.unread = false;

        let value = render(&ChatMessage::from_reference(
            &row,
            Some(&attachment_row()),
            &Config::for_tests(),
        ));

        assert_eq!(
            json_keys(&value["attachment"]),
            ATTACHMENT_KEYS,
            "the full Attachment entity"
        );
        assert_eq!(
            value["attachment"]["id"], ATTACHMENT_ID,
            "rendered from the media row"
        );
        assert_eq!(value["attachment"]["type"], "image", "from its MIME type");
        assert_eq!(value["unread"], false, "a read reference renders read");
    }

    #[test]
    fn an_idempotency_key_appears_only_when_the_row_carries_one() {
        let plain = ChatMessage::from_reference(&reference_row(), None, &Config::for_tests());
        assert!(
            plain.idempotency_key.is_none(),
            "a reference of a keyless message has none"
        );
        assert!(
            !has_key(&render(&plain), "idempotency_key"),
            "so it is absent"
        );

        let mut row = reference_row();
        row.idempotency_key = Some("7c9e6679-7425-40de-944b-e07fc1f90ae7".to_owned());
        let value = render(&ChatMessage::from_reference(
            &row,
            None,
            &Config::for_tests(),
        ));

        assert_eq!(
            json_keys(&value).len(),
            MESSAGE_KEYS.len() + 1,
            "one more key than the plain entity"
        );
        assert_eq!(
            value["idempotency_key"], "7c9e6679-7425-40de-944b-e07fc1f90ae7",
            "the key the client sent, echoed back from the row"
        );
    }

    #[test]
    fn a_chat_renders_the_chat_view() {
        let value = render_chat(3, None);

        assert_eq!(json_keys(&value), CHAT_KEYS, "exactly the view's keys");
        assert_eq!(value["id"], CHAT_ID, "the chat id");
        assert_eq!(value["unread"], 3, "the count the caller read");
        assert_eq!(value["last_message"], Value::Null, "an empty chat");
        assert_eq!(value["updated_at"], BUMPED, "in the Mastodon format");
        assert_eq!(value["pinned"], false, "the stored flag");
        assert_eq!(
            value["account"]["id"], RECIPIENT_ID,
            "the account is the recipient's, not the owner's"
        );
        assert_eq!(
            value["account"]["acct"], "bob@remote.example",
            "rendered as a full Account entity"
        );
        assert_eq!(
            value["account"]["uri"], "https://remote.example/users/bob",
            "with the recipient's ActivityPub id"
        );
    }

    #[test]
    fn a_chat_carries_its_last_message_when_it_has_one() {
        let row = reference_row();
        let last = ChatMessage::from_reference(&row, None, &Config::for_tests());
        let value = render_chat(1, Some(last));

        assert_eq!(
            json_keys(&value["last_message"]),
            MESSAGE_KEYS,
            "the nested message is a ChatMessage entity"
        );
        assert_eq!(
            value["last_message"]["id"], REFERENCE_ID,
            "rendered from the same reference row"
        );
        assert_eq!(value["last_message"]["unread"], true, "with its own flags");
    }

    #[test]
    fn pinned_and_read_chats_render_as_stored() {
        let config = Config::for_tests();
        let mut row = chat_row();
        row.pinned = true;
        row.updated_at = at(WHOLE_SECOND);

        let value =
            serde_json::to_value(Chat::from_row(&row, &recipient(), &config, 0, None)).unwrap();

        assert_eq!(value["pinned"], true, "a pinned chat says so");
        assert_eq!(value["unread"], 0, "and an all-read one counts zero");
        assert_eq!(
            value["updated_at"], "2024-01-31T12:34:56.000Z",
            "a whole second still renders three fractional digits"
        );
    }

    #[test]
    fn a_message_row_renders_through_its_reference() {
        // The entity is built from the joined row the model answers with: the
        // message's own columns become the entity's fields, while the entity's
        // id is the reference's.
        let message_row = MessageRow {
            id: id(MESSAGE_ID),
            sender_id: id(OWNER_ID),
            content: "<p>mine</p>".to_owned(),
            raw_text: "mine".to_owned(),
            attachment_media_id: None,
            ap_id: None,
            idempotency_key: None,
            deleted_at: None,
            inserted_at: at(WHOLE_SECOND),
        };
        let mut reference = reference_row();
        reference.sender_id = message_row.sender_id;
        reference.content = message_row.content.clone();
        reference.message_inserted_at = message_row.inserted_at;

        let value = render(&ChatMessage::from_reference(
            &reference,
            None,
            &Config::for_tests(),
        ));

        assert_eq!(value["account_id"], OWNER_ID, "the writer of the row");
        assert_eq!(value["content"], "<p>mine</p>", "their text");
        assert_eq!(
            value["created_at"], "2024-01-31T12:34:56.000Z",
            "and when they sent it, in the Mastodon format"
        );
        assert_eq!(
            value["id"], REFERENCE_ID,
            "while the entity id stays the reference's"
        );
    }

    #[test]
    fn an_empty_message_renders_its_empty_content() {
        let mut row = reference_row();
        row.content = String::new();
        row.raw_text = String::new();
        row.ap_id = None;

        let value = render(&ChatMessage::from_reference(
            &row,
            None,
            &Config::for_tests(),
        ));

        assert_eq!(
            value["content"], "",
            "a withdrawn message renders empty text"
        );
        assert_eq!(value["attachment"], Value::Null, "with no attachment");
    }
}
