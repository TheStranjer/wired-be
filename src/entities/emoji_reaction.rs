//! The emoji-reaction renders: one entry of a [`Status`]'s
//! `pleroma.emoji_reactions` array, and one entry of
//! `GET /api/v1/pleroma/statuses/{id}/reactions`.
//!
//! Both are the same fact at two depths, mirroring Pleroma's split between
//! `StatusView.build_emoji_map/4` (the status render, which names the reactors
//! by id) and `EmojiReactionView.render("show.json", …)`
//! (`lib/pleroma/web/pleroma_api/views/emoji_reaction_view.ex`, the reactions
//! endpoint, which embeds the reactor accounts). [`ReactionSummary`] renders
//! the [`models`](crate::models::emoji_reaction) roll-up for the first, and
//! [`EmojiReaction`] carries the rendered [`Account`] entities for the second.
//!
//! Like every entity (decision 3) nothing here touches the database: the
//! reactor list, the count and the `me` flag arrive from
//! [`crate::models::emoji_reaction`], and the endpoint hands
//! [`EmojiReaction::new`] the accounts it resolved from them.
//!
//! [`Status`]: crate::entities::status::Status

use serde::Serialize;

use crate::{entities::account::Account, models::emoji_reaction::ReactionSummary as SummaryRow};

/// One emoji on a status, as the Status entity's `pleroma.emoji_reactions`
/// array renders it: the roll-up with the first few reactors' ids.
#[derive(Clone, Debug, Serialize)]
pub struct ReactionSummary {
    /// `name`: the emoji as stored, one unicode grapheme or `:shortcode:` —
    /// the text a client sends back in the `PUT` path to react again.
    pub name: String,
    /// `count`: every account that reacted with this emoji, not only the
    /// [`account_ids`](Self::account_ids) listed here.
    pub count: i64,
    /// `me`: whether the viewer is among the reactors; `false` for an
    /// anonymous render.
    pub me: bool,
    /// `url`: the custom emoji's image URL, `null` for a unicode emoji.
    /// Pleroma runs this through its media proxy; this instance serves the URL
    /// the emoji was stored with.
    pub url: Option<String>,
    /// `account_ids`: the reactors as id strings (decision 4), earliest
    /// reaction first, capped by the model at
    /// [`MAX_SUMMARY_REACTORS`](crate::models::emoji_reaction::MAX_SUMMARY_REACTORS).
    pub account_ids: Vec<String>,
}

impl ReactionSummary {
    /// Renders the model's roll-up `summary` as the Status entity's entry:
    /// the reactor ids as the hyphenated `UUIDv7` text clients know them
    /// (decision 4), everything else passed through.
    #[must_use]
    pub fn from_model(summary: &SummaryRow) -> Self {
        Self {
            name: summary.name.clone(),
            count: summary.count,
            me: summary.me,
            url: summary.url.clone(),
            account_ids: summary
                .account_ids
                .iter()
                .map(ToString::to_string)
                .collect(),
        }
    }
}

/// One emoji on a status as the reactions endpoint renders it: the same
/// roll-up, with the reactor accounts themselves instead of their ids.
#[derive(Debug, Serialize)]
pub struct EmojiReaction {
    /// `name`: the emoji as stored.
    pub name: String,
    /// `count`: the reactors this entry lists.
    pub count: i64,
    /// `me`: whether the viewer reacted with this emoji; `false` when they are
    /// anonymous (Pleroma's `!!(user && user.ap_id in user_ap_ids)`).
    pub me: bool,
    /// `url`: the custom emoji's image URL, `null` for a unicode emoji.
    pub url: Option<String>,
    /// `accounts`: every account that reacted, as the [`Account`] entity,
    /// earliest reaction first.
    pub accounts: Vec<Account>,
}

impl EmojiReaction {
    /// One emoji's endpoint entry: `name` and `url` as the model's roll-up
    /// stated them, `me` as the endpoint decided, and `accounts` the reactors it
    /// resolved — which is what `count` counts, so the two can never disagree on
    /// the wire.
    #[must_use]
    pub fn new(name: String, url: Option<String>, me: bool, accounts: Vec<Account>) -> Self {
        let count = i64::try_from(accounts.len()).unwrap_or(i64::MAX);
        Self {
            name,
            count,
            me,
            url,
            accounts,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::{models::emoji_reaction::ReactionSummary as Row, test_fixtures::json_keys};

    /// A model roll-up of `name`, reacted to by `ids`.
    fn row(name: &str, count: i64, me: bool, url: Option<&str>, ids: &[Uuid]) -> Row {
        Row {
            name: name.to_owned(),
            count,
            me,
            url: url.map(str::to_owned),
            account_ids: ids.to_vec(),
        }
    }

    #[test]
    fn a_status_entry_renders_pleromas_five_keys() {
        let alice = Uuid::now_v7();
        let bob = Uuid::now_v7();
        let value = serde_json::to_value(ReactionSummary::from_model(&row(
            ":cat:",
            8,
            true,
            Some("https://x.example/c.png"),
            &[alice, bob],
        )))
        .expect("the entry serializes");
        assert_eq!(
            value,
            json!({
                "name": ":cat:",
                "count": 8,
                "me": true,
                "url": "https://x.example/c.png",
                "account_ids": [alice.hyphenated().to_string(), bob.hyphenated().to_string()],
            }),
            "the whole count rides beside the capped ids, which are the \
             hyphenated text clients know (decision 4)"
        );
        assert_eq!(
            json_keys(&value),
            vec!["account_ids", "count", "me", "name", "url"],
            "the status entry names ids, not accounts"
        );

        let unicode =
            serde_json::to_value(ReactionSummary::from_model(&row("👍", 1, false, None, &[])))
                .expect("the entry serializes");
        assert_eq!(
            unicode["url"],
            Value::Null,
            "a unicode emoji's url is JSON null, not absent"
        );
        assert_eq!(unicode["account_ids"], json!([]), "no reactor, no ids");
    }

    #[test]
    fn an_endpoint_entry_counts_the_accounts_it_lists() {
        let empty = EmojiReaction::new("🎉".to_owned(), None, true, Vec::new());
        assert_eq!(empty.count, 0, "the count is what the render lists");
        assert!(empty.accounts.is_empty(), "and nothing was listed");
        let value = serde_json::to_value(&empty).expect("the entry serializes");
        assert_eq!(
            json_keys(&value),
            vec!["accounts", "count", "me", "name", "url"],
            "Pleroma's five keys, and no `account_ids` on this render"
        );
        assert_eq!(
            (value["name"].as_str(), value["me"].as_bool()),
            (Some("🎉"), Some(true)),
            "the emoji and the viewer flag ride through"
        );
        assert_eq!(value["url"], Value::Null, "a unicode emoji has no image");
        assert_eq!(value["accounts"], json!([]), "and no reactors is `[]`");
    }
}
