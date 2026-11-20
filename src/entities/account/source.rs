//! The `source` object of an [`Account`] — the profile as its owner edits
//! it — with its Pleroma half, plus the two types beside it: the
//! [`AccountViewer`] an [`Account::from_user_for`] render is answered for,
//! and the [`MuteExpiresAt`] value of the mutes listing's one extra key.
//!
//! [`SourcePleroma`] holds the settings Pleroma's `maybe_put_settings`
//! puts beside the base map, and [`AccountSource::for_owner`] renders the
//! whole block off the row; no other viewer sees a `source` key at all.

use chrono::{DateTime, Utc};
use serde::Serialize;

#[cfg(doc)]
use super::Account;

use super::{Field, JsonBool, helpers::fields_from_json};
use crate::{models::user::User, util::time::mastodon_time};

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
            fields: fields_from_json(&user.raw_fields),
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

/// The value of an [`Account`]'s `mute_expires_at` key (decision 5's Mastodon
/// time format for the instant, JSON `null` for a mute that never expires).
///
/// `GET /api/v1/mutes` sends the key on every entry, timed mutes and
/// indefinite ones alike, so the listing's render has to tell "no expiry"
/// (present, `null`) apart from "this is not the mutes listing" (absent) —
/// one `Option` cannot carry both answers, which is why presence lives on
/// [`Account::mute_expires_at`] and the value here. `#[serde(transparent)]`
/// writes the inner `Option<String>` out as it is: its text or `null`.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct MuteExpiresAt(
    /// The mute's expiry as Mastodon time text, `None` for an indefinite mute
    /// (which renders JSON `null`).
    pub Option<String>,
);

impl MuteExpiresAt {
    /// The key of an entry whose mute expires at `expires_at`: the instant in
    /// the Mastodon time format, and JSON `null` when the mute has no expiry
    /// — either way the key itself is present, as the mutes listing sends it.
    #[must_use]
    pub fn from_expiry(expires_at: Option<DateTime<Utc>>) -> Self {
        Self(expires_at.map(|at| mastodon_time(&at)))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{config::Config, entities::account::test_support::*, models::user::Flag};

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
    fn source_fields_are_the_raw_text_stored_beside_the_rendered_ones() {
        let config = Config::for_tests();
        let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
        alice.fields = json!([
            {"name": "site", "value": "<a href=\"https://alice.example\">alice.example</a>"},
            {"name": "motto", "value": "<b>beep</b>"},
        ]);
        alice.raw_fields = json!([
            {"name": "site", "value": "https://alice.example"},
            {"name": "motto", "value": "<b>beep</b>"},
            {"name": "no value"},
            {"name": 3, "value": "four"},
            "not an object",
        ]);
        let value = render_for_owner(&alice, &config, 0);
        assert_eq!(
            value["fields"],
            json!([
                {"name": "site", "value": "<a href=\"https://alice.example\">alice.example</a>", "verified_at": null},
                {"name": "motto", "value": "<b>beep</b>", "verified_at": null},
            ]),
            "the public entity renders the `fields` jsonb exactly as stored"
        );
        let fields = source_of(&value)["fields"].clone();
        assert_eq!(
            fields,
            json!([
                {"name": "site", "value": "https://alice.example", "verified_at": null},
                {"name": "motto", "value": "<b>beep</b>", "verified_at": null},
            ]),
            "the source block round-trips the `raw_fields` jsonb verbatim, \
             and malformed entries are skipped like the top-level render \
             skips them"
        );

        alice.raw_fields = json!({"not": "an array"});
        assert_eq!(
            source_of(&render_for_owner(&alice, &config, 0))["fields"],
            json!([]),
            "jsonb of another shape renders as the empty list"
        );
    }
}
