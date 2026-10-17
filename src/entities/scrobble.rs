//! The Pleroma `Scrobble` entity: one listen, as the scrobble endpoints
//! render it.
//!
//! The reference shape is Pleroma's `ScrobbleView.render("show.json", …)`
//! (`lib/pleroma/web/pleroma_api/views/scrobble_view.ex`): the activity's
//! `id` and `created_at` beside the `Audio` object's `title`, `artist`,
//! `album`, `length` and `externalLink`, and the listener as the Mastodon
//! `Account` entity. The eight keys are declared in the order the ticket
//! lists them, with two differences from the view it fixes: `external_link`
//! is the one spelling (the view's deprecated `externalLink` duplicate is not
//! emitted) and `id` is the row's `UUIDv7` as lowercase hyphenated text, not
//! Pleroma's integer (decision 4). `created_at` is the row's `inserted_at` in
//! the Mastodon time format (decision 5), Pleroma's `Utils.to_masto_date` of
//! the activity's `published`.
//!
//! Pleroma strips HTML tags from the three text fields; nothing here stores
//! markup — [`crate::api::scrobbles`] keeps the client's parameters verbatim
//! and this table has no formatted side — so they render as stored.
//!
//! [`Scrobble::from_row`] is pure (decision 3): it renders a
//! [`models::scrobble`](crate::models::scrobble) row, the listener's [`User`]
//! row, the reader behind an optional token and the instance
//! [`Config`] — the `Account` needs the last two, exactly as Pleroma's view
//! renders `AccountView` with `for: opts[:for]`.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::{
    config::Config,
    entities::account::{Account, AccountViewer},
    models::{scrobble::Scrobble as ScrobbleRow, user::User},
};

/// The Pleroma `Scrobble` entity.
///
/// Build one with [`Scrobble::from_row`]; its [`Serialize`] impl emits `id`,
/// `account`, `title`, `artist`, `album`, `length`, `external_link` and
/// `created_at`.
#[derive(Debug, Serialize)]
pub struct Scrobble {
    /// The scrobble id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    pub id: String,
    /// The account that listened, as the Mastodon `Account` entity, rendered
    /// for the reader.
    pub account: Account,
    /// The title of the media playing.
    pub title: String,
    /// The artist, `null` when the client named none.
    pub artist: Option<String>,
    /// The album, `null` when the client named none.
    pub album: Option<String>,
    /// Length of the media in milliseconds, `null` when the client named
    /// none.
    pub length: Option<i32>,
    /// The URL referencing the media, `null` when the client sent none.
    pub external_link: Option<String>,
    /// When the listen was scrobbled, in the Mastodon time format
    /// (decision 5).
    #[serde(with = "crate::util::time::serde_mastodon_time")]
    pub created_at: DateTime<Utc>,
}

impl Scrobble {
    /// Renders `row` as a `Scrobble` entity: `owner` is the account it names
    /// as `account` (the row's `user_id`), `viewer` the reader whose view of
    /// that account is served (Pleroma's `for:`), and `config` what
    /// [`Account`] is rendered against.
    ///
    /// The relationship is not embedded: Pleroma's scrobble view renders a
    /// plain `show.json` account, so `pleroma.relationship` stays `{}` even
    /// when a signed-in reader lists someone else's scrobbles.
    #[must_use]
    pub fn from_row(
        row: &ScrobbleRow,
        owner: &User,
        viewer: AccountViewer<'_>,
        config: &Config,
    ) -> Self {
        Self {
            id: row.id.to_string(),
            account: Account::from_user_for(viewer, owner, config, None),
            title: row.title.clone(),
            artist: row.artist.clone(),
            album: row.album.clone(),
            length: row.length,
            external_link: row.external_link.clone(),
            created_at: row.inserted_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            status::Visibility,
            user::{DEFAULT_SCOPE, Flag},
        },
        util::time::parse_time,
    };

    /// The scrobble id, as a client sees it.
    const SCROBBLE_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// The listener's account id, which the entity embeds.
    const OWNER_ID: &str = "01934567-89ab-7cde-f012-3456789abcdf";
    /// The instant the scrobble was stored.
    const STAMPED: &str = "2024-01-31T12:34:56.789Z";
    /// Every JSON key the entity must emit, sorted, for an exact key-set
    /// check.
    const EXPECTED_KEYS: [&str; 8] = [
        "account",
        "album",
        "artist",
        "created_at",
        "external_link",
        "id",
        "length",
        "title",
    ];

    /// The instance configuration entities are rendered against.
    fn config() -> Config {
        Config::for_tests()
    }

    /// An instant from its ISO 8601 text.
    fn at(text: &str) -> DateTime<Utc> {
        parse_time(text).expect("the fixture times are timestamps")
    }

    /// The listener: a local account named `alice` on the test instance, at
    /// the column defaults a fresh registration carries.
    fn owner() -> User {
        User {
            id: Uuid::parse_str(OWNER_ID).expect("the fixture id is an id"),
            nickname: "alice".to_owned(),
            ap_id: format!("{TEST_BASE_URL}/users/alice"),
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
            default_scope: DEFAULT_SCOPE.to_owned(),
            sensitive_default: Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
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
            last_active_at: None,
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            last_fetched_at: None,
            inserted_at: at(STAMPED),
            updated_at: at(STAMPED),
            preferred_frontend: None,
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
        }
    }

    /// A stored scrobble of `visibility`, every optional column set.
    fn row(visibility: Visibility) -> ScrobbleRow {
        ScrobbleRow {
            id: Uuid::parse_str(SCROBBLE_ID).expect("the fixture id is an id"),
            user_id: owner().id,
            title: "Midnight City".to_owned(),
            artist: Some("M83".to_owned()),
            album: Some("Hurry Up, We're Dreaming".to_owned()),
            length: Some(243_000),
            external_link: Some("https://example.test/midnight-city".to_owned()),
            visibility,
            inserted_at: at(STAMPED),
        }
    }

    /// `row` rendered as a JSON value for `viewer`; entities always
    /// serialize.
    fn render(row: &ScrobbleRow, viewer: AccountViewer<'_>) -> Value {
        let entity = Scrobble::from_row(row, &owner(), viewer, &config());
        serde_json::to_value(entity).expect("an entity serializes")
    }

    /// The rendered object's keys, sorted, for exact key-set assertions.
    fn keys(value: &Value) -> Vec<&str> {
        let object = value.as_object().expect("a scrobble renders an object");
        let mut names: Vec<&str> = object.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn a_scrobble_renders_the_pleroma_entity() {
        let value = render(&row(Visibility::Public), None);

        assert_eq!(keys(&value), EXPECTED_KEYS, "exactly the keys");
        assert_eq!(value["id"], SCROBBLE_ID, "the id as hyphenated text");
        assert_eq!(value["title"], "Midnight City", "the title verbatim");
        assert_eq!(value["artist"], "M83", "the artist");
        assert_eq!(value["album"], "Hurry Up, We're Dreaming", "the album");
        assert_eq!(value["length"], 243_000, "the length in milliseconds");
        assert_eq!(
            value["external_link"], "https://example.test/midnight-city",
            "the external link, snake_case only"
        );
        assert!(
            value.get("externalLink").is_none(),
            "and not the deprecated camelCase duplicate"
        );
        assert_eq!(
            value["created_at"], STAMPED,
            "the row's stamp in the Mastodon format"
        );
    }

    #[test]
    fn the_account_is_the_listener_and_is_rendered_for_the_viewer() {
        let alice = owner();
        let anonymous = render(&row(Visibility::Public), None);
        let value = render(&row(Visibility::Public), Some(&alice));

        for entity in [&anonymous, &value] {
            assert_eq!(
                entity["account"]["id"],
                alice.id.to_string(),
                "the entity's account is the scrobbler"
            );
            assert_eq!(
                entity["account"]["acct"], "alice",
                "a local account's handle without a domain"
            );
            assert_eq!(
                entity["account"]["url"],
                format!("{TEST_BASE_URL}/users/alice"),
                "on this instance"
            );
            assert_eq!(
                entity["account"]["pleroma"]["relationship"],
                Value::Object(serde_json::Map::new()),
                "the scrobble view embeds no relationship, not even self"
            );
        }
    }

    #[test]
    fn the_optional_columns_render_as_null() {
        let bare = ScrobbleRow {
            title: "just a title".to_owned(),
            artist: None,
            album: None,
            length: None,
            external_link: None,
            ..row(Visibility::Public)
        };
        let value = render(&bare, None);

        assert_eq!(value["title"], "just a title", "the title is never null");
        for key in ["artist", "album", "length", "external_link"] {
            assert!(value[key].is_null(), "`{key}` renders as null: {value}");
        }
    }

    #[test]
    fn every_visibility_renders_the_same_shape() {
        for visibility in [
            Visibility::Public,
            Visibility::Unlisted,
            Visibility::Private,
            Visibility::Direct,
        ] {
            let value = render(&row(visibility), None);
            assert_eq!(
                keys(&value),
                EXPECTED_KEYS,
                "visibility is not part of the entity: {value}"
            );
        }
    }
}
