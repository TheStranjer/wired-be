//! The Mastodon `Account` entity: its core fields.
//!
//! The reference shape is Pleroma's `AccountView.do_render("show.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/account_view.ex`); the JSON keys are
//! emitted exactly as that view names them, restricted to the core fields.
//! [`Account::from_user`] is pure (decision 3): it renders a [`User`] row
//! and the instance [`Config`] — no database, so remote users' counts come
//! from the stored counters and nothing is recomputed per request.
//!
//! Deliberately absent here: the `source` and `pleroma` extension objects
//! (later tickets add them as `Account` options that `skip_serializing_if`
//! they are unset), `hide_collections`, and any media proxying.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::{config::Config, models::user::User};

/// The `actor_type` value that makes the entity's `group` flag true.
const GROUP_ACTOR_TYPE: &str = "Group";

/// Path appended to [`Config::base_url`] for the default avatar, matching
/// Pleroma's `[:assets, :default_user_avatar]` fallback
/// (`lib/pleroma/user.ex`, `avatar_url/2`).
const DEFAULT_AVATAR_PATH: &str = "/images/avi.png";

/// Path appended to [`Config::base_url`] for the default banner, matching
/// Pleroma's `banner_url/2` fallback.
const DEFAULT_BANNER_PATH: &str = "/images/banner.png";

/// Format of `last_status_at`: a bare `YYYY-MM-DD` date, the counterpart
/// of Pleroma's `NaiveDateTime.to_date() |> Date.to_iso8601()`.
const DATE_FORMAT: &str = "%Y-%m-%d";

/// A JSON boolean field of an [`Account`].
///
/// `Account` carries six booleans, past the denied `struct_excessive_bools`
/// threshold, so each is wrapped in this newtype; serde serializes a
/// newtype struct as its inner value, so on the wire these stay plain
/// `true`/`false`.
#[derive(Debug, Serialize)]
pub struct JsonBool(pub bool);

/// One profile metadata line, as Mastodon renders it: the stored
/// `{name, value}` pair of the `fields` jsonb plus a verification time.
#[derive(Debug, Serialize)]
pub struct Field {
    /// The field label, as the user typed it.
    pub name: String,
    /// The field contents as sanitized HTML.
    pub value: String,
    /// When a linked `value` was verified against the account, in Mastodon
    /// timestamp format; nothing verifies links yet, so this is always
    /// JSON `null`.
    pub verified_at: Option<String>,
}

/// The Mastodon `Account` entity (core fields).
///
/// Build one with [`Account::from_user`]; its [`Serialize`] impl emits
/// exactly the JSON keys listed in `docs/ARCHITECTURE.md` decision 5's
/// image of the Mastodon API, in declaration order.
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
    /// Number of followers (stored counter).
    pub followers_count: i32,
    /// Number of follows (stored counter).
    pub following_count: i32,
    /// Number of posted statuses (stored counter).
    pub statuses_count: i32,
    /// Date of the newest status as `YYYY-MM-DD`, or `null`.
    pub last_status_at: Option<String>,
    /// Custom emojis used in the profile; always the empty array until
    /// the emoji ticket fills it.
    pub emojis: Vec<Value>,
    /// Profile metadata, in storage order.
    pub fields: Vec<Field>,
    /// The account this one moved to; always `null` until the move ticket
    /// fills it with a rendered `Account`.
    pub moved: Option<Value>,
    /// Whether the account is deactivated: `is_active` inverted.
    pub suspended: JsonBool,
    /// Whether the account is rate-limited by moderation; nothing limits
    /// accounts yet, so this is always false.
    pub limited: JsonBool,
}

impl Account {
    /// Renders `user` as an `Account` for the instance described by
    /// `config`.
    ///
    /// Pure (decision 3): every value comes from the row, from `config`,
    /// or from a documented constant — counts included, which for remote
    /// users are exactly the counters federation stored.
    #[must_use]
    pub fn from_user(user: &User, config: &Config) -> Self {
        let avatar = user
            .avatar_url
            .clone()
            .unwrap_or_else(|| format!("{}{DEFAULT_AVATAR_PATH}", config.base_url));
        let header = user
            .banner_url
            .clone()
            .unwrap_or_else(|| format!("{}{DEFAULT_BANNER_PATH}", config.base_url));
        let url = user
            .profile_url
            .clone()
            .unwrap_or_else(|| user.ap_id.clone());
        Self {
            id: user.id.to_string(),
            username: nickname_part(&user.nickname).to_owned(),
            acct: user.nickname.clone(),
            fqn: full_nickname(user, config),
            display_name: user.name.clone(),
            locked: JsonBool(user.locked.get()),
            bot: JsonBool(user.bot.get()),
            discoverable: JsonBool(user.discoverable.get()),
            group: JsonBool(user.actor_type == GROUP_ACTOR_TYPE),
            created_at: user.inserted_at,
            note: user.bio.clone(),
            url,
            uri: user.ap_id.clone(),
            avatar: avatar.clone(),
            avatar_static: avatar,
            header: header.clone(),
            header_static: header,
            followers_count: user.followers_count,
            following_count: user.following_count,
            statuses_count: user.statuses_count,
            last_status_at: user
                .last_status_at
                .map(|at| at.format(DATE_FORMAT).to_string()),
            emojis: Vec::new(),
            fields: fields_from_json(&user.fields),
            moved: None,
            suspended: JsonBool(!user.is_active.get()),
            limited: JsonBool(false),
        }
    }
}

/// The web profile URL of the local account `nickname` on this instance:
/// `{base_url}/users/{nickname}`, the same shape as
/// [`local_ap_id`](crate::models::user::local_ap_id) but built from the
/// config instead of a raw base url.
#[must_use]
pub fn local_url(config: &Config, nickname: &str) -> String {
    format!("{}/users/{nickname}", config.base_url)
}

/// The part of `nickname` before the first `@` — Pleroma's
/// `username_from_nickname`.
fn nickname_part(nickname: &str) -> &str {
    nickname
        .split_once('@')
        .map_or(nickname, |(nick, _domain)| nick)
}

/// The fully-qualified `nick@domain` handle (Pleroma's
/// `User.full_nickname`): a remote nickname is stored with its host and
/// passes through; a local one gains [`Config::domain`].
fn full_nickname(user: &User, config: &Config) -> String {
    if user.nickname.contains('@') {
        user.nickname.clone()
    } else {
        format!("{}@{}", user.nickname, config.domain())
    }
}

/// Maps the `fields` jsonb — an array of `{name, value}` objects whose
/// value is sanitized HTML — into entity [`Field`]s. Entries without a
/// string `name` and `value` are skipped, and stored jsonb of any other
/// shape renders as the empty list, so malformed storage never fails a
/// response.
fn fields_from_json(stored: &Value) -> Vec<Field> {
    stored
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let name = entry.get("name").and_then(Value::as_str)?;
                    let value = entry.get("value").and_then(Value::as_str)?;
                    Some(Field {
                        name: name.to_owned(),
                        value: value.to_owned(),
                        verified_at: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::{models::user::Flag, util::time::parse_time};

    /// The hyphenated lowercase id every fixture row carries.
    const ACCOUNT_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// The instance origin of [`Config::for_tests`].
    const BASE: &str = "https://test.example";
    /// Every JSON key the core entity must emit, sorted.
    const EXPECTED_KEYS: [&str; 26] = [
        "acct",
        "avatar",
        "avatar_static",
        "bot",
        "created_at",
        "discoverable",
        "display_name",
        "emojis",
        "fields",
        "followers_count",
        "following_count",
        "fqn",
        "group",
        "header",
        "header_static",
        "id",
        "last_status_at",
        "limited",
        "locked",
        "moved",
        "note",
        "statuses_count",
        "suspended",
        "uri",
        "url",
        "username",
    ];

    /// 2024-01-31T12:34:56.789Z, the instant every fixture row is
    /// inserted and last updated at.
    fn stamped() -> DateTime<Utc> {
        parse_time("2024-01-31T12:34:56.789Z").unwrap()
    }

    /// A [`User`] row named `nickname` with actor id `ap_id`, at the field
    /// defaults a freshly federated mirror carries: a remote `Person`, no
    /// images, no profile url, empty text, zero counts, active.
    fn base_user(nickname: &str, ap_id: &str) -> User {
        User {
            id: Uuid::parse_str(ACCOUNT_ID).unwrap(),
            nickname: nickname.to_owned(),
            ap_id: ap_id.to_owned(),
            local: Flag::new(false),
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
            fields: serde_json::json!([]),
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
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            last_fetched_at: None,
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// `user` rendered to a JSON value; these structs always serialize.
    fn render(user: &User, config: &Config) -> Value {
        serde_json::to_value(Account::from_user(user, config)).unwrap()
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
        assert_eq!(
            value,
            serde_json::json!({
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
            }),
            "local account snapshot"
        );
        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exact key set");
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
        assert_eq!(
            value,
            serde_json::json!({
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
                "avatar": "https://test.example/images/avi.png",
                "avatar_static": "https://test.example/images/avi.png",
                "header": "https://test.example/images/banner.png",
                "header_static": "https://test.example/images/banner.png",
                "followers_count": 12,
                "following_count": 3,
                "statuses_count": 48,
                "last_status_at": null,
                "emojis": [],
                "fields": [],
                "moved": null,
                "suspended": false,
                "limited": false,
            }),
            "remote account snapshot: counts come from the stored counters, \
             the handle already carries the host, and url falls back to ap_id"
        );
    }

    #[test]
    fn null_images_and_urls_fall_back_to_the_instance() {
        let config = Config::for_tests();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        alice.local = Flag::new(true);
        let value = render(&alice, &config);
        assert_eq!(
            value["avatar"],
            serde_json::json!(format!("{BASE}/images/avi.png")),
            "a missing avatar gets the instance default"
        );
        assert_eq!(
            value["header"],
            serde_json::json!(format!("{BASE}/images/banner.png")),
            "a missing banner gets the instance default"
        );
        assert_eq!(
            value["url"],
            serde_json::json!(format!("{BASE}/users/alice")),
            "a missing profile_url renders the actor id"
        );
        assert_eq!(
            value["fqn"],
            serde_json::json!("alice@test.example"),
            "a local nickname gains the config domain"
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
    fn fields_map_to_name_value_and_null_verified_at() {
        let config = Config::for_tests();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        alice.fields = serde_json::json!([
            {"name": "one", "value": "<b>1</b>"},
            {"name": "two", "value": "2", "verified_at": "2020-01-01"},
            {"name": "no value"},
            {"name": 3, "value": "4"},
            "not an object",
        ]);
        let value = render(&alice, &config);
        assert_eq!(
            value["fields"],
            serde_json::json!([
                {"name": "one", "value": "<b>1</b>", "verified_at": null},
                {"name": "two", "value": "2", "verified_at": null},
            ]),
            "entries map in order with verified_at null; malformed ones are skipped"
        );

        alice.fields = serde_json::json!({"not": "an array"});
        let malformed = render(&alice, &config);
        assert_eq!(
            malformed["fields"],
            serde_json::json!([]),
            "jsonb of another shape renders as the empty list"
        );
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

    #[test]
    fn local_url_builds_the_profile_url_from_the_config() {
        let config = Config::for_tests();
        assert_eq!(
            local_url(&config, "alice"),
            "https://test.example/users/alice",
            "the profile url is the base url plus the users path and nickname"
        );
    }
}
