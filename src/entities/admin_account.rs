//! The admin API's account entities: what an admin sees of one account
//! (WBE-195, WBE-198, WBE-197).
//!
//! The reference is `Pleroma.Web.AdminAPI.AccountView`
//! (`lib/pleroma/web/admin_api/views/account_view.ex`), whose `"show.json"`
//! renders the fifteen keys the ticket pins, whose `"index.json"` wraps
//! the entity list in `{users, count, page_size}`, and whose
//! `"credentials.json"` renders the twenty-four keys of
//! [`AdminCredentials`] — the moderation form's fields, with `avatar`,
//! `banner` and `background` through `MediaProxy.url` like the listing's
//! avatar. It is a deliberately different shape from the Mastodon
//! [`Account`](super::account::Account)
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
use serde_json::Value;

use crate::{
    config::Config,
    entities::account::{
        DEFAULT_AVATAR_PATH, DEFAULT_BANNER_PATH, JsonBool, plain_display_name, profile_image,
    },
    models::user::{User, admin},
    util::time::mastodon_time,
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
    /// [`is_active`](User::is_active): false once an account is deactivated,
    /// suspended or disabled.
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
            display_name: plain_display_name(user),
            is_active: JsonBool(user.is_active.get() && !user.is_disabled()),
            local: JsonBool(user.local.get()),
            roles: AdminRoles {
                admin: user.is_admin.get(),
                moderator: user.is_moderator.get(),
            },
            tags: user.tags.clone(),
            is_confirmed: JsonBool(user.email_confirmed.get()),
            is_approved: JsonBool(!user.approval_pending.get()),
            is_suggested: JsonBool(user.is_suggested.get()),
            url: user.profile_url_or_ap_id().to_owned(),
            registration_reason: user.registration_reason.clone(),
            actor_type: user.actor_type.clone(),
            created_at: mastodon_time(&user.inserted_at),
        }
    }
}

/// One account's editable credentials as the admin API renders them:
/// Pleroma's `AdminAPI.AccountView` `"credentials.json"`, the answer of
/// `GET /api/v1/pleroma/admin/users/{nickname}/credentials` (WBE-198).
///
/// The shape is the moderation form's own: the twenty-four keys Pleroma's
/// `Map.take` lists in the view's order, the twelve flags as [`JsonBool`]s
/// (again only to keep the serializer's struct under Clippy's
/// `struct_excessive_bools`), `avatar` and `banner` through the same
/// `profile_image` defaulting and proxying [`AdminAccount`] applies, and
/// `background` the stored url alone — Pleroma's `image_url(user.background)`
/// has no default asset, so an account without one answers `null`.
/// `pleroma_settings_store` is the raw client settings blob, `fields` and
/// `raw_fields` the stored pair, and `bio` the rendered note — the values
/// the matching `PATCH` writes, read back as the row holds them.
#[derive(Debug, Serialize)]
pub struct AdminCredentials {
    /// The row's `UUIDv7` as lowercase hyphenated text (decision 4).
    pub id: String,
    /// The profile note as stored rendered html.
    pub bio: String,
    /// The account's email, `None` rendered as `null`.
    pub email: Option<String>,
    /// The profile metadata as stored: a JSON array of rendered entries.
    pub fields: Value,
    /// The display name as stored.
    pub name: String,
    /// The full handle (decision 7).
    pub nickname: String,
    /// Whether follow requests need manual approval.
    pub is_locked: JsonBool,
    /// Whether clients should render this account's content as plain text.
    pub no_rich_text: JsonBool,
    /// The visibility new statuses default to.
    pub default_scope: String,
    /// Whether the follow collection is hidden from other accounts.
    pub hide_follows: JsonBool,
    /// Whether the follower count renders as zero for others.
    pub hide_followers_count: JsonBool,
    /// Whether the follow count renders as zero for others.
    pub hide_follows_count: JsonBool,
    /// Whether the follower collection is hidden from other accounts.
    pub hide_followers: JsonBool,
    /// Whether the favourites list is private to the account.
    pub hide_favorites: JsonBool,
    /// Whether a `Move` of the account may take the following list with it.
    pub allow_following_move: JsonBool,
    /// Whether the staff flags may be shown to other accounts.
    pub show_role: JsonBool,
    /// Whether this account's statuses skip thread containment.
    pub skip_thread_containment: JsonBool,
    /// The client settings blob, stored as received.
    pub pleroma_settings_store: Value,
    /// The profile metadata as typed, beside its rendered twin.
    pub raw_fields: Value,
    /// Whether the account opted into discovery.
    pub is_discoverable: JsonBool,
    /// The `ActivityPub` actor type.
    pub actor_type: String,
    /// The avatar, defaulted and media-proxied as in [`AdminAccount`].
    pub avatar: String,
    /// The banner, defaulted to `DEFAULT_BANNER_PATH` and proxied.
    pub banner: String,
    /// The background image url as stored, with no default — `null` for an
    /// account that never set one.
    pub background: Option<String>,
}

impl AdminCredentials {
    /// Renders one `users` row in Pleroma's credentials shape.
    #[must_use]
    pub fn from_user(user: &User, config: &Config) -> Self {
        Self {
            id: user.id.to_string(),
            bio: user.bio.clone(),
            email: user.email.clone(),
            fields: user.fields.clone(),
            name: user.name.clone(),
            nickname: user.nickname.clone(),
            is_locked: JsonBool(user.locked.get()),
            no_rich_text: JsonBool(user.no_rich_text.get()),
            default_scope: user.default_scope.clone(),
            hide_follows: JsonBool(user.hide_follows.get()),
            hide_followers_count: JsonBool(user.hide_followers_count.get()),
            hide_follows_count: JsonBool(user.hide_follows_count.get()),
            hide_followers: JsonBool(user.hide_followers.get()),
            hide_favorites: JsonBool(user.hide_favorites.get()),
            allow_following_move: JsonBool(user.allow_following_move.get()),
            show_role: JsonBool(user.show_role.get()),
            skip_thread_containment: JsonBool(user.skip_thread_containment.get()),
            pleroma_settings_store: user.settings_store.clone(),
            raw_fields: user.raw_fields.clone(),
            is_discoverable: JsonBool(user.discoverable.get()),
            actor_type: user.actor_type.clone(),
            avatar: profile_image(
                user,
                config,
                user.avatar_url.as_deref(),
                DEFAULT_AVATAR_PATH,
            ),
            banner: profile_image(
                user,
                config,
                user.banner_url.as_deref(),
                DEFAULT_BANNER_PATH,
            ),
            background: user.background_image_url.clone(),
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

/// One account's two staff groups as the admin API's permission-group
/// reads answer them.
///
/// The shape is Pleroma's `right_get` map, `is_moderator` first and
/// `is_admin` beside it, the column names rather than the `roles` object
/// [`AdminAccount`] nests them in.
///
/// Both flags come back whichever group the request named — Pleroma's
/// `GET /users/:nickname/permission_group/:permission_group` route runs the
/// very same `right_get` as its group-less twin — and the write pair answers
/// only the one key it moved, which
/// [`crate::api::admin::users`] builds from
/// [`crate::models::user::admin::Role::column`] instead of from this type.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct AdminPermissionGroup {
    /// Whether the account is a moderator (`is_moderator`).
    pub is_moderator: bool,
    /// Whether the account is an admin (`is_admin`).
    pub is_admin: bool,
}

impl AdminPermissionGroup {
    /// The two staff flags of one `users` row.
    #[must_use]
    pub const fn from_user(user: &User) -> Self {
        Self {
            is_moderator: user.is_moderator.get(),
            is_admin: user.is_admin.get(),
        }
    }
}

/// The whole `{"users": […]}` answer of the admin suggest pair
/// (WBE-197).
///
/// This is Pleroma's `AccountView` `"index.json"` clause that takes the
/// list alone, without [`AdminAccountList`]'s `count` and `page_size`,
/// because a write answers its subjects and not a page.
#[derive(Debug, Serialize)]
pub struct AdminAccounts {
    /// The accounts, in the order the request named them.
    pub users: Vec<AdminAccount>,
}

impl AdminAccounts {
    /// Renders `users` as admin entities under the one `users` key.
    #[must_use]
    pub fn from_users(users: &[User], config: &Config) -> Self {
        Self {
            users: users
                .iter()
                .map(|found| AdminAccount::from_user(found, config))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

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
            suspended_at: None,
            disabled_at: None,
            remote_suspended: Flag::new(false),
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
                "avatar": format!("{TEST_BASE_URL}/media/default/avi.png"),
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
            plain_display_name(&named),
            "Ada the Countess",
            "the tags go, the text stays"
        );
        let bracked = User {
            name: "Ada<br>Byron".to_owned(),
            ..base_user("ada")
        };
        assert_eq!(
            plain_display_name(&bracked),
            "Ada Byron",
            "a break is one space"
        );
        assert_eq!(
            plain_display_name(&base_user("ada")),
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

    /// The ticket's twenty-four credentials keys, in the view's order:
    /// Pleroma's `Map.take` list with the three images appended.
    const CREDENTIAL_KEYS: [&str; 24] = [
        "id",
        "bio",
        "email",
        "fields",
        "name",
        "nickname",
        "is_locked",
        "no_rich_text",
        "default_scope",
        "hide_follows",
        "hide_followers_count",
        "hide_follows_count",
        "hide_followers",
        "hide_favorites",
        "allow_following_move",
        "show_role",
        "skip_thread_containment",
        "pleroma_settings_store",
        "raw_fields",
        "is_discoverable",
        "actor_type",
        "avatar",
        "banner",
        "background",
    ];

    /// A default account renders the full credentials form: the stored
    /// flags as plain booleans, the defaults for the two images, `null`
    /// for email and background, and nothing beyond the pinned keys. (The
    /// ticket's KEY ORDER is the struct's declaration order: a parsed JSON
    /// value's map sorts, so only the exact key SET is assertable here,
    /// and the wire order is the handler's `Json` serializer's.)
    #[test]
    fn the_credentials_entity_pins_the_twenty_four_keys() {
        let rendered = serde_json::to_value(AdminCredentials::from_user(
            &base_user("alice"),
            &Config::for_tests(),
        ))
        .expect("the entity serializes");
        let keys = rendered
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<&str>>();
        assert_eq!(
            keys,
            CREDENTIAL_KEYS.iter().copied().collect::<BTreeSet<&str>>(),
            "exactly the twenty-four pinned keys"
        );
        assert_eq!(
            rendered,
            json!({
                "id": ACCOUNT_ID,
                "bio": "",
                "email": null,
                "fields": [],
                "name": "",
                "nickname": "alice",
                "is_locked": false,
                "no_rich_text": false,
                "default_scope": "public",
                "hide_follows": false,
                "hide_followers_count": false,
                "hide_follows_count": false,
                "hide_followers": false,
                "hide_favorites": true,
                "allow_following_move": true,
                "show_role": true,
                "skip_thread_containment": false,
                "pleroma_settings_store": {},
                "raw_fields": [],
                "is_discoverable": false,
                "actor_type": "Person",
                "avatar": format!("{TEST_BASE_URL}/media/default/avi.png"),
                "banner": format!("{TEST_BASE_URL}/media/default/banner.png"),
                "background": null,
            }),
            "the column defaults, spelled out"
        );
    }

    /// The permission-group read answers exactly Pleroma's `right_get` map:
    /// both flags, always, under their column names — the group the route
    /// segment named is not a filter on the answer. A mirror's row answers
    /// the same shape with both flags false.
    #[test]
    fn the_permission_group_answers_both_flags() {
        let plain = serde_json::to_value(AdminPermissionGroup::from_user(&base_user("alice")))
            .expect("the group serializes");
        assert_eq!(
            plain,
            json!({ "is_moderator": false, "is_admin": false }),
            "an ordinary account holds neither group"
        );
        let staff = User {
            is_admin: Flag::new(true),
            is_moderator: Flag::new(true),
            ..base_user("root")
        };
        assert_eq!(
            serde_json::to_value(AdminPermissionGroup::from_user(&staff))
                .expect("the group serializes"),
            json!({ "is_moderator": true, "is_admin": true }),
            "and both flags come back at once"
        );
    }

    /// The suggest pair's answer is Pleroma's `index.json` clause that
    /// carries the list alone: one `users` key, no `count`, no `page_size`,
    /// the entries the same admin entities the show renders.
    #[test]
    fn the_suggestion_answer_wraps_the_list_alone() {
        let moved = vec![base_user("alice"), base_user("bob")];
        let rendered =
            serde_json::to_value(AdminAccounts::from_users(&moved, &Config::for_tests()))
                .expect("the answer serializes");
        assert_eq!(
            rendered
                .as_object()
                .expect("an object")
                .keys()
                .collect::<Vec<_>>(),
            vec!["users"],
            "exactly the one key"
        );
        let entries = rendered["users"].as_array().expect("the users array");
        assert_eq!(entries.len(), 2, "both moved accounts");
        assert_eq!(
            entries
                .iter()
                .filter_map(|found| found["nickname"].as_str())
                .collect::<Vec<_>>(),
            vec!["alice", "bob"],
            "in the order the request named them"
        );
        assert_eq!(
            rendered["users"][0]["is_suggested"],
            json!(false),
            "the flag the write just set rides in the entity"
        );
        assert_eq!(
            serde_json::to_value(AdminAccounts::from_users(&[], &Config::for_tests()))
                .expect("the empty answer serializes"),
            json!({ "users": [] }),
            "and a write that moved nobody answers the empty list"
        );
    }

    /// The writable columns come through as stored — the email, the note,
    /// the settings blob, the metadata pair — while the images keep the
    /// entity's own defaulting: a stated background is the stored url, and
    /// there is no default for it.
    #[test]
    fn the_credentials_answer_the_written_columns() {
        let written = User {
            email: Some("alice@example.org".to_owned()),
            bio: "<p>hello</p>".to_owned(),
            name: "Alice".to_owned(),
            locked: Flag::new(true),
            discoverable: Flag::new(true),
            default_scope: "private".to_owned(),
            fields: json!([{"name": "web", "value": "<a href=\"x\">x</a>"}]),
            raw_fields: json!([{"name": "web", "value": "x"}]),
            settings_store: json!({"config": {"fe": "mochi-pub"}}),
            actor_type: "Service".to_owned(),
            banner_url: Some(format!("{TEST_BASE_URL}/uploads/banner.png")),
            background_image_url: Some(format!("{TEST_BASE_URL}/uploads/bg.png")),
            ..base_user("alice")
        };
        let rendered =
            serde_json::to_value(AdminCredentials::from_user(&written, &Config::for_tests()))
                .expect("the entity serializes");
        assert_eq!(rendered["email"], json!("alice@example.org"));
        assert_eq!(rendered["bio"], json!("<p>hello</p>"));
        assert_eq!(rendered["name"], json!("Alice"));
        assert_eq!(rendered["is_locked"], json!(true));
        assert_eq!(rendered["is_discoverable"], json!(true));
        assert_eq!(rendered["default_scope"], json!("private"));
        assert_eq!(rendered["actor_type"], json!("Service"));
        assert_eq!(
            rendered["pleroma_settings_store"],
            json!({"config": {"fe": "mochi-pub"}}),
            "the blob is answered as stored"
        );
        assert_eq!(
            rendered["fields"],
            json!([{"name": "web", "value": "<a href=\"x\">x</a>"}]),
            "the rendered pair"
        );
        assert_eq!(
            rendered["raw_fields"],
            json!([{"name": "web", "value": "x"}]),
            "and the raw one"
        );
        assert_eq!(
            rendered["banner"],
            json!(format!("{TEST_BASE_URL}/uploads/banner.png")),
            "a stored banner is the plain url"
        );
        assert_eq!(
            rendered["background"],
            json!(format!("{TEST_BASE_URL}/uploads/bg.png")),
            "and the stored background"
        );
    }
}
