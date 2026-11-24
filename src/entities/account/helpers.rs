//! The helpers the renders share: the two viewer-privacy rules, the URL
//! and handle builders and the profile-metadata mapping.
//!
//! Nothing here builds an entity: each function answers one stored value,
//! one URL or one list of [`Field`]s, and the renders in `entity_render`
//! and `pleroma` reach into this module for them instead of duplicating a
//! rule.

use serde_json::Value;

#[cfg(doc)]
use super::AccountSource;

use super::{AccountViewer, Field};
use crate::{config::Config, models::user::User, services::media_proxy};

/// Whether `viewer` is looking at their own account — Pleroma's
/// `self = opts[:for] == user`, compared by primary key.
pub(super) fn is_owner_of(viewer: AccountViewer<'_>, user: &User) -> bool {
    viewer.is_some_and(|account| account.id == user.id)
}

/// `followers_count` as a viewer may see it: the stored counter, or `0`
/// for anyone but the owner once the account hides the count (the
/// behaviour Pleroma's renderer gates on `hide_followers_count`).
pub(super) const fn visible_followers_count(user: &User, is_self: bool) -> i32 {
    if user.hide_followers_count.get() && !is_self {
        0
    } else {
        user.followers_count
    }
}

/// [`visible_followers_count`]'s counterpart for the follow count and
/// `hide_follows_count`.
pub(super) const fn visible_following_count(user: &User, is_self: bool) -> i32 {
    if user.hide_follows_count.get() && !is_self {
        0
    } else {
        user.following_count
    }
}

/// The avatar/banner URL a remote user's profile renders with: the stored
/// `stored_url`, or `{frontend_base_url}{default_path}` when unset, then
/// through the media proxy helper — remote users only (ticket), so a local
/// user's images pass through untouched. The default assets are shipped by
/// the frontend, so the fallback hangs off [`Config::frontend_base_url`],
/// not the API's own origin.
///
/// Crate-visible because the admin API's account entity renders the same
/// `avatar` Pleroma's `AdminAPI.AccountView` puts through
/// `User.avatar_url/1 |> MediaProxy.url/1` (see
/// [`AdminAccount`](crate::entities::admin_account::AdminAccount)).
pub(crate) fn profile_image(
    user: &User,
    config: &Config,
    stored_url: Option<&str>,
    default_path: &str,
) -> String {
    let fallback = format!("{}{default_path}", config.frontend_base_url);
    let url = stored_url.unwrap_or(&fallback);
    if user.local.get() {
        url.to_owned()
    } else {
        media_proxy::proxy_url(config, url)
    }
}

/// The part of `nickname` before the first `@` — Pleroma's
/// `username_from_nickname`.
pub(crate) fn nickname_part(nickname: &str) -> &str {
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

/// Maps a stored profile-metadata jsonb — an array of `{name, value}`
/// objects — into entity [`Field`]s, used for both columns: the `fields`
/// jsonb whose values are sanitized HTML renders the top-level entity, and
/// the `raw_fields` jsonb whose values are the editing text renders
/// [`AccountSource::fields`]. Entries without a string `name` and `value`
/// are skipped, and stored jsonb of any other shape renders as the empty
/// list, so malformed storage never fails a response.
pub(super) fn fields_from_json(stored: &Value) -> Vec<Field> {
    stored.as_array().map_or_default(|entries| {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        entities::account::{DEFAULT_AVATAR_PATH, DEFAULT_BANNER_PATH, test_support::*},
        models::user::Flag,
    };

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

    /// The unset avatar/banner fall back to the paths under
    /// `FRONTEND_BASE_URL` when that origin differs from `BASE_URL`, and
    /// stay where they were on the default config, where the variable is
    /// unset and the two origins are equal.
    #[test]
    fn default_images_follow_the_frontend_origin() {
        let mut config = Config::for_tests();
        config.frontend_base_url = "https://fe.example".to_owned();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        alice.local = Flag::new(true);
        let value = render(&alice, &config);
        for (key, path) in [
            ("avatar", DEFAULT_AVATAR_PATH),
            ("avatar_static", DEFAULT_AVATAR_PATH),
            ("header", DEFAULT_BANNER_PATH),
            ("header_static", DEFAULT_BANNER_PATH),
        ] {
            let url = value[key].as_str().unwrap();
            assert!(
                url.starts_with("https://fe.example") && url.ends_with(path),
                "{key} falls back to {path} under the frontend origin: {url}"
            );
        }

        let defaults = render(&alice, &Config::for_tests());
        assert_eq!(
            defaults["avatar"],
            serde_json::json!(format!("{BASE}{DEFAULT_AVATAR_PATH}")),
            "with FRONTEND_BASE_URL unset the avatar URL is unchanged"
        );
        assert_eq!(
            defaults["header"],
            serde_json::json!(format!("{BASE}{DEFAULT_BANNER_PATH}")),
            "with FRONTEND_BASE_URL unset the header URL is unchanged"
        );
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
    fn remote_user_images_are_proxied_when_the_media_proxy_is_enabled() {
        let config = proxy_config();
        let value = render(&remote_user_with_images(), &config);
        for (key, suffix) in [
            ("avatar", "/avatar.png"),
            ("avatar_static", "/avatar.png"),
            ("header", "/banner.gif"),
            ("header_static", "/banner.gif"),
        ] {
            let url = value[key].as_str().unwrap();
            assert!(
                url.starts_with("https://test.example/proxy/") && url.ends_with(suffix),
                "{key} becomes a signed proxy link with its filename: {url}"
            );
        }
        let link = value["avatar"].as_str().unwrap();
        let (_, tail) = link.split_once("/proxy/").unwrap();
        let mut segments = tail.split('/');
        let (sig, encoded) = (segments.next().unwrap(), segments.next().unwrap());
        assert_eq!(
            media_proxy::decode(&config, sig, encoded).unwrap(),
            "https://remote.example/avatar.png",
            "the proxied avatar decodes back to the stored url"
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
    fn local_images_and_instance_defaults_pass_through_when_enabled() {
        let config = proxy_config();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        alice.local = Flag::new(true);
        alice.avatar_url = Some("https://remote.example/bought.png".to_owned());
        let local = render(&alice, &config);
        assert_eq!(
            local["avatar"], "https://remote.example/bought.png",
            "only remote users' images are proxied"
        );
        assert_eq!(
            local["header"],
            serde_json::json!(format!("{BASE}/images/banner.png")),
            "the unset banner is the instance default on the instance's own host"
        );

        let disabled = render(&remote_user_with_images(), &Config::for_tests());
        assert_eq!(
            disabled["avatar"], "https://remote.example/avatar.png",
            "a disabled proxy leaves a remote user's images as stored"
        );

        let mut carol = base_user("carol@remote.example", "https://remote.example/users/carol");
        carol.domain = Some("remote.example".to_owned());
        let defaults = render(&carol, &config);
        assert_eq!(
            defaults["avatar"],
            serde_json::json!(format!("{BASE}/images/avi.png")),
            "a remote user without a stored avatar keeps the instance default, proxied or not"
        );
    }
}
