//! The `pleroma` extension block of an [`Account`]: the keys Pleroma's
//! `show.json` renderer adds, and the one render that fills them.
//!
//! [`PleromaMetadata`] declares the keys — the stored privacy and state
//! flags, the two role flags and the seven keys only the account's own
//! viewer receives; [`pleroma_metadata`] answers them for a viewer who is
//! the account (`is_self`) or someone else, [`visible_roles`] and
//! [`accepts_chat_messages`] and [`birthday_json`] are the column rules
//! behind three of those keys, and [`embedded_relationship`] picks the
//! `relationship` value.

use serde::Serialize;
use serde_json::{Value, json};

#[cfg(doc)]
use super::Account;

use super::{AccountViewer, JsonBool};
use crate::{
    entities::relationship::RelationshipMap,
    models::user::{User, notification_settings::NotificationSettings},
};

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
    /// `notification_settings`: own viewer only; the account's flags as
    /// the canonical object `{"block_from_strangers": …,
    /// "hide_notification_contents": …}` (Pleroma's
    /// `maybe_put_notification_settings`, which renders the whole
    /// `User.NotificationSetting` for the owner).
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
pub(super) fn embedded_relationship(
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
pub(super) fn pleroma_metadata(user: &User, is_self: bool, relationship: Value) -> PleromaMetadata {
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
        // the owner sees zeroes and the chat token is the empty
        // placeholders those tickets leave; the notification flags are
        // the stored column, rendered in their canonical shape.
        unread_conversation_count: is_self.then_some(0),
        unread_notifications_count: is_self.then_some(0),
        notification_settings: is_self.then(|| NotificationSettings::for_user(user).to_value()),
        settings_store: is_self.then(|| user.settings_store.clone()),
        chat_token: is_self.then_some(Value::Null),
        birthday: (is_self || user.show_birthday.get()).then(|| birthday_json(user)),
        show_birthday: is_self.then(|| JsonBool(user.show_birthday.get())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, entities::account::test_support::*, models::user::Flag};

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
                "notification_settings": {
                    "block_from_strangers": true,
                    "hide_notification_contents": false,
                },
                "settings_store": {"pleroma_fe": {"theme": "dark"}},
                "chat_token": null,
                "birthday": "1990-01-31",
                "show_birthday": false,
            }),
            "the owner's pleroma block: chat and notification counters are \
             zeroes until their tickets land, the chat token is null, and \
             the notification flags render the stored column in its \
             canonical shape"
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
}
