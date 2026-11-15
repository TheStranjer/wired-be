//! The admin API's account entity: what an admin sees of one account in
//! the users listing and the users show endpoint (WBE-195).
//!
//! The reference is `Pleroma.Web.AdminAPI.AccountView`
//! (`lib/pleroma/web/admin_api/views/account_view.ex`), whose `"show.json"`
//! renders the fifteen keys the ticket pins and whose `"index.json"` wraps
//! the entity list in `{users, count, page_size}`. It is a deliberately
//! different shape from the Mastodon [`Account`](super::account::Account)
//! entity: a moderation console's row, so the private columns an admin is
//! owed — `email`, `registration_reason`, the two approval flags — are in,
//! while the counters, note and `pleroma` block are not.
//!
//! Three of Pleroma's renderings are re-worded for this schema:
//!
//! - `is_confirmed` and `is_approved` are its `is_confirmed` /
//!   `is_approved` columns; here the same facts are
//!   [`email_confirmed`](User::email_confirmed) and the negation of
//!   [`approval_pending`](User::approval_pending).
//! - `url` is its `user.uri || user.ap_id`; here
//!   [`profile_url`](User::profile_url) is the human-facing URL and
//!   [`ap_id`](User::ap_id) the fallback — the reading the
//!   [`Account`](super::account::Account) entity already applies.
//! - `avatar` is its `User.avatar_url(user) |> MediaProxy.url()`, which is
//!   `profile_image`: the stored url, the frontend's default asset when
//!   unset, and the media proxy over a remote account's url.
//!
//! `display_name` is Pleroma's `HTML.strip_tags(user.name || user.nickname)`
//! reduced to one line of plain text, and `created_at` the Mastodon-style
//! timestamp of [`mastodon_time`] (decision 5).

use serde::Serialize;

use crate::{
    config::Config,
    entities::account::{DEFAULT_AVATAR_PATH, JsonBool, profile_image},
    models::user::{User, admin},
    util::{sanitize::sanitize_display_name, time::mastodon_time},
};

/// The `roles` object of [`AdminAccount`]: Pleroma's `roles/1`, the two
/// staff flags by their bare names.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct AdminRoles {
    /// Whether the account is an admin (`is_admin`).
    pub admin: bool,
    /// Whether the account is a moderator (`is_moderator`).
    pub moderator: bool,
}

/// One account as the admin API renders it: Pleroma's
/// `AdminAPI.AccountView` `"show.json"`.
///
/// The five status flags are [`JsonBool`]s only because a serializer cannot
/// hold ten bare booleans (Clippy's `struct_excessive_bools`); on the wire
/// they are plain `true` / `false`. `email` is `null` for an account that
/// never gave one — every remote mirror, and any local account registered
/// without an address — and `registration_reason` is `null` for the
/// registration that stated no reason.
#[derive(Debug, Serialize)]
pub struct AdminAccount {
    /// The row's `UUIDv7` as lowercase hyphenated text (decision 4).
    pub id: String,
    /// The account's email, `None` rendered as `null`.
    pub email: Option<String>,
    /// The avatar, defaulted and media-proxied as the profile entity has it.
    pub avatar: String,
    /// The full handle: `alice` locally, `alice@remote.example` for a
    /// mirror (decision 7).
    pub nickname: String,
    /// The display name as plain text, the nickname when the account has no
    /// name of its own.
    pub display_name: String,
    /// [`is_active`](User::is_active): false once an account is deactivated
    /// or suspended.
    pub is_active: JsonBool,
    /// Whether this instance owns the account.
    pub local: JsonBool,
    /// The staff roles, under their bare names.
    pub roles: AdminRoles,
    /// The profile tags Pleroma's admin UI filters on.
    pub tags: Vec<String>,
    /// Pleroma's `is_confirmed`: [`email_confirmed`](User::email_confirmed)
    /// here.
    pub is_confirmed: JsonBool,
    /// Pleroma's `is_approved`: the absence of
    /// [`approval_pending`](User::approval_pending) here.
    pub is_approved: JsonBool,
    /// Whether an admin put the account in the suggestion pool.
    pub is_suggested: JsonBool,
    /// The profile URL, [`ap_id`](User::ap_id) when the account has none.
    pub url: String,
    /// The reason given at registration, `None` rendered as `null`.
    pub registration_reason: Option<String>,
    /// The `ActivityPub` actor type.
    pub actor_type: String,
    /// When the row was inserted, in the Mastodon timestamp format.
    pub created_at: String,
}

impl AdminAccount {
    /// Renders one `users` row in Pleroma's admin shape.
    #[must_use]
    pub fn from_user(user: &User, config: &Config) -> Self {
        Self {
            id: user.id.to_string(),
            email: user.email.clone(),
            avatar: profile_image(
                user,
                config,
                user.avatar_url.as_deref(),
                DEFAULT_AVATAR_PATH,
            ),
            nickname: user.nickname.clone(),
            display_name: display_name(user),
            is_active: JsonBool(user.is_active.get()),
            local: JsonBool(user.local.get()),
            roles: AdminRoles {
                admin: user.is_admin.get(),
                moderator: user.is_moderator.get(),
            },
            tags: user.tags.clone(),
            is_confirmed: JsonBool(user.email_confirmed.get()),
            is_approved: JsonBool(!user.approval_pending.get()),
            is_suggested: JsonBool(user.is_suggested.get()),
            url: user
                .profile_url
                .clone()
                .unwrap_or_else(|| user.ap_id.clone()),
            registration_reason: user.registration_reason.clone(),
            actor_type: user.actor_type.clone(),
            created_at: mastodon_time(&user.inserted_at),
        }
    }
}

/// The whole `GET /api/v1/pleroma/admin/users` answer: Pleroma's
/// `AccountView` `"index.json"`, with all three of its keys.
///
/// The page's entities, the total the criteria select across every page,
/// and the size the page was cut to.
#[derive(Debug, Serialize)]
pub struct AdminAccountList {
    /// The page, in the listing's nickname order.
    pub users: Vec<AdminAccount>,
    /// How many accounts the criteria match, pages included.
    pub count: i64,
    /// The `page_size` of the request, its default and cap already applied.
    pub page_size: i64,
}

impl AdminAccountList {
    /// Renders one [`admin::ListPage`] under the `page_size` that produced
    /// it.
    #[must_use]
    pub fn from_page(page: &admin::ListPage, page_size: i64, config: &Config) -> Self {
        Self {
            users: page
                .users
                .iter()
                .map(|found| AdminAccount::from_user(found, config))
                .collect(),
            count: page.count,
            page_size,
        }
    }
}

/// Pleroma's `strip_tags(user.name || user.nickname)`: the display name
/// when the account has one, the handle when it does not, and either way
/// reduced to one line of plain text (the column holds sanitized markup, so
/// a name fetched from a remote actor can still carry tags).
fn display_name(user: &User) -> String {
    let stored = if user.name.is_empty() {
        user.nickname.as_str()
    } else {
        user.name.as_str()
    };
    sanitize_display_name(stored)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{DEFAULT_SCOPE, Flag, mfa},
        util::time::parse_time,
    };

    /// The fixture account's id, as stored.
    const ACCOUNT_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

    /// 2024-01-31T12:34:56.789Z, the instant every fixture row is inserted
    /// and last updated at.
    fn stamped() -> DateTime<Utc> {
        parse_time("2024-01-31T12:34:56.789Z").expect("the fixture instant")
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

    /// The entity of `user` on the test instance's config.
    fn entity(user: &User) -> AdminAccount {
        AdminAccount::from_user(user, &Config::for_tests())
    }

    /// A plain local account renders exactly the ticket's fifteen keys, the
    /// nulls spelled as nulls and the roles as their own object.
    #[test]
    fn an_account_renders_the_fifteen_pinned_keys() {
        let rendered =
            serde_json::to_value(entity(&base_user("alice"))).expect("the entity serializes");
        assert_eq!(
            rendered,
            json!({
                "id": ACCOUNT_ID,
                "email": null,
                "avatar": format!("{TEST_BASE_URL}/images/avi.png"),
                "nickname": "alice",
                "display_name": "alice",
                "is_active": true,
                "local": true,
                "roles": { "admin": false, "moderator": false },
                "tags": [],
                "is_confirmed": true,
                "is_approved": true,
                "is_suggested": false,
                "url": format!("{TEST_BASE_URL}/users/alice"),
                "registration_reason": null,
                "actor_type": "Person",
                "created_at": "2024-01-31T12:34:56.789Z",
            }),
            "no counters, no note, no surprises"
        );
    }

    /// The moderation columns come through as stored: the email, the stated
    /// registration reason, the tags and the two role flags.
    #[test]
    fn the_private_columns_are_answered_to_an_admin() {
        let staff = User {
            email: Some("root@example.org".to_owned()),
            name: "Root".to_owned(),
            is_admin: Flag::new(true),
            is_moderator: Flag::new(true),
            tags: vec!["staff".to_owned(), "ops".to_owned()],
            registration_reason: Some("please let me in".to_owned()),
            is_suggested: Flag::new(true),
            actor_type: "Service".to_owned(),
            profile_url: Some(format!("{TEST_BASE_URL}/alice")),
            ..base_user("alice")
        };
        let rendered = serde_json::to_value(entity(&staff)).expect("the entity serializes");
        assert_eq!(rendered["email"], json!("root@example.org"), "the email");
        assert_eq!(
            rendered["registration_reason"],
            json!("please let me in"),
            "and the reason"
        );
        assert_eq!(
            rendered["roles"],
            json!({ "admin": true, "moderator": true }),
            "both roles at once"
        );
        assert_eq!(rendered["tags"], json!(["staff", "ops"]), "both tags");
        assert_eq!(rendered["is_suggested"], json!(true), "and the pool flag");
        assert_eq!(rendered["actor_type"], json!("Service"), "the actor type");
        assert_eq!(
            rendered["url"],
            json!(format!("{TEST_BASE_URL}/alice")),
            "a stated profile url wins over the actor id"
        );
    }

    /// The two approval flags read the schema's own columns: an account
    /// awaiting staff approval is unapproved, one awaiting the address
    /// check unconfirmed, and a deactivated account says so.
    #[test]
    fn the_state_flags_fold_their_inverted_columns() {
        let pending = User {
            approval_pending: Flag::new(true),
            email_confirmed: Flag::new(false),
            is_active: Flag::new(false),
            ..base_user("newbie")
        };
        let rendered = serde_json::to_value(entity(&pending)).expect("the entity serializes");
        assert_eq!(rendered["is_approved"], json!(false), "approval is pending");
        assert_eq!(
            rendered["is_confirmed"],
            json!(false),
            "the email is not confirmed"
        );
        assert_eq!(
            rendered["is_active"],
            json!(false),
            "and the account is off"
        );
    }

    /// `display_name` is plain text: the stored name with its markup
    /// stripped, and the nickname when the account carries no name.
    #[test]
    fn the_display_name_is_stripped_text_or_the_nickname() {
        let named = User {
            name: "Ada <b>the Countess</b>".to_owned(),
            ..base_user("ada")
        };
        assert_eq!(
            display_name(&named),
            "Ada the Countess",
            "the tags go, the text stays"
        );
        let bracked = User {
            name: "Ada<br>Byron".to_owned(),
            ..base_user("ada")
        };
        assert_eq!(display_name(&bracked), "Ada Byron", "a break is one space");
        assert_eq!(
            display_name(&base_user("ada")),
            "ada",
            "with no name at all, the nickname"
        );
    }

    /// A remote mirror's avatar goes through the media proxy while a local
    /// account's stored url passes through untouched.
    #[test]
    fn the_avatar_follows_the_profile_entitys_own_reading() {
        let config = Config::for_tests();
        let local = User {
            avatar_url: Some(format!("{TEST_BASE_URL}/uploads/avatar.png")),
            ..base_user("alice")
        };
        assert_eq!(
            entity(&local).avatar,
            format!("{TEST_BASE_URL}/uploads/avatar.png"),
            "a local avatar is the stored url"
        );
        let mirror = User {
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            nickname: "bob@remote.example".to_owned(),
            ap_id: "https://remote.example/users/bob".to_owned(),
            avatar_url: Some("https://remote.example/avatar.png".to_owned()),
            ..base_user("bob@remote.example")
        };
        let rendered = AdminAccount::from_user(&mirror, &config);
        assert_eq!(
            rendered.avatar,
            crate::services::media_proxy::proxy_url(&config, "https://remote.example/avatar.png"),
            "a remote avatar is proxied"
        );
        assert!(
            !rendered.local.0,
            "and the entity says the account is remote"
        );
    }

    /// The list wraps the page's entities under `users` and carries the
    /// total and the page size beside them, Pleroma's `index.json`.
    #[test]
    fn the_list_wraps_the_page_with_its_total() {
        let page = admin::ListPage {
            users: vec![base_user("alice"), base_user("bob")],
            count: 137,
        };
        let rendered =
            serde_json::to_value(AdminAccountList::from_page(&page, 50, &Config::for_tests()))
                .expect("the list serializes");
        let entries = rendered["users"].as_array().expect("the users array");
        assert_eq!(entries.len(), 2, "both rows of the page");
        assert_eq!(
            entries
                .iter()
                .filter_map(|found| found["nickname"].as_str())
                .collect::<Vec<_>>(),
            vec!["alice", "bob"],
            "in the order the listing read them"
        );
        assert_eq!(
            rendered["count"],
            json!(137),
            "the whole total, not the page"
        );
        assert_eq!(
            rendered["page_size"],
            json!(50),
            "and the size it was cut to"
        );
    }

    /// The empty page renders the empty array beside its total, not a
    /// missing key.
    #[test]
    fn an_empty_page_renders_an_empty_array() {
        let page = admin::ListPage {
            users: Vec::new(),
            count: 0,
        };
        let rendered = serde_json::to_value(AdminAccountList::from_page(
            &page,
            200,
            &Config::for_tests(),
        ))
        .expect("the list serializes");
        assert_eq!(rendered["users"], json!([]), "no rows");
        assert_eq!(rendered["count"], json!(0), "nothing matched");
        assert_eq!(
            rendered["page_size"],
            json!(200),
            "the cap the request asked for"
        );
    }
}
