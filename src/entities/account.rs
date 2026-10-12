//! The Mastodon `Account` entity: its core fields, `source` object and
//! `pleroma` block.
//!
//! The reference shape is Pleroma's `AccountView.do_render("show.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/account_view.ex`); the JSON keys are
//! emitted exactly as that view names them, restricted to the core fields
//! and the Pleroma extension. [`Account::from_user`] is pure (decision 3):
//! it renders a [`User`] row and the instance [`Config`] — no database, so
//! remote users' counts come from the stored counters and nothing is
//! recomputed per request.
//!
//! [`Account::source`] is the one key not every render answers with:
//! Pleroma puts the editing block in only when the viewer is the account
//! itself, so it is an `Option` that `skip_serializing_if` hides and only
//! [`Account::for_owner`] — the `verify_credentials` and
//! `update_credentials` render — sets. Deliberately absent here:
//! `hide_collections` and any media proxying.
//!
//! The `pleroma` block is [`PleromaMetadata`]: it renders the Pleroma
//! columns of `users` (see the `users_pleroma_extensions` migration), and
//! the seven keys Pleroma only sends to the account's own user are gated
//! on the *viewer* — the account making the request, Mastodon's
//! `AccountViewer`. Pass it with [`Account::from_user_for`];
//! [`Account::from_user`] renders the anonymous view.
//!
//! Two privacy rules follow the viewer too, as in Pleroma's `show.json`
//! renderer: a hidden follow/follower count renders as `0` for everyone
//! but the owner, and the admin/moderator flags render `false` for
//! everyone but the owner when the account asked not to show its role.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    config::Config, entities::relationship::RelationshipMap, models::user::User,
    util::sanitize::strip_html,
};

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
#[derive(Clone, Debug, Serialize)]
pub struct JsonBool(pub bool);

/// One profile metadata line, as Mastodon renders it: the stored
/// `{name, value}` pair of the `fields` jsonb plus a verification time.
///
/// The same shape serves both renderings: the top-level
/// [`fields`](Account::fields) carry the stored value as sanitized HTML,
/// while [`AccountSource::fields`] hands the owner the raw text to edit.
#[derive(Debug, Serialize)]
pub struct Field {
    /// The field label, as the user typed it.
    pub name: String,
    /// The field contents: sanitized HTML in the top-level entity, raw
    /// source text in [`AccountSource::fields`].
    pub value: String,
    /// When a linked `value` was verified against the account, in Mastodon
    /// timestamp format; nothing verifies links yet, so this is always
    /// JSON `null`.
    pub verified_at: Option<String>,
}

/// The `pleroma` half of an [`AccountSource`]: the account's own settings,
/// as Pleroma's `maybe_put_settings` renders them beside the base `source`
/// map (`lib/pleroma/web/mastodon_api/views/account_view.ex`).
#[derive(Debug, Serialize)]
pub struct SourcePleroma {
    /// `show_role`: whether the admin/moderator flags may be shown to
    /// others.
    pub show_role: JsonBool,
    /// `no_rich_text`: whether clients should render this account's
    /// content as plain text.
    pub no_rich_text: JsonBool,
    /// `discoverable`: whether the account opted into discovery.
    pub discoverable: JsonBool,
    /// `actor_type`: the `ActivityPub` actor kind (`Person`, `Service`,
    /// `Group`, …) as stored.
    pub actor_type: String,
}

/// The `source` object of an [`Account`]: the profile as its owner edits
/// it, which the public entity renders sanitized or not at all.
///
/// Pleroma builds it in `AccountView.do_render("show.json", …)`
/// (`lib/pleroma/web/mastodon_api/views/account_view.ex`): the base map
/// holds the raw note and fields, and `maybe_put_settings` adds the
/// posting defaults — but only when the viewer is the account itself.
/// That is why [`Account::source`] is an `Option` this entity only fills
/// for `verify_credentials` and `update_credentials`; every other render
/// leaves it `None` and serde omits the key entirely.
#[derive(Debug, Serialize)]
pub struct AccountSource {
    /// `privacy`: the visibility new statuses default to
    /// (`public`, `unlisted`, `private` or `direct`).
    pub privacy: String,
    /// `sensitive`: whether new statuses are flagged sensitive by default.
    pub sensitive: JsonBool,
    /// `language`: the BCP 47 tag new statuses default to, JSON `null`
    /// when the account stated no preference.
    pub language: Option<String>,
    /// `note`: the profile note as its raw source text, not the sanitized
    /// HTML the top-level `note` renders.
    pub note: String,
    /// `fields`: the profile metadata as raw text, in storage order.
    pub fields: Vec<Field>,
    /// `follow_requests_count`: how many follow requests are waiting for
    /// this account to answer them.
    pub follow_requests_count: i32,
    /// Pleroma's extension block of the source object (see
    /// [`SourcePleroma`]).
    pub pleroma: SourcePleroma,
}

impl AccountSource {
    /// Renders the `source` block of `user` for `user` itself, with
    /// `follow_requests_count` as the pending queue
    /// [`pending_count`](crate::models::follow::pending_count) counted.
    ///
    /// Pure (decision 3): everything but that count comes off the row.
    #[must_use]
    pub fn for_owner(user: &User, follow_requests_count: i32) -> Self {
        Self {
            privacy: user.default_scope.clone(),
            sensitive: JsonBool(user.sensitive_default.get()),
            language: user.language.clone(),
            note: user.raw_bio.clone(),
            fields: raw_fields_from_json(&user.fields),
            follow_requests_count,
            pleroma: SourcePleroma {
                show_role: JsonBool(user.show_role.get()),
                no_rich_text: JsonBool(user.no_rich_text.get()),
                discoverable: JsonBool(user.discoverable.get()),
                actor_type: user.actor_type.clone(),
            },
        }
    }
}

/// The account making a request, as [`Account::from_user_for`] takes it:
/// Mastodon's `AccountViewer`. `None` is an anonymous viewer, who sees
/// nothing of another account that the account itself hides.
pub type AccountViewer<'a> = Option<&'a User>;

/// The `pleroma` extension object of an [`Account`]: the Pleroma-specific
/// keys of Pleroma's `show.json` renderer
/// (`lib/pleroma/web/mastodon_api/views/account_view.ex`).
///
/// The seven keys Pleroma only puts in for the account's own user
/// (`unread_conversation_count`, `unread_notifications_count`,
/// `notification_settings`, `settings_store`, `chat_token`, `birthday`,
/// `show_birthday`) are `Option`s that [`Account::from_user_for`] leaves
/// `None` for every other viewer, so serde omits them entirely;
/// `birthday` is the exception that a stranger may also see, when the
/// account sets [`show_birthday`](User::show_birthday).
#[derive(Debug, Serialize)]
pub struct PleromaMetadata {
    /// `background_image`: the profile background URL, or `null` when
    /// unset (unlike the avatar and banner, there is no instance default).
    pub background_image: Option<String>,
    /// `tags`: the profile tags as stored.
    pub tags: Vec<String>,
    /// `relationship`: the viewer's
    /// [`Relationship`](crate::entities::relationship::Relationship) to
    /// this account when one is embedded — an authenticated viewer looking
    /// at someone other than themselves, with the relationship preloaded
    /// in the map (see [`Account::from_user_for`]); `{}` for every other
    /// view, as Pleroma renders an anonymous `relationship.json`.
    pub relationship: Value,
    /// `is_admin`: the admin role, or `false` when hidden
    /// (`visible_roles` decides).
    pub is_admin: JsonBool,
    /// `is_moderator`: the moderator role, or `false` when hidden
    /// (`visible_roles` decides).
    pub is_moderator: JsonBool,
    /// `confirmation_pending`: the inverse of
    /// [`email_confirmed`](User::email_confirmed).
    pub confirmation_pending: JsonBool,
    /// `hide_favorites`: whether the favourites list is private.
    pub hide_favorites: JsonBool,
    /// `hide_followers`: whether the follower collection is hidden.
    pub hide_followers: JsonBool,
    /// `hide_follows`: whether the follow collection is hidden.
    pub hide_follows: JsonBool,
    /// `hide_followers_count`: whether the follower count is hidden.
    pub hide_followers_count: JsonBool,
    /// `hide_follows_count`: whether the follow count is hidden.
    pub hide_follows_count: JsonBool,
    /// `accepts_chat_messages`: the advertised chat capability, `null` for
    /// a remote actor whose row never recorded one.
    pub accepts_chat_messages: Option<bool>,
    /// `favicon`: the icon of the actor's instance; instance favicons are
    /// not collected here, so this is always `null`.
    pub favicon: Option<String>,
    /// `skip_thread_containment`: whether this account's statuses skip it.
    pub skip_thread_containment: JsonBool,
    /// `allow_following_move`: whether a move takes the following list.
    pub allow_following_move: JsonBool,
    /// `deactivated`: the inverse of [`is_active`](User::is_active), which
    /// the top-level `suspended` key carries the same way.
    pub deactivated: JsonBool,
    /// `ap_id`: the `ActivityPub` actor id, the same string as the
    /// top-level `uri`.
    pub ap_id: String,
    /// `unread_conversation_count`: own viewer only; chat is not
    /// implemented here, so it is always `0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_conversation_count: Option<i32>,
    /// `unread_notifications_count`: own viewer only; the notification
    /// markers arrive with a later ticket, so it is always `0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_notifications_count: Option<i32>,
    /// `notification_settings`: own viewer only; `{}` until the
    /// notification-settings ticket stores them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_settings: Option<Value>,
    /// `settings_store`: own viewer only; the stored blob, which the server
    /// only round-trips.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_store: Option<Value>,
    /// `chat_token`: own viewer only; `null`, because chat is not
    /// implemented here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_token: Option<Value>,
    /// `birthday`: own viewer always, any other viewer only when the
    /// account shows it; the stored `YYYY-MM-DD` date, or `null` when the
    /// account gave none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub birthday: Option<Value>,
    /// `show_birthday`: own viewer only; whether strangers may see the
    /// birthday.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_birthday: Option<JsonBool>,
}

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
    /// Custom emojis used in the profile; always the empty array until
    /// the emoji ticket fills it.
    pub emojis: Vec<Value>,
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

impl Account {
    /// Renders `user` as an `Account` of the instance described by
    /// `config`, as an anonymous viewer sees it: what the account hides
    /// stays hidden, the owner-only keys of [`pleroma`](Self::pleroma)
    /// are absent, and `pleroma.relationship` is the empty object (an
    /// anonymous request has no relationship to embed).
    ///
    /// Pure (decision 3): every value comes from the row, from `config`,
    /// or from a documented constant — counts included, which for remote
    /// users are exactly the counters federation stored.
    #[must_use]
    pub fn from_user(user: &User, config: &Config) -> Self {
        Self::from_user_for(None, user, config, None)
    }

    /// Renders `user` as an `Account` of the instance described by
    /// `config`, as seen by `viewer` (an [`AccountViewer`]: `None` for an
    /// anonymous request), with `relationships` as the optional preload
    /// of the viewer's [`Relationship`](crate::entities::relationship::Relationship)
    /// rows keyed by target id (see [`RelationshipMap`]).
    ///
    /// The viewer decides three things, as Pleroma's `show.json` renderer
    /// does: [`followers_count`](Self::followers_count) and
    /// [`following_count`](Self::following_count) render as `0` for anyone
    /// but the owner when the account hides them, the
    /// [`is_admin`](PleromaMetadata::is_admin) and
    /// [`is_moderator`](PleromaMetadata::is_moderator) flags render as
    /// `false` for anyone but the owner when the account hides its role,
    /// and the owner-only keys of [`PleromaMetadata`] appear only for the
    /// owner (`birthday` reaches a stranger too, when the account shows
    /// it). And `pleroma.relationship` embeds the viewer's relationship
    /// to this account — for a viewer looking at someone other than
    /// themselves, when `relationships` carries that account's row;
    /// `{}` otherwise, Pleroma's anonymous rendering.
    #[must_use]
    pub fn from_user_for(
        viewer: AccountViewer<'_>,
        user: &User,
        config: &Config,
        relationships: Option<&RelationshipMap>,
    ) -> Self {
        let is_self = is_owner_of(viewer, user);
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
            followers_count: visible_followers_count(user, is_self),
            following_count: visible_following_count(user, is_self),
            statuses_count: user.statuses_count,
            last_status_at: user
                .last_status_at
                .map(|at| at.format(DATE_FORMAT).to_string()),
            emojis: Vec::new(),
            fields: fields_from_json(&user.fields),
            moved: None,
            also_known_as: None,
            suspended: JsonBool(!user.is_active.get()),
            limited: JsonBool(false),
            source: None,
            pleroma: pleroma_metadata(
                user,
                is_self,
                embedded_relationship(viewer, user, is_self, relationships),
            ),
        }
    }

    /// Renders `user` as its own credentials endpoints answer it —
    /// `verify_credentials` and `update_credentials` — by attaching the
    /// [`source`](Self::source) block to the self view of
    /// [`from_user_for`](Self::from_user_for).
    ///
    /// `follow_requests_count` is the pending queue
    /// [`pending_count`](crate::models::follow::pending_count) counted:
    /// the one part of the block no row carries, so the caller reads it
    /// from the database and hands it in, keeping the entity pure
    /// (decision 3). The viewer is the account itself, so the owner-only
    /// keys of [`pleroma`](Self::pleroma) are in, the hidden counters are
    /// visible and `pleroma.relationship` is Pleroma's empty object.
    #[must_use]
    pub fn for_owner(user: &User, config: &Config, follow_requests_count: i32) -> Self {
        let mut account = Self::from_user_for(Some(user), user, config, None);
        account.source = Some(AccountSource::for_owner(user, follow_requests_count));
        // The own view is the one alias clients read and write: the stored
        // actor ids come out verbatim, as a top-level key (ticket decision:
        // not inside the `pleroma` block).
        account.also_known_as = Some(user.also_known_as.clone());
        account
    }

    /// Attaches the nested [`moved`](Self::moved) entity: renders the move
    /// target `target` with [`Account::from_user`] — the plain, non-owner
    /// view — and stores it as this account's `moved` value. The render is
    /// built by this method and never looks at `target`'s own
    /// `moved_to_ap_id`, so the nesting is one level deep: the nested
    /// account's `moved` is `null` even if that account moved on (Pleroma's
    /// `AccountView` renders `moved` non-recursively for the same reason).
    ///
    /// Resolving `moved_to_ap_id` to a row is the caller's job (it needs
    /// the database); an unresolvable target simply keeps `moved: null`.
    #[must_use]
    pub fn with_moved(mut self, target: &User, config: &Config) -> Self {
        self.moved = serde_json::to_value(Self::from_user(target, config)).ok();
        self
    }
}

/// Whether `viewer` is looking at their own account — Pleroma's
/// `self = opts[:for] == user`, compared by primary key.
fn is_owner_of(viewer: AccountViewer<'_>, user: &User) -> bool {
    viewer.is_some_and(|account| account.id == user.id)
}

/// `followers_count` as a viewer may see it: the stored counter, or `0`
/// for anyone but the owner once the account hides the count (the
/// behaviour Pleroma's renderer gates on `hide_followers_count`).
const fn visible_followers_count(user: &User, is_self: bool) -> i32 {
    if user.hide_followers_count.get() && !is_self {
        0
    } else {
        user.followers_count
    }
}

/// [`visible_followers_count`]'s counterpart for the follow count and
/// `hide_follows_count`.
const fn visible_following_count(user: &User, is_self: bool) -> i32 {
    if user.hide_follows_count.get() && !is_self {
        0
    } else {
        user.following_count
    }
}

/// The `is_admin`/`is_moderator` pair as a viewer may see it: Pleroma
/// leaves both keys out of the renderer entirely when `show_role` is
/// false and the viewer is not the account itself; here the keys are
/// always present and simply render `false`.
const fn visible_roles(user: &User, is_self: bool) -> (bool, bool) {
    if user.show_role.get() || is_self {
        (user.is_admin.get(), user.is_moderator.get())
    } else {
        (false, false)
    }
}

/// The `accepts_chat_messages` value.
///
/// A local account always knows its own setting. A remote mirror keeps the
/// column default (`true`) until federation sees
/// `capabilities.acceptsChatMessages`, and a `NOT NULL` column cannot mark
/// that default as "never advertised", so a remote row still at its
/// default renders JSON `null` — what Pleroma sends for an actor that
/// never advertised the capability — while an advertised `false` renders
/// `false`.
const fn accepts_chat_messages(user: &User) -> Option<bool> {
    let advertised = user.accepts_chat_messages.get();
    match (user.local.get(), advertised) {
        (false, true) => None,
        (_, value) => Some(value),
    }
}

/// The `birthday` value for a viewer allowed to see the key: the stored
/// date in its `YYYY-MM-DD` spelling, or JSON `null` when the account gave
/// none. Whether the key appears at all is the caller's decision.
fn birthday_json(user: &User) -> Value {
    user.birthday.map_or(Value::Null, |day| json!(day))
}

/// The `pleroma.relationship` value: the viewer's embedded
/// [`Relationship`](crate::entities::relationship::Relationship) when
/// there is one to embed — a viewer (anonymous requests never get one),
/// not the account's own row (Pleroma keeps the owner's own relationship
/// out of `show.json`), and an entry for this account in the preloaded
/// `relationships` map — and `{}` in every other case. Serializing the
/// entity cannot fail (its fields are strings and booleans), so the
/// degenerate error branch just renders the same empty object.
fn embedded_relationship(
    viewer: AccountViewer<'_>,
    user: &User,
    is_self: bool,
    relationships: Option<&RelationshipMap>,
) -> Value {
    if viewer.is_none() || is_self {
        return json!({});
    }
    relationships.and_then(|map| map.get(&user.id)).map_or_else(
        || json!({}),
        |relationship| serde_json::to_value(relationship).unwrap_or_else(|_| json!({})),
    )
}

/// Builds the `pleroma` block for `user` as seen by a viewer who is the
/// account (`is_self`) or someone else; `relationship` is the
/// [`embedded_relationship`] value the caller's viewer gets.
fn pleroma_metadata(user: &User, is_self: bool, relationship: Value) -> PleromaMetadata {
    let (is_admin, is_moderator) = visible_roles(user, is_self);
    PleromaMetadata {
        background_image: user.background_image_url.clone(),
        tags: user.tags.clone(),
        relationship,
        is_admin: JsonBool(is_admin),
        is_moderator: JsonBool(is_moderator),
        confirmation_pending: JsonBool(!user.email_confirmed.get()),
        hide_favorites: JsonBool(user.hide_favorites.get()),
        hide_followers: JsonBool(user.hide_followers.get()),
        hide_follows: JsonBool(user.hide_follows.get()),
        hide_followers_count: JsonBool(user.hide_followers_count.get()),
        hide_follows_count: JsonBool(user.hide_follows_count.get()),
        accepts_chat_messages: accepts_chat_messages(user),
        favicon: None,
        skip_thread_containment: JsonBool(user.skip_thread_containment.get()),
        allow_following_move: JsonBool(user.allow_following_move.get()),
        deactivated: JsonBool(!user.is_active.get()),
        ap_id: user.ap_id.clone(),
        // Chat and the notification markers are not implemented here, so
        // the owner sees zeroes and the empty settings placeholder; the
        // tickets that add them fill these in.
        unread_conversation_count: is_self.then_some(0),
        unread_notifications_count: is_self.then_some(0),
        notification_settings: is_self.then(|| json!({})),
        settings_store: is_self.then(|| user.settings_store.clone()),
        chat_token: is_self.then_some(Value::Null),
        birthday: (is_self || user.show_birthday.get()).then(|| birthday_json(user)),
        show_birthday: is_self.then(|| JsonBool(user.show_birthday.get())),
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
/// passes through; a local one gains [`Config::domain`]. Crate-visible
/// because the alias endpoints render stored actor ids back as these
/// handles (see [`crate::api::pleroma_account_settings`]).
#[must_use]
pub(crate) fn full_nickname(user: &User, config: &Config) -> String {
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

/// The key a stored field's raw text value would sit under: Pleroma keeps
/// the editing spelling beside the rendered one (its `raw_fields`, whose
/// entries `put_fields` derives `fields` from), so a field stored with one
/// round-trips verbatim instead of being re-derived from its markup.
const FIELD_RAW_KEY: &str = "value_plaintext";

/// Maps the `fields` jsonb into the [`AccountSource::fields`] an owner
/// edits: the same entries as [`fields_from_json`], with the value as
/// plain text rather than as markup. A field that stores a raw value under
/// [`FIELD_RAW_KEY`] round-trips it verbatim; one that only stores the
/// sanitized HTML (every field this instance has today) is reduced to its
/// text by [`strip_html`], so what a client puts in a textarea is what the
/// account wrote, not the markup around it.
fn raw_fields_from_json(stored: &Value) -> Vec<Field> {
    stored
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let name = entry.get("name").and_then(Value::as_str)?;
                    let raw = entry.get(FIELD_RAW_KEY).and_then(Value::as_str);
                    let html = entry.get("value").and_then(Value::as_str);
                    let value = match (raw, html) {
                        (Some(raw), _) => raw.to_owned(),
                        (None, Some(html)) => strip_html(html),
                        (None, None) => return None,
                    };
                    Some(Field {
                        name: name.to_owned(),
                        value,
                        verified_at: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use uuid::Uuid;

    use super::*;
    use crate::{
        entities::relationship::Relationship,
        models::user::{DEFAULT_SCOPE, Flag},
        util::time::parse_time,
    };

    /// The hyphenated lowercase id every fixture row carries.
    const ACCOUNT_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// A second id, for a fixture account that must not be the same row as
    /// [`ACCOUNT_ID`].
    const OTHER_ACCOUNT_ID: &str = "01934567-89ab-7cde-f012-3456789abcdf";
    /// The instance origin of [`Config::for_tests`].
    const BASE: &str = "https://test.example";
    /// Every JSON key the core entity must emit, sorted.
    const EXPECTED_KEYS: [&str; 27] = [
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
        "pleroma",
        "statuses_count",
        "suspended",
        "uri",
        "url",
        "username",
    ];
    /// The `pleroma` keys a viewer who is not the account sees: without
    /// the seven owner-only keys, and without `birthday` when the account
    /// hides it.
    const PLEROMA_KEYS_FOR_OTHERS: [&str; 17] = [
        "accepts_chat_messages",
        "allow_following_move",
        "ap_id",
        "background_image",
        "confirmation_pending",
        "deactivated",
        "favicon",
        "hide_favorites",
        "hide_followers",
        "hide_followers_count",
        "hide_follows",
        "hide_follows_count",
        "is_admin",
        "is_moderator",
        "relationship",
        "skip_thread_containment",
        "tags",
    ];
    /// Every `pleroma` key that can ever appear — the list above plus the
    /// seven the account sees only for itself — sorted for key-set
    /// assertions.
    const ALL_PLEROMA_KEYS: [&str; 24] = [
        "accepts_chat_messages",
        "allow_following_move",
        "ap_id",
        "background_image",
        "birthday",
        "chat_token",
        "confirmation_pending",
        "deactivated",
        "favicon",
        "hide_favorites",
        "hide_followers",
        "hide_followers_count",
        "hide_follows",
        "hide_follows_count",
        "is_admin",
        "is_moderator",
        "notification_settings",
        "relationship",
        "settings_store",
        "show_birthday",
        "skip_thread_containment",
        "tags",
        "unread_conversation_count",
        "unread_notifications_count",
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
            last_active_at: None,
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

    /// `user` rendered as `viewer` sees it (`None` is anonymous), without
    /// any preloaded relationships.
    fn render_for(viewer: Option<&User>, user: &User, config: &Config) -> Value {
        render_for_with(viewer, user, config, None)
    }

    /// `user` rendered as `viewer` sees it with `relationships` as the
    /// optional preload for the embedded `pleroma.relationship`.
    fn render_for_with(
        viewer: Option<&User>,
        user: &User,
        config: &Config,
        relationships: Option<&RelationshipMap>,
    ) -> Value {
        serde_json::to_value(Account::from_user_for(viewer, user, config, relationships)).unwrap()
    }

    /// The `pleroma` block of a rendered account, owned so a test can bind
    /// it in the same statement it renders in.
    fn pleroma_of(value: &Value) -> Value {
        value
            .get("pleroma")
            .cloned()
            .expect("every account renders pleroma")
    }

    /// The `pleroma` block a row at the migration defaults renders to an
    /// anonymous viewer: the public keys only, with `ap_id` and the chat
    /// capability (`accepts_chat`) as given.
    fn pleroma_block(ap_id: &str, accepts_chat: Option<bool>) -> Value {
        json!({
            "background_image": null,
            "tags": [],
            "relationship": {},
            "is_admin": false,
            "is_moderator": false,
            "confirmation_pending": false,
            "hide_favorites": true,
            "hide_followers": false,
            "hide_follows": false,
            "hide_followers_count": false,
            "hide_follows_count": false,
            "accepts_chat_messages": accepts_chat,
            "favicon": null,
            "skip_thread_containment": false,
            "allow_following_move": true,
            "deactivated": false,
            "ap_id": ap_id,
        })
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

    /// A local `alice` whose Pleroma columns are all set: profile tags, a
    /// background image, a birthday and a stored client-settings blob.
    fn alice_with_pleroma_profile() -> User {
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        alice.local = Flag::new(true);
        alice.tags = vec!["rust".to_owned(), "gnw".to_owned()];
        alice.background_image_url = Some(format!("{BASE}/bg.png"));
        alice.birthday = NaiveDate::from_ymd_opt(1990, 1, 31);
        alice.settings_store = json!({"pleroma_fe": {"theme": "dark"}});
        alice
    }

    /// A second account to act as a viewer who is not the rendered one:
    /// its own primary key, since being the owner is decided by id.
    fn mallory() -> User {
        let mut stranger = base_user(
            "mallory@remote.example",
            "https://remote.example/users/mallory",
        );
        stranger.id = Uuid::parse_str(OTHER_ACCOUNT_ID).unwrap();
        stranger
    }

    #[test]
    fn own_viewer_sees_the_owner_only_pleroma_keys() {
        let config = Config::for_tests();
        let alice = alice_with_pleroma_profile();
        let value = render_for(Some(&alice), &alice, &config);
        assert_eq!(
            json_keys(&pleroma_of(&value)),
            ALL_PLEROMA_KEYS,
            "the account's own user sees every key of the block"
        );
        assert_eq!(
            pleroma_of(&value),
            json!({
                "background_image": format!("{BASE}/bg.png"),
                "tags": ["rust", "gnw"],
                "relationship": {},
                "is_admin": false,
                "is_moderator": false,
                "confirmation_pending": false,
                "hide_favorites": true,
                "hide_followers": false,
                "hide_follows": false,
                "hide_followers_count": false,
                "hide_follows_count": false,
                "accepts_chat_messages": true,
                "favicon": null,
                "skip_thread_containment": false,
                "allow_following_move": true,
                "deactivated": false,
                "ap_id": format!("{BASE}/users/alice"),
                "unread_conversation_count": 0,
                "unread_notifications_count": 0,
                "notification_settings": {},
                "settings_store": {"pleroma_fe": {"theme": "dark"}},
                "chat_token": null,
                "birthday": "1990-01-31",
                "show_birthday": false,
            }),
            "the owner's pleroma block: chat and notification counters are \
             zeroes and empty placeholders until their tickets land, and \
             the chat token is null"
        );
    }

    #[test]
    fn another_viewer_sees_only_the_public_pleroma_keys() {
        let config = Config::for_tests();
        let alice = alice_with_pleroma_profile();
        let stranger = mallory();
        for who in [None, Some(&stranger)] {
            let value = render_for(who, &alice, &config);
            let pleroma = pleroma_of(&value);
            assert_eq!(
                json_keys(&pleroma),
                PLEROMA_KEYS_FOR_OTHERS,
                "the public view of the block is the same anonymously and \
                 for another account"
            );
        }
        let pleroma = pleroma_of(&render_for(Some(&stranger), &alice, &config));
        for hidden in [
            "unread_conversation_count",
            "unread_notifications_count",
            "notification_settings",
            "settings_store",
            "chat_token",
            "show_birthday",
        ] {
            assert!(
                pleroma.get(hidden).is_none(),
                "`{hidden}` is only ever rendered for the account itself"
            );
        }
    }

    #[test]
    fn hidden_follow_counts_are_zero_for_everyone_but_the_owner() {
        let config = Config::for_tests();
        let mut alice = alice_with_pleroma_profile();
        alice.hide_followers_count = Flag::new(true);
        alice.hide_follows_count = Flag::new(true);
        alice.followers_count = 5;
        alice.following_count = 6;
        let stranger = mallory();

        for who in [None, Some(&stranger)] {
            let value = render_for(who, &alice, &config);
            assert_eq!(
                value["followers_count"], 0,
                "a hidden follower count renders as zero"
            );
            assert_eq!(
                value["following_count"], 0,
                "a hidden follow count renders as zero"
            );
            assert_eq!(
                pleroma_of(&value)["hide_followers_count"],
                true,
                "the flag itself is public: clients learn what is hidden"
            );
        }

        let own = render_for(Some(&alice), &alice, &config);
        assert_eq!(
            own["followers_count"], 5,
            "the owner always sees the stored counters"
        );
        assert_eq!(
            own["following_count"], 6,
            "the owner always sees the stored counters"
        );
    }

    #[test]
    fn hiding_a_collection_without_its_count_keeps_the_counter() {
        let config = Config::for_tests();
        let mut alice = alice_with_pleroma_profile();
        alice.hide_followers = Flag::new(true);
        alice.hide_follows = Flag::new(true);
        alice.followers_count = 5;
        alice.following_count = 6;
        let stranger = mallory();

        let value = render_for(Some(&stranger), &alice, &config);
        assert_eq!(
            value["followers_count"], 5,
            "hide_followers hides the collection, not the counter"
        );
        assert_eq!(
            value["following_count"], 6,
            "hide_follows hides the collection, not the counter"
        );
        assert_eq!(
            pleroma_of(&value)["hide_followers"],
            true,
            "both flags render as stored"
        );
    }

    #[test]
    fn role_flags_are_hidden_when_the_account_hides_its_role() {
        let config = Config::for_tests();
        let mut alice = alice_with_pleroma_profile();
        alice.is_admin = Flag::new(true);
        alice.is_moderator = Flag::new(true);
        alice.show_role = Flag::new(false);
        let stranger = mallory();

        for who in [None, Some(&stranger)] {
            let pleroma = pleroma_of(&render_for(who, &alice, &config));
            assert_eq!(
                pleroma["is_admin"], false,
                "show_role: false keeps the admin role from strangers"
            );
            assert_eq!(
                pleroma["is_moderator"], false,
                "show_role: false keeps the moderator role from strangers"
            );
        }

        let own = pleroma_of(&render_for(Some(&alice), &alice, &config));
        assert_eq!(
            own["is_admin"], true,
            "the account itself always sees its roles"
        );
        assert_eq!(
            own["is_moderator"], true,
            "the account itself always sees its roles"
        );

        alice.show_role = Flag::new(true);
        let shown = pleroma_of(&render_for(Some(&stranger), &alice, &config));
        assert_eq!(
            shown["is_admin"], true,
            "with the default show_role the public view carries the roles"
        );
        assert_eq!(shown["is_moderator"], true, "and the moderator flag");
    }

    #[test]
    fn confirmation_pending_and_deactivated_are_the_inverted_columns() {
        let config = Config::for_tests();
        let mut alice = alice_with_pleroma_profile();
        assert_eq!(
            pleroma_of(&render(&alice, &config))["confirmation_pending"],
            false,
            "a confirmed email is not a pending confirmation"
        );

        alice.email_confirmed = Flag::new(false);
        alice.is_active = Flag::new(false);
        let pleroma = pleroma_of(&render(&alice, &config));
        assert_eq!(
            pleroma["confirmation_pending"], true,
            "an unconfirmed email renders confirmation_pending"
        );
        assert_eq!(
            pleroma["deactivated"], true,
            "an inactive account renders deactivated, as the top-level \
             suspended renders true"
        );
    }

    #[test]
    fn accepts_chat_messages_is_null_for_a_remote_actor_that_never_advertised_it() {
        let config = Config::for_tests();

        // A mirror federation never saw the capability for keeps the
        // column default, which there is no way to mark as "not
        // advertised", so it renders as null.
        let never = base_user("bob@remote.example", "https://remote.example/users/bob");
        assert_eq!(
            pleroma_of(&render(&never, &config))["accepts_chat_messages"],
            Value::Null,
            "a remote actor at the default renders null"
        );

        let mut declined = base_user("bob@remote.example", "https://remote.example/users/bob");
        declined.accepts_chat_messages = Flag::new(false);
        assert_eq!(
            pleroma_of(&render(&declined, &config))["accepts_chat_messages"],
            false,
            "a remote actor that advertised false renders false"
        );

        let mut local = alice_with_pleroma_profile();
        assert_eq!(
            pleroma_of(&render(&local, &config))["accepts_chat_messages"],
            true,
            "a local account always answers for itself"
        );
        local.accepts_chat_messages = Flag::new(false);
        assert_eq!(
            pleroma_of(&render(&local, &config))["accepts_chat_messages"],
            false,
            "and so does one that opted out"
        );
    }

    #[test]
    fn birthday_reaches_strangers_only_when_the_account_shows_it() {
        let config = Config::for_tests();
        let alice = alice_with_pleroma_profile();
        let stranger = mallory();

        for who in [None, Some(&stranger)] {
            assert!(
                pleroma_of(&render_for(who, &alice, &config))
                    .get("birthday")
                    .is_none(),
                "show_birthday: false leaves the key out for others"
            );
        }

        let own = pleroma_of(&render_for(Some(&alice), &alice, &config));
        assert_eq!(
            own["birthday"], "1990-01-31",
            "the account itself always sees its birthday"
        );
        assert_eq!(
            own["show_birthday"], false,
            "and sees the switch that controls the others"
        );

        let mut sharing = alice;
        sharing.show_birthday = Flag::new(true);
        let other = pleroma_of(&render_for(Some(&stranger), &sharing, &config));
        assert_eq!(
            other["birthday"], "1990-01-31",
            "show_birthday: true makes the date public"
        );
        assert!(
            other.get("show_birthday").is_none(),
            "the switch itself stays owner-only"
        );

        let mut blank = mallory();
        blank.local = Flag::new(true);
        blank.show_birthday = Flag::new(true);
        let own_blank = pleroma_of(&render_for(Some(&blank), &blank, &config));
        assert_eq!(
            own_blank["birthday"],
            Value::Null,
            "an account without a birthday renders the key as null for itself"
        );
    }

    #[test]
    fn unset_profile_columns_render_their_empty_shapes() {
        let config = Config::for_tests();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        alice.local = Flag::new(true);
        let pleroma = pleroma_of(&render(&alice, &config));
        assert_eq!(
            pleroma["background_image"],
            Value::Null,
            "no background image is null, not the instance banner default"
        );
        assert_eq!(pleroma["tags"], json!([]), "no tags is the empty array");
        assert_eq!(
            pleroma["relationship"],
            json!({}),
            "with no viewer and no preload there is no relationship to embed"
        );
        assert_eq!(
            pleroma["favicon"],
            Value::Null,
            "instance favicons are not collected here"
        );
        assert_eq!(
            pleroma["ap_id"],
            format!("{BASE}/users/alice"),
            "ap_id repeats the actor id the top-level uri carries"
        );

        alice.tags = vec!["one".to_owned()];
        alice.background_image_url = Some(format!("{BASE}/bg.png"));
        let filled = pleroma_of(&render(&alice, &config));
        assert_eq!(filled["tags"], json!(["one"]), "tags render as stored");
        assert_eq!(
            filled["background_image"],
            format!("{BASE}/bg.png"),
            "the stored background url renders as itself"
        );
    }

    /// A loaded relationship for `target`: the follow-derived flags a
    /// viewer who follows them with notifications on and reblogs hidden
    /// would have; the rest at the constructor's defaults.
    fn following_relationship(target: &User) -> Relationship {
        Relationship {
            following: JsonBool(true),
            notifying: JsonBool(true),
            subscribing: JsonBool(true),
            showing_reblogs: JsonBool(false),
            ..Relationship::unconnected(target.id)
        }
    }

    #[test]
    fn a_stranger_viewer_gets_the_relationship_embedded_in_pleroma() {
        let config = Config::for_tests();
        let alice = alice_with_pleroma_profile();
        let stranger = mallory();
        let relationship = following_relationship(&alice);
        let map = RelationshipMap::from([(alice.id, relationship.clone())]);

        let pleroma = pleroma_of(&render_for_with(
            Some(&stranger),
            &alice,
            &config,
            Some(&map),
        ));
        assert_eq!(
            pleroma["relationship"],
            serde_json::to_value(&relationship).unwrap(),
            "the preloaded relationship for the rendered account is embedded \
             verbatim, keys and all"
        );
        assert_eq!(
            json_keys(&pleroma),
            PLEROMA_KEYS_FOR_OTHERS,
            "embedding changes the value of `relationship`, not the key set"
        );
    }

    #[test]
    fn the_relationship_stays_empty_without_a_stranger_or_a_preload() {
        let config = Config::for_tests();
        let alice = alice_with_pleroma_profile();
        let stranger = mallory();
        let relationship = following_relationship(&alice);
        let map = RelationshipMap::from([(alice.id, relationship)]);

        // An anonymous viewer never gets one, however full the preload is.
        let anonymous = pleroma_of(&render_for_with(None, &alice, &config, Some(&map)));
        assert_eq!(
            anonymous["relationship"],
            json!({}),
            "rendering an anonymous `relationship.json` is Pleroma's empty object"
        );
        // The account's own viewer does not either: it is no one's target here.
        let own = pleroma_of(&render_for_with(Some(&alice), &alice, &config, Some(&map)));
        assert_eq!(
            own["relationship"],
            json!({}),
            "a self relationship is not embedded"
        );
        // A viewer without the preload (or without this account in it)
        // falls back to the placeholder, as every other caller renders.
        for who in [
            render_for(Some(&stranger), &alice, &config),
            render_for_with(
                Some(&stranger),
                &alice,
                &config,
                Some(&RelationshipMap::new()),
            ),
        ] {
            assert_eq!(
                pleroma_of(&who)["relationship"],
                json!({}),
                "no preloaded row for the target means the empty object"
            );
        }
        // A preload that only knows about someone else embeds nothing.
        let other = RelationshipMap::from([(mallory().id, following_relationship(&mallory()))]);
        let unrelated = pleroma_of(&render_for_with(
            Some(&stranger),
            &alice,
            &config,
            Some(&other),
        ));
        assert_eq!(
            unrelated["relationship"],
            json!({}),
            "the map is keyed by the rendered account, not any account"
        );
    }

    /// `user` rendered by [`Account::for_owner`] — the
    /// `verify_credentials` view — with `requests` pending follow requests.
    fn render_for_owner(user: &User, config: &Config, requests: i32) -> Value {
        serde_json::to_value(Account::for_owner(user, config, requests)).unwrap()
    }

    /// The `source` block of a rendered account, owned so a test can bind
    /// it in the same statement it renders in.
    fn source_of(value: &Value) -> Value {
        value
            .get("source")
            .cloned()
            .expect("source block is rendered")
    }

    /// The key set of a value, for the presence assertions below.
    fn has_key(value: &Value, key: &str) -> bool {
        value.get(key).is_some()
    }

    #[test]
    fn no_render_but_the_owner_s_gets_a_source_block() {
        let config = Config::for_tests();
        let alice = alice_with_pleroma_profile();
        let stranger = mallory();
        for value in [
            render(&alice, &config),
            render_for(Some(&stranger), &alice, &config),
            render_for(Some(&alice), &alice, &config),
        ] {
            assert!(
                !has_key(&value, "source"),
                "`skip_serializing_if` keeps the editing block out of a \
                 render that did not ask for it"
            );
            assert!(
                !has_key(&value, "also_known_as"),
                "the top-level alias list belongs to the owner view only"
            );
        }
        let own = render_for_owner(&alice, &config, 0);
        assert!(
            has_key(&own, "source"),
            "the owner render is the one that carries it"
        );
        let mut keys = EXPECTED_KEYS.to_vec();
        keys.push("source");
        keys.push("also_known_as");
        keys.sort_unstable();
        assert_eq!(
            json_keys(&own),
            keys,
            "source and also_known_as are the keys only the owner render adds"
        );
    }

    #[test]
    fn the_owner_view_renders_the_stored_aliases_verbatim() {
        let config = Config::for_tests();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        alice.local = Flag::new(true);
        assert_eq!(
            render_for_owner(&alice, &config, 0)["also_known_as"],
            json!([]),
            "an account with no aliases renders the empty array, not a \
             missing key"
        );

        alice.also_known_as = vec![
            format!("{BASE}/users/alicia"),
            "https://old.example/users/alice".to_owned(),
        ];
        assert_eq!(
            render_for_owner(&alice, &config, 0)["also_known_as"],
            json!([
                format!("{BASE}/users/alicia"),
                "https://old.example/users/alice",
            ]),
            "the owner view carries the stored actor ids, in order"
        );
    }

    #[test]
    fn with_moved_nests_a_plain_non_recursive_account_render() {
        let config = Config::for_tests();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        alice.local = Flag::new(true);
        alice.moved_to_ap_id = Some("https://new.example/users/alice".to_owned());
        let target = base_user("alice@new.example", "https://new.example/users/alice");

        // The caller decides whether the account moved: attaching is what
        // renders the key. Without it the entity still answers moved: null.
        let bare = serde_json::to_value(Account::from_user(&alice, &config)).unwrap();
        assert_eq!(bare["moved"], Value::Null, "from_user never attaches it");

        let moved =
            serde_json::to_value(Account::from_user(&alice, &config).with_moved(&target, &config))
                .unwrap();
        assert_eq!(
            moved["moved"]["id"],
            serde_json::json!(target.id.to_string()),
            "the nested entity is the target's own account"
        );
        assert_eq!(
            moved["moved"]["acct"],
            serde_json::json!("alice@new.example"),
            "rendered with the target's handle"
        );
        assert_eq!(
            moved["moved"]["moved"],
            Value::Null,
            "the nesting is one level deep: the target's own move (if any) \
             is not followed"
        );
        assert!(
            moved["moved"].get("also_known_as").is_none(),
            "the nested render is the plain viewer's, not an owner view"
        );
    }

    #[test]
    fn the_source_block_at_the_column_defaults_is_the_documented_shape() {
        let config = Config::for_tests();
        let alice = base_user("alice", &format!("{BASE}/users/alice"));
        assert_eq!(
            source_of(&render_for_owner(&alice, &config, 0)),
            json!({
                "privacy": "public",
                "sensitive": false,
                "language": null,
                "note": "",
                "fields": [],
                "follow_requests_count": 0,
                "pleroma": {
                    "show_role": true,
                    "no_rich_text": false,
                    "discoverable": false,
                    "actor_type": "Person",
                },
            }),
            "a row nobody has edited answers the defaults of the columns"
        );
    }

    #[test]
    fn the_source_block_reads_the_account_s_own_settings() {
        let config = Config::for_tests();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        alice.default_scope = "unlisted".to_owned();
        alice.sensitive_default = Flag::new(true);
        alice.language = Some("eo".to_owned());
        alice.raw_bio = "hi **there**".to_owned();
        alice.bio = "<p>hi <strong>there</strong></p>".to_owned();
        alice.no_rich_text = Flag::new(true);
        alice.show_role = Flag::new(false);
        alice.discoverable = Flag::new(true);
        alice.actor_type = "Service".to_owned();
        let value = render_for_owner(&alice, &config, 4);
        assert_eq!(
            source_of(&value),
            json!({
                "privacy": "unlisted",
                "sensitive": true,
                "language": "eo",
                "note": "hi **there**",
                "fields": [],
                "follow_requests_count": 4,
                "pleroma": {
                    "show_role": false,
                    "no_rich_text": true,
                    "discoverable": true,
                    "actor_type": "Service",
                },
            }),
            "the block renders the stored settings, the raw note rather \
             than the sanitized one, and the count the caller passed in"
        );
        assert_eq!(
            value["note"], "<p>hi <strong>there</strong></p>",
            "the top-level note keeps rendering the sanitized HTML"
        );
    }

    #[test]
    fn source_fields_are_the_raw_text_of_the_stored_ones() {
        let config = Config::for_tests();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        alice.fields = json!([
            {"name": "site", "value": "<a href=\"https://alice.example\">alice.example</a>"},
            {"name": "raw", "value": "<b>kept as stored</b>", "value_plaintext": "**kept as stored**"},
            {"name": "no value"},
            {"name": 3, "value": "four"},
            "not an object",
        ]);
        let fields = source_of(&render_for_owner(&alice, &config, 0))["fields"].clone();
        assert_eq!(
            fields,
            json!([
                {"name": "site", "value": "alice.example", "verified_at": null},
                {"name": "raw", "value": "**kept as stored**", "verified_at": null},
            ]),
            "an HTML-only field is reduced to its text, one stored with a \
             plaintext value round-trips it verbatim, and malformed \
             entries are skipped like the top-level render skips them"
        );

        alice.fields = json!({"not": "an array"});
        assert_eq!(
            source_of(&render_for_owner(&alice, &config, 0))["fields"],
            json!([]),
            "jsonb of another shape renders as the empty list"
        );
    }

    #[test]
    fn the_owner_render_keeps_everything_the_self_view_shows() {
        let config = Config::for_tests();
        let mut alice = alice_with_pleroma_profile();
        alice.followers_count = 5;
        alice.hide_followers_count = Flag::new(true);
        alice.is_admin = Flag::new(true);
        alice.show_role = Flag::new(false);
        let own = render_for_owner(&alice, &config, 1);
        let self_view = render_for(Some(&alice), &alice, &config);
        assert_eq!(
            own["followers_count"], 5,
            "a count hidden from strangers is visible to the owner here too"
        );
        assert_eq!(
            own["pleroma"], self_view["pleroma"],
            "the pleroma block of the owner render is the self view's, \
             owner-only keys and all"
        );
        assert_eq!(
            own["id"], self_view["id"],
            "and the core entity is the same account either way"
        );
    }
}
