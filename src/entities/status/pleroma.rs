//! The `pleroma` extension object of a [`Status`]: its type, the five
//! viewer facts the block renders, and the pure builder that assembles
//! them from the row and the batch's reads.
//!
//! [`Status::from_source`] hands [`pleroma_block`] the row, its replied-to
//! author, its reaction roll-ups and a [`BlockViewer`]; the builder answers
//! the [`PleromaMetadata`] every render of this tree embeds. Nothing here
//! reads a database (decision 3): each key is one stored column or one
//! value the batch already loaded — see [`crate::services::render_status`].

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use super::{PlainText, entity::plain_text};
use crate::{
    entities::{account::JsonBool, emoji_reaction::ReactionSummary},
    models::{
        emoji_reaction::ReactionSummary as SummaryRow, status::Status as StatusRow, user::User,
    },
};

#[cfg(doc)]
use super::{Status, StatusSource};
#[cfg(doc)]
use crate::util::sanitize::strip_html;

/// The `pleroma` extension object of a [`Status`]: the Pleroma-specific
/// keys of `StatusView.render("show.json", …)`.
///
/// `quote_id` and `quote_url` mirror Pleroma's: the quoted status's local
/// id and its public url (see [`quote_id`](PleromaMetadata::quote_id) and
/// [`quote_url`](PleromaMetadata::quote_url)), rendered even when `quote`
/// itself is withheld.
#[derive(Clone, Debug, Serialize)]
pub struct PleromaMetadata {
    /// `content`: the post as its author wrote it — see the renderer's
    /// plain-text rule for where the value comes from: the stored
    /// [`raw_text`](StatusRow::raw_text), falling back to
    /// [`strip_html`] of the content when no source text was stored.
    pub content: PlainText,
    /// `spoiler_text`: the content warning as stored, in the same
    /// `{"text/plain": …}` map (Pleroma's `summary`, and `""` when there
    /// is none).
    pub spoiler_text: PlainText,
    /// `conversation_id`: the row's thread id as the hyphenated lowercase
    /// `UUIDv7` string (decision 4) — this instance's replacement for
    /// Pleroma's deprecated CRC32 context id.
    pub conversation_id: String,
    /// `direct_conversation_id`: the id of the viewer's
    /// [`Participation`](crate::models::conversation::Participation) in
    /// the DM thread this status belongs to, as the hyphenated lowercase
    /// `UUIDv7` text (decision 4), or `null` — the value of the same
    /// [`participation_ids`](crate::models::conversation::participation_ids)
    /// read the batch runs per page, keyed to the row's
    /// [`direct_conversation_id`](StatusRow::direct_conversation_id). A
    /// status of no DM thread, an anonymous viewer and a stranger to the
    /// thread all render `null`; only a participant ever sees a value.
    pub direct_conversation_id: Option<String>,
    /// `in_reply_to_account_acct`: the handle of the replied-to status's
    /// author as stored (decision 7), or `null` when this status answers
    /// nothing, or this instance does not know that author.
    pub in_reply_to_account_acct: Option<String>,
    /// `local`: whether this instance owns the post.
    pub local: JsonBool,
    /// `expires_at`: when the post is due to disappear, in the Mastodon
    /// time format (decision 5), or `null` — Pleroma's `Purge` job
    /// schedule, here the stored [`expires_at`](StatusRow::expires_at)
    /// column.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub expires_at: Option<DateTime<Utc>>,
    /// `thread_muted`: whether the viewer muted this post's whole thread;
    /// the thread-mute ticket's batched
    /// [`muted_conversation_ids`](crate::models::conversation_mute::muted_conversation_ids)
    /// read answers it — the same fact as the top-level `muted`, keyed to
    /// the row's `conversation_id`, so the two always agree.
    pub thread_muted: JsonBool,
    /// `emoji_reactions`: the post's reactions, one entry per emoji in the
    /// roll-up's order (the emoji's first reaction, then the emoji text),
    /// empty for a post nobody reacted to. [`crate::services::render_status`]
    /// batch-loads the whole page's share of them in one read of
    /// [`emoji_reaction::for_statuses`](crate::models::emoji_reaction::for_statuses).
    pub emoji_reactions: Vec<ReactionSummary>,
    /// `parent_visible`: whether the viewer may see the replied-to
    /// status, computed per viewer like Pleroma's
    /// `visible_for_user?(reply_to, for)`. `true` for a root post and for
    /// a reply whose parent the batch's one
    /// [`filter_visible`](crate::models::status_visibility::filter_visible)
    /// read over the page's parents admitted; `false` when the parent is
    /// hidden from the viewer or gone. The page filter narrows the
    /// rendered status, not its parent, so a public reply to a
    /// followers-only post renders `false` for a stranger.
    pub parent_visible: JsonBool,
    /// `quotes_count`: the stored counter of statuses quoting this one.
    /// Pleroma emits it at the top level as well; here it belongs to the
    /// extension block only.
    pub quotes_count: i32,
    /// `quote`: the bare quoted status as a nested entity — the object
    /// the top-level [`Status::quote`] carries as its `quoted_status`,
    /// rendered once and mirrored here the way Pleroma's `StatusView`
    /// does — or `null` when this status quotes nothing or the viewer
    /// may not see the quoted post (see
    /// [`quote_visible`](PleromaMetadata::quote_visible)).
    pub quote: Option<Value>,
    /// `quote_visible`: whether this viewer may see the quoted status, as
    /// the quoting ticket's one batched
    /// [`filter_visible`](crate::models::status_visibility::filter_visible)
    /// read decides; false exactly when [`quote`](PleromaMetadata::quote)
    /// is `null`, which is why the two can never disagree.
    pub quote_visible: JsonBool,
    /// `quote_id`: the quoted status's id as the hyphenated lowercase
    /// `UUIDv7` text (decision 4), or `null` when this status quotes
    /// nothing — the same value as the entity's top-level `quote_id`,
    /// emitted whatever the viewer may see of the quoted post.
    pub quote_id: Option<String>,
    /// `quote_url`: the quoted post's `url`, else its `uri`, or `null`
    /// when this status quotes nothing or the quoted row is gone. Like
    /// [`quote_id`](PleromaMetadata::quote_id) it is rendered even when
    /// [`quote`](PleromaMetadata::quote) is withheld, which is what
    /// Pleroma-FE shows for a quote of an invisible post.
    pub quote_url: Option<String>,
    /// `bookmark_folder`: the id of the folder the viewer's bookmark of
    /// this post is filed in, as the hyphenated lowercase `UUIDv7` text
    /// (decision 4), or `null` — the value of the same
    /// [`bookmarked_folders`](crate::models::bookmark::bookmarked_folders)
    /// read that answers [`Status::bookmarked`] (the folders ticket),
    /// which is why only a bookmarked post can ever carry a folder here.
    pub bookmark_folder: Option<String>,
    /// `pinned_at`: when the viewer pinned this post to their own profile,
    /// in the Mastodon time format (decision 5), or `null` — the value of
    /// the same [`pinned_ids`](crate::models::pinned_status::pinned_ids)
    /// read that answers [`Status::pinned`], which is why a post that
    /// renders `pinned: true` always carries its pin's timestamp here too.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub pinned_at: Option<DateTime<Utc>>,
}

/// The six viewer facts the `pleroma` block renders, grouped because
/// there are more than [`pleroma_block`] could take beside the row, its
/// replied-to author and its reactions (decision 14's five-argument limit).
///
/// Each is the batch's one read per fact, keyed to the row: [`StatusSource`]'s
/// `pinned_at`, `muted`, `bookmark_folder`, `direct_participation`, `quote`
/// and `parent_visible`.
#[derive(Debug)]
pub(super) struct BlockViewer {
    /// When the viewer pinned the row, `None` while they have pinned
    /// nothing.
    pub(super) pinned_at: Option<DateTime<Utc>>,
    /// Whether the viewer muted the row's thread — the same [`JsonBool`] the
    /// entity's top-level `muted` carries, so the two halves never disagree.
    pub(super) muted: JsonBool,
    /// The folder the viewer's bookmark files the row in, `None` when the
    /// bookmark is unfiled or there is none.
    pub(super) bookmark_folder: Option<Uuid>,
    /// The viewer's `conversation_participations` row for the row's DM
    /// thread, `None` when they are no participant or there is no thread.
    pub(super) direct_participation: Option<Uuid>,
    /// The quoted original as the batch rendered it for this viewer,
    /// `None` when there is nothing this viewer may see — the same
    /// object the entity's top-level [`Status::quote`] carries as its
    /// `quoted_status`, which is why the two mirrors can never disagree.
    pub(super) quote: Option<Value>,
    /// The quoted row's `url`, else its `uri`, `None` when there is no
    /// quoted row — set whether or not the viewer may see it.
    pub(super) quote_url: Option<String>,
    /// Whether the viewer may see the replied-to status — the batch's
    /// parent-visibility read, `true` for a root post.
    pub(super) parent_visible: JsonBool,
}

/// Builds the `pleroma` block of `row`, whose replied-to author is
/// `reply_author` when the batch knew one, whose reactions are `reactions`
/// when the batch found any, and whose pin stamp, thread-mute flag,
/// bookmark folder, DM participation and rendered quote are `viewer`'s
/// five facts — see [`PleromaMetadata`] for where each key comes from and
/// which later tickets fill the placeholders.
pub(super) fn pleroma_block(
    row: &StatusRow,
    reply_author: Option<&User>,
    reactions: &[SummaryRow],
    viewer: BlockViewer,
) -> PleromaMetadata {
    let BlockViewer {
        pinned_at,
        muted,
        bookmark_folder,
        direct_participation,
        quote,
        quote_url,
        parent_visible,
    } = viewer;
    let quote_visible = JsonBool(quote.is_some());
    PleromaMetadata {
        content: plain_text(row),
        spoiler_text: PlainText {
            text_plain: row.spoiler_text.clone(),
        },
        conversation_id: row.conversation_id.to_string(),
        direct_conversation_id: direct_participation.map(|id| id.to_string()),
        in_reply_to_account_acct: reply_author.map(|author| author.nickname.clone()),
        local: JsonBool(row.local),
        expires_at: row.expires_at,
        thread_muted: muted,
        emoji_reactions: reactions.iter().map(ReactionSummary::from_model).collect(),
        parent_visible,
        quotes_count: row.quotes_count,
        quote,
        quote_visible,
        quote_id: row.quote_of_id.map(|id| id.to_string()),
        quote_url,
        bookmark_folder: bookmark_folder.map(|id| id.to_string()),
        pinned_at,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::{
        entities::status::test_support::*, models::emoji_reaction::ReactionSummary as SummaryRow,
    };

    #[test]
    fn a_filed_bookmark_lands_in_the_folder_key_too() {
        let alice = base_user("alice");
        let row = base_row();
        let folder = Uuid::parse_str("01955555-5555-7555-8555-555555555555").unwrap();
        let value = render(&row, &Parts::of(&alice).filed_in(folder), None);
        assert_eq!(value["bookmarked"], true, "the file is a bookmark");
        assert_eq!(
            value["pleroma"]["bookmark_folder"],
            folder.to_string(),
            "and the folder the same read names reaches the pleroma block \
             as the hyphenated id (decision 4)"
        );
    }

    #[test]
    fn the_viewers_pin_lands_in_the_flag_and_the_stamp() {
        let alice = base_user("alice");
        let row = base_row();
        let value = render(&row, &Parts::of(&alice).pinned(), None);
        assert_eq!(
            value["pinned"], true,
            "the pins batch's membership reaches the entity verbatim"
        );
        assert_eq!(
            value["pleroma"]["pinned_at"], "2024-01-31T12:34:56.789Z",
            "and the same read's timestamp reaches the pleroma block"
        );
    }

    #[test]
    fn the_viewers_thread_mute_lands_in_both_flags() {
        let alice = base_user("alice");
        let row = base_row();
        let value = render(&row, &Parts::of(&alice).muted_thread(), None);
        assert_eq!(
            value["muted"], true,
            "the thread-mutes batch's membership reaches the entity verbatim"
        );
        assert_eq!(
            value["pleroma"]["thread_muted"], true,
            "and the same membership reaches the pleroma block — the two \
             halves are one lookup"
        );
    }

    #[test]
    fn an_unseeable_quote_keeps_its_id_and_nulls_its_render() {
        let alice = base_user("alice");
        let quoted_id = Uuid::parse_str("01955555-5555-7555-8555-555555555555").unwrap();
        let mut row = base_row();
        row.quote_of_id = Some(quoted_id);
        let value = render(&row, &Parts::of(&alice).quoting_hidden(), None);
        assert_eq!(
            value["quote_id"],
            quoted_id.to_string(),
            "the id is emitted regardless of visibility"
        );
        assert_eq!(
            value["quote"],
            json!({ "state": "unauthorized", "quoted_status": null }),
            "the render is withheld"
        );
        assert_eq!(
            value["pleroma"]["quote"],
            Value::Null,
            "in the pleroma block too"
        );
        assert_eq!(
            value["pleroma"]["quote_visible"], false,
            "false exactly when the quote is not accepted — one fact, two keys"
        );
    }

    #[test]
    fn an_unpinned_row_carries_no_stamp() {
        let alice = base_user("alice");
        let row = base_row();
        let value = render(&row, &Parts::of(&alice), None);
        assert_eq!(value["pinned"], false, "no pin in the batch");
        assert_eq!(
            value["pleroma"]["pinned_at"],
            Value::Null,
            "and so no timestamp either — the two always agree"
        );
    }

    #[test]
    fn the_pleroma_block_carries_the_stored_row() {
        let alice = base_user("alice");
        let row = base_row();
        let pleroma = render(&row, &Parts::of(&alice), None)["pleroma"].clone();
        assert_eq!(pleroma["content"], json!({"text/plain": "hello world"}));
        assert_eq!(pleroma["spoiler_text"], json!({"text/plain": ""}));
        assert_eq!(pleroma["conversation_id"], row.conversation_id.to_string());
        assert_eq!(pleroma["direct_conversation_id"], Value::Null);
        assert_eq!(pleroma["in_reply_to_account_acct"], Value::Null);
        assert_eq!(pleroma["local"], true);
        assert_eq!(pleroma["expires_at"], Value::Null);
        assert_eq!(pleroma["thread_muted"], false);
        assert_eq!(
            pleroma["emoji_reactions"],
            json!([]),
            "no reactions in the batch is the empty array"
        );
        assert_eq!(pleroma["parent_visible"], true);
        assert_eq!(pleroma["quotes_count"], 0);
        assert_eq!(
            pleroma["quote"],
            Value::Null,
            "a row with no rendered quote mirrors the null"
        );
        assert_eq!(
            pleroma["quote_visible"], false,
            "and there is nothing for the viewer to see"
        );
        assert_eq!(pleroma["bookmark_folder"], Value::Null);
        assert_eq!(pleroma["pinned_at"], Value::Null);
    }

    #[test]
    fn the_pleroma_block_lists_the_reactions_the_batch_found() {
        let alice = base_user("alice");
        let bob = other_user("bob", REACTOR_ID);
        let row = base_row();
        let summaries = vec![
            rollup("👍", 2, true, None, &[alice.id, bob.id]),
            rollup(":cat:", 1, false, Some(CAT_IMAGE), &[bob.id]),
        ];
        let pleroma = render(&row, &Parts::of(&alice).reacted(summaries), None)["pleroma"].clone();
        assert_eq!(
            pleroma["emoji_reactions"],
            json!([
                {
                    "name": "👍",
                    "count": 2,
                    "me": true,
                    "url": null,
                    "account_ids": [AUTHOR_ID, REACTOR_ID],
                },
                {
                    "name": ":cat:",
                    "count": 1,
                    "me": false,
                    "url": CAT_IMAGE,
                    "account_ids": [REACTOR_ID],
                },
            ]),
            "the roll-up reaches the wire in the batch's order, with the \
             reactor ids as the hyphenated text clients know"
        );
    }

    /// A model [`SummaryRow`] to render, in one line.
    fn rollup(
        name: &str,
        count: i64,
        me: bool,
        url: Option<&str>,
        account_ids: &[Uuid],
    ) -> SummaryRow {
        SummaryRow {
            name: name.to_owned(),
            count,
            me,
            url: url.map(str::to_owned),
            account_ids: account_ids.to_vec(),
        }
    }
}
