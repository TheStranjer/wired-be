//! The Mastodon admin API's account entity: what `GET /api/v{1,2}/admin/accounts`
//! answers for one account (WBE-210).
//!
//! The reference is `Pleroma.Web.MastodonAPI.Admin.AccountView`
//! (`lib/pleroma/web/mastodon_api/admin/views/account_view.ex`), whose
//! `"show.json"` renders the sixteen keys of [`AdminAccount`] — Mastodon's
//! admin account entity, which is *not* the moderation-console row
//! [`admin_account::AdminAccount`](super::admin_account::AdminAccount)
//! renders: this one keeps the whole [`Account`] profile under `account` and
//! adds only the private columns an admin is owed — `email`, the registration
//! reason as `invite_request`, the two approval flags and the role.
//!
//! Four of Pleroma's renderings are the ticket's, not the view's:
//!
//! - `role` is Pleroma's bare `"admin"` / `"moderator"` / `null` string; the
//!   ticket pins Mastodon 4.x's role *object* instead, which is what
//!   [`AdminRole::for_user`] builds.
//! - `locale` is Pleroma's `nil`; the ticket pins the empty string, which is
//!   what a Mastodon account with no interface language reads as.
//! - `suspended` and `silenced` are Pleroma's `nil`s; the ticket pins
//!   `false`, and this schema has neither a suspension nor a silence column
//!   to weigh, so both stay `false` on every row.
//!
//! `username` and `domain` are Pleroma's `username_from_nickname` /
//! `domain_from_nickname` pair, the handle's two halves: decision 7 stores a
//! mirror's nickname as `nick@host` and a local one as `nick`, so a local
//! account's `domain` is `null` and a mirror's is the host its row carries.
//! `created_at` is the Mastodon-style timestamp of [`mastodon_time`]
//! (decision 5), and `account` the plain [`Account`] render: Pleroma's admin
//! view renders the profile with no viewer and `skip_visibility_check`, and
//! the moderation facts of the row live in the keys around it, not in it.

use serde::Serialize;

use crate::{
    config::Config,
    entities::account::{Account, JsonBool, nickname_part},
    models::user::User,
    util::time::mastodon_time,
};

/// Mastodon 4.x's role object as [`AdminAccount`] renders it: the five keys
/// the ticket pins.
///
/// The built-in ids and names are spelled out and everything a custom role
/// would carry (colour, bitmask, badge) sits at the built-in role's own value.
/// The four texts are constants because the three roles this instance can
/// answer are the built-in ones: id `1` is Mastodon's administrator, `2` its
/// moderator, and the `-99` of [`AdminRole::for_user`]'s last arm the
/// placeholder Mastodon answers for an account that holds no role at all.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct AdminRole {
    /// The role's id as text. (Decision 4's `UUIDv7` rule is about *rows*; a
    /// Mastodon role is a small integer and stays the text of that integer.)
    pub id: &'static str,
    /// The role's name, and the empty string for an account that holds none.
    pub name: &'static str,
    /// The badge colour: Mastodon's built-in roles are uncoloured.
    pub color: &'static str,
    /// The permission bitmask as text.
    pub permissions: &'static str,
    /// Whether the role is shown beside the account's name.
    pub highlighted: bool,
}

impl AdminRole {
    /// The role of one `users` row: admin before moderator, and the unnamed
    /// placeholder for the account that holds neither.
    ///
    /// Pleroma's `role/1` matches `is_admin` first, so an account holding both
    /// flags is the higher of the two roles.
    #[must_use]
    pub const fn for_user(user: &User) -> Self {
        if user.is_admin.get() {
            Self {
                id: "1",
                name: "Admin",
                color: "",
                permissions: "1",
                highlighted: true,
            }
        } else if user.is_moderator.get() {
            Self {
                id: "2",
                name: "Moderator",
                color: "",
                // Nothing of this instance's corresponds to Mastodon's
                // permission bitmask, so every role but the administrator's
                // states the empty one.
                permissions: "0",
                highlighted: true,
            }
        } else {
            Self {
                id: "-99",
                name: "",
                color: "",
                permissions: "0",
                highlighted: false,
            }
        }
    }
}

/// One account as the Mastodon admin API renders it: Pleroma's
/// `MastodonAPI.Admin.AccountView` `"show.json"`.
///
/// It is the entity both `GET /api/v1/admin/accounts` and its v2 twin page
/// through, beside the `…/admin/accounts/{id}` show route. The five state
/// flags are [`JsonBool`]s only because a serializer cannot hold ten bare
/// booleans (Clippy's `struct_excessive_bools`, threshold 2); on the wire they
/// are plain `true` / `false`. `email` and `invite_request` are `null` for an
/// account that never gave one — every remote mirror, and any local account
/// registered without an address or a reason — and `ip` is `null` on every
/// row, because this schema keeps no login address; `ips`, `locale`,
/// `suspended` and `silenced` are the empty and false readings the ticket pins
/// for the same reason.
#[derive(Debug, Serialize)]
pub struct AdminAccount {
    /// The row's `UUIDv7` as lowercase hyphenated text (decision 4).
    pub id: String,
    /// The part of the handle before the `@`.
    pub username: String,
    /// The host of a mirror's handle, `null` for a local account.
    pub domain: Option<String>,
    /// When the row was inserted, in the Mastodon timestamp format.
    pub created_at: String,
    /// The account's email, `None` rendered as `null`.
    pub email: Option<String>,
    /// The last address the account signed in from: this schema stores none,
    /// so `null` on every row.
    pub ip: Option<String>,
    /// Every address the account signed in from, likewise: the empty list.
    pub ips: Vec<String>,
    /// The interface language, the empty string this instance answers for
    /// every account.
    pub locale: String,
    /// The reason given at registration, `None` rendered as `null` —
    /// Mastodon's `invite_request` for an instance without invitations.
    pub invite_request: Option<String>,
    /// The account's role, as Mastodon's role object.
    pub role: AdminRole,
    /// Pleroma's `is_confirmed`: [`email_confirmed`](User::email_confirmed)
    /// here.
    pub confirmed: JsonBool,
    /// Whether the account is suspended ([`User::is_suspended`]): staff
    /// suspended it, its origin server did, or its deletion is under way.
    pub suspended: JsonBool,
    /// Whether staff silenced it; nothing silences one here.
    pub silenced: JsonBool,
    /// Pleroma's `disabled`: the account is inactive without being
    /// [suspended](Self::suspended), so the two never both read true.
    pub disabled: JsonBool,
    /// Pleroma's `approved`: the absence of
    /// [`approval_pending`](User::approval_pending) here.
    pub approved: JsonBool,
    /// The public profile entity, as an anonymous reader sees it.
    pub account: Account,
}

impl AdminAccount {
    /// Renders one `users` row in Mastodon's admin shape on the instance
    /// `config` describes.
    #[must_use]
    pub fn from_user(user: &User, config: &Config) -> Self {
        Self {
            id: user.id.to_string(),
            username: nickname_part(&user.nickname).to_owned(),
            domain: user.domain.clone(),
            created_at: mastodon_time(&user.inserted_at),
            email: user.email.clone(),
            ip: None,
            ips: Vec::new(),
            locale: String::new(),
            invite_request: user.registration_reason.clone(),
            role: AdminRole::for_user(user),
            confirmed: JsonBool(user.email_confirmed.get()),
            suspended: JsonBool(user.is_suspended()),
            silenced: JsonBool(false),
            disabled: JsonBool(!user.is_active.get() && !user.is_suspended()),
            approved: JsonBool(!user.approval_pending.get()),
            account: Account::from_user(user, config),
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

    /// The entity's sixteen keys, in the view's order.
    const ENTITY_KEYS: [&str; 16] = [
        "id",
        "username",
        "domain",
        "created_at",
        "email",
        "ip",
        "ips",
        "locale",
        "invite_request",
        "role",
        "confirmed",
        "suspended",
        "silenced",
        "disabled",
        "approved",
        "account",
    ];

    /// The role object's five keys.
    const ROLE_KEYS: [&str; 5] = ["id", "name", "color", "permissions", "highlighted"];

    /// 2024-01-31T12:34:56.789Z, the instant every fixture row is inserted.
    fn stamped() -> DateTime<Utc> {
        parse_time("2024-01-31T12:34:56.789Z").expect("the fixture instant")
    }

    /// A `User` row named `nickname`, active, local and approved, at the
    /// column defaults a freshly registered account carries.
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
            remote_suspended: Flag::new(false),
        }
    }

    /// The entity of `user` on the test instance's config.
    fn entity(user: &User) -> serde_json::Value {
        serde_json::to_value(AdminAccount::from_user(user, &Config::for_tests()))
            .expect("the entity serializes")
    }

    /// The keys of one rendered object, sorted so that a missing or an extra
    /// one shows up as a diff rather than a lucky pass.
    fn keys(value: &serde_json::Value) -> BTreeSet<&str> {
        value
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    /// A plain local account renders exactly the sixteen keys the ticket
    /// pins, the four facts this schema does not keep as the empty readings
    /// they are, and the profile entity beside them.
    #[test]
    fn an_account_renders_the_sixteen_pinned_keys() {
        let alice = base_user("alice");
        let rendered = entity(&alice);
        assert_eq!(
            keys(&rendered),
            ENTITY_KEYS.iter().copied().collect::<BTreeSet<&str>>(),
            "exactly the sixteen pinned keys"
        );
        assert_eq!(
            rendered,
            json!({
                "id": ACCOUNT_ID,
                "username": "alice",
                "domain": null,
                "created_at": "2024-01-31T12:34:56.789Z",
                "email": null,
                "ip": null,
                "ips": [],
                "locale": "",
                "invite_request": null,
                "role": {
                    "id": "-99",
                    "name": "",
                    "color": "",
                    "permissions": "0",
                    "highlighted": false,
                },
                "confirmed": true,
                "suspended": false,
                "silenced": false,
                "disabled": false,
                "approved": true,
                "account": rendered["account"],
            }),
            "the profile entity rides under `account`"
        );
        assert_eq!(
            keys(&rendered["account"]),
            keys(
                &serde_json::to_value(Account::from_user(&alice, &Config::for_tests()))
                    .expect("the profile serializes")
            ),
            "and it is the plain anonymous render, keys included"
        );
    }

    /// A mirror splits into the two halves of its stored handle: the head as
    /// `username`, the host as `domain`, where a local account's domain is
    /// the `null` the ticket pins.
    #[test]
    fn a_mirror_splits_into_username_and_domain() {
        let mirror = User {
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            ..base_user("bob@remote.example")
        };
        let rendered = entity(&mirror);
        assert_eq!(rendered["username"], json!("bob"), "the handle's head");
        assert_eq!(rendered["domain"], json!("remote.example"), "and its host");
        assert_eq!(
            entity(&base_user("alice"))["domain"],
            json!(null),
            "a local account has no domain to name"
        );
    }

    /// The three staff readings of the role object, and the administrator's
    /// precedence over the moderator when one account holds both flags.
    #[test]
    fn the_role_object_names_the_held_role() {
        let admin = User {
            is_admin: Flag::new(true),
            ..base_user("root")
        };
        let both = User {
            is_admin: Flag::new(true),
            is_moderator: Flag::new(true),
            ..base_user("root")
        };
        assert_eq!(
            entity(&both)["role"],
            json!({
                "id": "1",
                "name": "Admin",
                "color": "",
                "permissions": "1",
                "highlighted": true,
            }),
            "admin outranks moderator, as Pleroma's first clause does"
        );
        let moderator = User {
            is_moderator: Flag::new(true),
            ..base_user("moddy")
        };
        assert_eq!(
            entity(&moderator)["role"],
            json!({
                "id": "2",
                "name": "Moderator",
                "color": "",
                "permissions": "0",
                "highlighted": true,
            }),
            "the moderator's own object"
        );
        assert_eq!(
            keys(&entity(&moderator)["role"]),
            ROLE_KEYS.iter().copied().collect::<BTreeSet<&str>>(),
            "five keys, never a sixth"
        );
        assert_eq!(
            entity(&admin)["role"],
            entity(&both)["role"],
            "and the admin flag alone already decides it"
        );
    }

    /// The private columns answer the admin — the email and the registration
    /// reason as `invite_request` — and the four state flags read their own
    /// (inverted) columns.
    #[test]
    fn the_private_columns_and_the_state_flags_read_the_row() {
        let pending = User {
            email: Some("newbie@example.org".to_owned()),
            registration_reason: Some("please let me in".to_owned()),
            approval_pending: Flag::new(true),
            email_confirmed: Flag::new(false),
            is_active: Flag::new(false),
            ..base_user("newbie")
        };
        let rendered = entity(&pending);
        assert_eq!(rendered["email"], json!("newbie@example.org"));
        assert_eq!(
            rendered["invite_request"],
            json!("please let me in"),
            "the reason is Mastodon's invite request"
        );
        assert_eq!(rendered["approved"], json!(false), "approval is pending");
        assert_eq!(
            rendered["confirmed"],
            json!(false),
            "the address is not checked"
        );
        assert_eq!(
            rendered["disabled"],
            json!(true),
            "an inactive account is a disabled one"
        );
        assert_eq!(
            rendered["suspended"],
            json!(false),
            "and suspension is not the same fact"
        );
    }

    /// A suspension, local or remote, reads `suspended` and not `disabled`,
    /// whether or not the row is also inactive (WBE-878).
    #[test]
    fn a_suspension_reads_suspended_and_not_disabled() {
        let local = User {
            is_active: Flag::new(false),
            suspended_at: Some(stamped()),
            ..base_user("local")
        };
        let remote = User {
            is_active: Flag::new(false),
            remote_suspended: Flag::new(true),
            ..base_user("remote")
        };
        for (name, row) in [("local", &local), ("remote", &remote)] {
            let rendered = entity(row);
            assert_eq!(rendered["suspended"], json!(true), "the {name} suspension");
            assert_eq!(
                rendered["disabled"],
                json!(false),
                "is not also a disabled account ({name})"
            );
        }
    }
}
