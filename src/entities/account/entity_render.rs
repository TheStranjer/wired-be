//! The `impl` block of [`Account`]: the pure renders that fill it.
//!
//! [`Account::from_user`] and [`Account::from_user_for`] build the entity
//! from a [`User`] row and the instance [`Config`] — the viewer deciding
//! the hidden counters, the hidden roles, the owner-only keys and the
//! embedded relationship through the rules in `helpers` and
//! [`pleroma_metadata`]; [`Account::for_owner`] is the
//! `verify_credentials` render, adding the [`AccountSource`] block; and
//! [`Account::with_moved`] and [`Account::with_mute_expires_at`] attach the
//! two keys no plain render answers. Nothing here reads a database
//! (decision 3): the follow-request count, the move target and the mute
//! expiry all arrive as arguments the caller has read.
//!
//! [`pleroma_metadata`]: super::pleroma::pleroma_metadata

use chrono::{DateTime, Utc};

#[cfg(doc)]
use super::PleromaMetadata;

use super::{
    Account, AccountSource, AccountViewer, DATE_FORMAT, DEFAULT_AVATAR_PATH, DEFAULT_BANNER_PATH,
    GROUP_ACTOR_TYPE, JsonBool, MuteExpiresAt,
    helpers::{
        fields_from_json, full_nickname, is_owner_of, nickname_part, profile_image,
        visible_followers_count, visible_following_count,
    },
    pleroma::{embedded_relationship, pleroma_metadata},
};
use crate::{config::Config, entities::relationship::RelationshipMap, models::user::User};

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
        let avatar = profile_image(
            user,
            config,
            user.avatar_url.as_deref(),
            DEFAULT_AVATAR_PATH,
        );
        let header = profile_image(
            user,
            config,
            user.banner_url.as_deref(),
            DEFAULT_BANNER_PATH,
        );
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
            mute_expires_at: None,
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

    /// Attaches the mutes listing's [`mute_expires_at`](Self::mute_expires_at)
    /// key: `Some` renders the instant in the Mastodon time format, `None`
    /// (an indefinite mute) renders JSON `null`, and either way the key is
    /// there — which is what makes this the only place an ordinary [`Account`]
    /// render gains it, and why [`Account::from_user_for`] leaves the field
    /// absent. Called by `GET /api/v1/mutes` for every entry, whose [`Mute`]
    /// row the listing paged over carries the expiry.
    ///
    /// [`Mute`]: crate::models::mute::Mute
    #[must_use]
    pub fn with_mute_expires_at(mut self, expires_at: Option<DateTime<Utc>>) -> Self {
        self.mute_expires_at = Some(MuteExpiresAt::from_expiry(expires_at));
        self
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{entities::account::test_support::*, models::user::Flag};

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

    /// The mutes listing's extra key: absent from an ordinary render (so every
    /// other endpoint's shape is untouched, key set included), and present on
    /// an entry the listing built — `null` for an indefinite mute, the Mastodon
    /// time for a timed one.
    #[test]
    fn mute_expires_at_appears_only_on_the_listing_render() {
        let config = Config::for_tests();
        let alice = base_user("alice", &format!("{BASE}/users/alice"));

        let plain = serde_json::to_value(Account::from_user(&alice, &config)).unwrap();
        assert!(
            plain.get("mute_expires_at").is_none(),
            "a plain render does not carry the key at all"
        );
        assert_eq!(
            json_keys(&plain),
            EXPECTED_KEYS,
            "which is why every account's key set is unchanged"
        );

        let indefinite =
            serde_json::to_value(Account::from_user(&alice, &config).with_mute_expires_at(None))
                .unwrap();
        assert_eq!(
            indefinite["mute_expires_at"],
            Value::Null,
            "an indefinite mute sends the key as null"
        );
        let mut with_key = json_keys(&plain);
        with_key.push("mute_expires_at");
        with_key.sort_unstable();
        assert_eq!(
            json_keys(&indefinite),
            with_key,
            "and is otherwise the same entity"
        );

        let timed = serde_json::to_value(
            Account::from_user(&alice, &config).with_mute_expires_at(Some(stamped())),
        )
        .unwrap();
        assert_eq!(
            timed["mute_expires_at"], "2024-01-31T12:34:56.789Z",
            "a timed mute sends the expiry in the Mastodon format"
        );
    }
}
