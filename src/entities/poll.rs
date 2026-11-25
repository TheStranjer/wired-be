//! The Mastodon `Poll` entity: the question a status asks and its counts.
//!
//! The reference shape is Pleroma's `PollView.render("show.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/poll_view.ex`); the JSON keys are
//! emitted in the order the Mastodon API names them: `id`, `expires_at`,
//! `expired`, `multiple`, `votes_count`, `voters_count`, `voted`,
//! `own_votes`, `options`, `emojis`. Like every entity
//! (`docs/ARCHITECTURE.md` decision 3) this struct is pure data: [`Poll::build`]
//! turns the [`models::poll`](crate::models::poll) rows plus the viewer's
//! marks into the render without a database or a request, and `now` arrives
//! as a parameter so a test can pin the expiry math to an instant.
//!
//! The rules the builder encodes, per the ticket:
//!
//! - [`expired`](Poll::expired) is a strict `expires_at < now`; a poll
//!   stored with no deadline (the `None` column) never expires. The instant
//!   equal to the deadline is therefore still open.
//! - an option's [`votes_count`](PollOption::votes_count) is JSON `null`
//!   while the poll's `hide_totals` is set and it has not expired; once the
//!   deadline passes the stored counts render like any other poll's.
//! - `voted` and `own_votes` are the viewer's marks, read by
//!   [`voted_option_positions`](crate::models::poll::voted_option_positions);
//!   a viewer who voted nothing gets `false` and `[]`. The poll's own
//!   author gets `voted: true` with `own_votes: []`
//!   ([`Poll::with_author_view`]).
//!
//! The status-wiring ticket connected it: [`crate::services::render_status`]
//! batch-loads the rows and the viewer's marks and hands the built entity
//! to [`Status::poll`](crate::entities::status::Status), which renders it
//! in its `poll` key (`null` for a status that carries none).

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::{
    config::Config,
    entities::{account::JsonBool, custom_emoji::CustomEmoji},
    models::{
        custom_emoji::CustomEmoji as EmojiRow,
        poll::{Poll as PollRow, PollOption as PollOptionRow},
    },
    util::emoji::shortcodes_in,
};

/// One entry of a [`Poll`]'s `options` array: the choice's text and its
/// share of the votes.
#[derive(Clone, Debug, Serialize)]
pub struct PollOption {
    /// The option's text, as its author wrote it.
    pub title: String,
    /// Votes this option has, or JSON `null` while the poll hides its
    /// totals — see [`Poll::build`] for when the value is withheld.
    pub votes_count: Option<i32>,
}

/// The Mastodon `Poll` entity.
///
/// Build one with [`Poll::build`]; its [`Serialize`] impl emits exactly the
/// ten keys of the [module docs](self). Presence: every key is always
/// emitted — the nullable [`expires_at`](Self::expires_at) renders JSON
/// `null` rather than disappearing, and [`emojis`](Self::emojis) the empty
/// list until [`Poll::with_emojis`] fills it. The three booleans ride the same [`JsonBool`] newtype
/// [`Account`](crate::entities::account::Account)'s flags do, which keeps
/// the wire value a plain `true`/`false` while the struct stays under the
/// denied `struct_excessive_bools` threshold (`docs/ARCHITECTURE.md`
/// decision 14).
#[derive(Clone, Debug, Serialize)]
pub struct Poll {
    /// The poll id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    pub id: String,
    /// When voting closes, in the Mastodon time format (decision 5), or
    /// `null` for a poll that never expires.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether the deadline has passed — `false` at the deadline itself
    /// (the comparison is strict) and for a poll with none.
    pub expired: JsonBool,
    /// Whether a voter may mark more than one option.
    pub multiple: JsonBool,
    /// Votes cast in total (the stored roll-up column).
    pub votes_count: i32,
    /// Distinct accounts that have voted (the stored roll-up column).
    pub voters_count: i32,
    /// Whether the viewer has marked anything on this poll.
    pub voted: JsonBool,
    /// The positions of the options the viewer voted for, as given
    /// (the model reads them ascending by `position`); `[]` for a viewer
    /// who has not voted.
    pub own_votes: Vec<i32>,
    /// The choices, in the order voters see them.
    pub options: Vec<PollOption>,
    /// Custom emojis whose `:shortcode:` appears in an option title; `[]`
    /// until [`Poll::with_emojis`] fills it.
    pub emojis: Vec<CustomEmoji>,
}

impl Poll {
    /// Renders the poll `poll` with its `options` (in order) as seen by the
    /// viewer whose marks are `own_votes`, at the instant `now`.
    ///
    /// Pure (decision 3): every key comes from those four inputs.
    /// `now` decides [`expired`](Self::expired) and — through it — whether
    /// `hide_totals` still withholds the per-option counts; passing it in
    /// rather than reading the clock is what makes the tests deterministic.
    #[must_use]
    pub fn build(
        poll: &PollRow,
        options: &[PollOptionRow],
        own_votes: &[i32],
        now: DateTime<Utc>,
    ) -> Self {
        let expired = poll.expires_at.is_some_and(|deadline| deadline < now);
        let hidden = poll.hide_totals && !expired;
        Self {
            id: poll.id.to_string(),
            expires_at: poll.expires_at,
            expired: JsonBool(expired),
            multiple: JsonBool(poll.multiple),
            votes_count: poll.votes_count,
            voters_count: poll.voters_count,
            voted: JsonBool(!own_votes.is_empty()),
            own_votes: own_votes.to_vec(),
            options: options
                .iter()
                .map(|option| PollOption {
                    title: option.title.clone(),
                    votes_count: (!hidden).then_some(option.votes_count),
                })
                .collect(),
            emojis: Vec::new(),
        }
    }

    /// Marks the poll as seen by its own author when `is_author` holds.
    ///
    /// The author's `voted` reads `true` with `own_votes` still `[]`, as
    /// Mastodon's `Poll#voted?` and Pleroma's `PollView` render it, so
    /// clients show results rather than a form the vote endpoint refuses.
    #[must_use]
    pub const fn with_author_view(mut self, is_author: bool) -> Self {
        if is_author {
            self.voted = JsonBool(true);
        }
        self
    }

    /// Fills [`emojis`](Self::emojis) from `emojis`, keeping only those
    /// whose `:shortcode:` appears in an option title.
    ///
    /// `emojis` is any list that may be wider than the poll (a status's
    /// whole emoji list); an emoji used only in the body or the content
    /// warning is dropped, as Mastodon's `PollSerializer#emojis` does. The
    /// kept ones keep the order `emojis` lists them in.
    #[must_use]
    pub fn with_emojis(mut self, emojis: &[EmojiRow], config: &Config) -> Self {
        let used: HashSet<&str> = self
            .options
            .iter()
            .flat_map(|option| shortcodes_in(&option.title))
            .collect();
        let kept: Vec<CustomEmoji> = emojis
            .iter()
            .filter(|emoji| used.contains(emoji.shortcode.as_str()))
            .map(|emoji| CustomEmoji::from_model(emoji, config))
            .collect();
        self.emojis = kept;
        self
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone as _};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    /// The poll id every fixture row carries, as stored text.
    const POLL_ID: &str = "01944444-4444-7444-8444-444444444444";
    /// The status id the fixture poll belongs to.
    const STATUS_ID: &str = "01955555-5555-7555-8555-555555555555";

    /// The instant the open-side renders happen at: 2026-01-01T12:00:00Z.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap()
    }

    /// A poll deadline one hour after [`now`], as a wire string too.
    fn deadline() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 13, 0, 0).unwrap()
    }

    /// The wire text of [`deadline`], in the Mastodon format (decision 5).
    const DEADLINE_WIRE: &str = "2026-01-01T13:00:00.000Z";

    /// A [`PollRow`] with the fixture ids, this deadline and these roll-up
    /// counters: single choice, totals shown. Tests that need the two flags
    /// set them on the returned row, because the gate caps `bool`
    /// parameters at one per function.
    fn poll_row(expires_at: Option<DateTime<Utc>>, voters: i32, votes_cast: i32) -> PollRow {
        PollRow {
            id: Uuid::parse_str(POLL_ID).unwrap(),
            status_id: Uuid::parse_str(STATUS_ID).unwrap(),
            expires_at,
            multiple: false,
            hide_totals: false,
            voters_count: voters,
            votes_count: votes_cast,
            ended_notified: crate::models::user::Flag::default(),
        }
    }

    /// A [`PollOptionRow`] of the fixture poll at `position`, named
    /// `title`, with `votes_count` votes.
    fn option_row(position: i32, title: &str, votes_count: i32) -> PollOptionRow {
        PollOptionRow {
            id: Uuid::now_v7(),
            poll_id: Uuid::parse_str(POLL_ID).unwrap(),
            position,
            title: title.to_owned(),
            votes_count,
        }
    }

    /// The two choices every fixture poll offers.
    fn options() -> Vec<PollOptionRow> {
        vec![option_row(0, "tea", 8), option_row(1, "coffee", 4)]
    }

    /// Shift `instant` by `seconds` (negative goes back in time).
    fn shift(instant: DateTime<Utc>, seconds: i64) -> DateTime<Utc> {
        instant
            .checked_add_signed(TimeDelta::seconds(seconds))
            .unwrap()
    }

    #[test]
    fn a_full_poll_renders_every_field_in_the_ticket_s_shape() {
        let mut row = poll_row(Some(deadline()), 7, 12);
        row.multiple = true;
        let poll = Poll::build(&row, &options(), &[0], now());
        let value = serde_json::to_value(&poll).unwrap();
        assert_eq!(
            value,
            json!({
                "id": POLL_ID,
                "expires_at": DEADLINE_WIRE,
                "expired": false,
                "multiple": true,
                "votes_count": 12,
                "voters_count": 7,
                "voted": true,
                "own_votes": [0],
                "options": [
                    {"title": "tea", "votes_count": 8},
                    {"title": "coffee", "votes_count": 4},
                ],
                "emojis": [],
            }),
            "the whole entity, key for key and value for value"
        );
        // `serde_json` objects are key-sorted without `preserve_order`, so
        // the rendered order stands or falls on the struct's declaration;
        // this pins the field list the object above already implies.
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "emojis",
                "expired",
                "expires_at",
                "id",
                "multiple",
                "options",
                "own_votes",
                "voted",
                "voters_count",
                "votes_count",
            ],
            "exactly the entity's ten fields, nothing more"
        );
    }

    #[test]
    fn hide_totals_withholds_option_counts_while_open_and_lifts_once_expired() {
        let mut hidden = poll_row(Some(deadline()), 5, 12);
        hidden.hide_totals = true;
        let open = serde_json::to_value(Poll::build(&hidden, &options(), &[], now())).unwrap();
        assert_eq!(open["expired"], false, "an hour to go");
        assert_eq!(
            open["options"],
            json!([
                {"title": "tea", "votes_count": null},
                {"title": "coffee", "votes_count": null},
            ]),
            "per-option counts hidden while the poll runs"
        );
        assert_eq!(open["votes_count"], 12, "the roll-up total still renders");
        assert_eq!(open["voters_count"], 5, "and so does the voter count");

        let closed =
            serde_json::to_value(Poll::build(&hidden, &options(), &[], shift(deadline(), 1)))
                .unwrap();
        assert_eq!(closed["expired"], true, "one second past the deadline");
        assert_eq!(
            closed["options"],
            json!([
                {"title": "tea", "votes_count": 8},
                {"title": "coffee", "votes_count": 4},
            ]),
            "the stored counts show once the poll is over"
        );
    }

    #[test]
    fn the_author_view_reads_voted_while_own_votes_stays_the_viewer_s_marks() {
        let row = poll_row(None, 0, 0);
        let author = Poll::build(&row, &options(), &[], now()).with_author_view(true);
        let author_json = serde_json::to_value(&author).unwrap();
        assert_eq!(author_json["voted"], true, "the author reads voted");
        assert_eq!(author_json["own_votes"], json!([]), "with no marks of hers");

        let other = Poll::build(&row, &options(), &[], now()).with_author_view(false);
        let other_json = serde_json::to_value(&other).unwrap();
        assert_eq!(other_json["voted"], false, "anyone else who voted nothing");

        let voter = Poll::build(&row, &options(), &[1], now()).with_author_view(false);
        let voter_json = serde_json::to_value(&voter).unwrap();
        assert_eq!(voter_json["voted"], true, "a voter reads voted");
        assert_eq!(voter_json["own_votes"], json!([1]));
    }

    #[test]
    fn a_viewer_who_voted_nothing_reads_unvoted_with_an_empty_own_votes() {
        let poll = Poll::build(&poll_row(None, 3, 3), &options(), &[], now());
        let value = serde_json::to_value(&poll).unwrap();
        assert_eq!(value["voted"], false, "no marks, no vote");
        assert_eq!(value["own_votes"], json!([]), "and the empty array");
        assert_eq!(value["expires_at"], Value::Null, "a poll with no deadline");
        assert_eq!(value["expired"], false, "is never expired");
    }

    #[test]
    fn a_viewer_who_voted_carries_their_positions_in_voted_and_own_votes() {
        let mut row = poll_row(None, 3, 5);
        row.multiple = true;
        let poll = Poll::build(&row, &options(), &[0, 1], now());
        let value = serde_json::to_value(&poll).unwrap();
        assert_eq!(value["voted"], true, "two marks are a vote");
        assert_eq!(value["own_votes"], json!([0, 1]), "the marks as given");
    }

    #[test]
    fn expiry_is_strict_the_deadline_itself_is_still_open() {
        let row = poll_row(Some(deadline()), 1, 1);
        let at = serde_json::to_value(Poll::build(&row, &options(), &[], deadline())).unwrap();
        assert_eq!(at["expired"], false, "expires_at == now is not expired");
        let past =
            serde_json::to_value(Poll::build(&row, &options(), &[], shift(deadline(), 1))).unwrap();
        assert_eq!(past["expired"], true, "one second past is");
    }

    /// A local enabled [`EmojiRow`] named `shortcode`, with a still url.
    fn emoji_row(shortcode: &str) -> EmojiRow {
        EmojiRow {
            id: Uuid::now_v7(),
            shortcode: shortcode.to_owned(),
            domain: None,
            image_url: format!("https://test.example/emoji/{shortcode}.gif"),
            static_url: Some(format!("https://test.example/emoji/{shortcode}.png")),
            category: None,
            tags: Vec::new(),
            visible_in_picker: true,
            disabled: false,
            inserted_at: now(),
            updated_at: now(),
        }
    }

    #[test]
    fn an_emoji_used_in_an_option_title_renders_with_its_urls() {
        let titles = [option_row(0, ":wiredcat: yes", 0), option_row(1, "no", 0)];
        let poll = Poll::build(&poll_row(None, 0, 0), &titles, &[], now())
            .with_emojis(&[emoji_row("wiredcat")], &Config::for_tests());
        let value = serde_json::to_value(&poll).unwrap();
        let emojis = value["emojis"].as_array().unwrap();
        assert_eq!(emojis.len(), 1, "the one emoji the options use");
        assert_eq!(emojis[0]["shortcode"], "wiredcat", "named by shortcode");
        assert_eq!(
            emojis[0]["url"], "https://test.example/emoji/wiredcat.gif",
            "with its image url"
        );
        assert_eq!(
            emojis[0]["static_url"], "https://test.example/emoji/wiredcat.png",
            "and its still url"
        );
    }

    #[test]
    fn an_emoji_used_only_outside_the_options_is_left_out() {
        let titles = [option_row(0, ":wiredcat: yes", 0), option_row(1, "no", 0)];
        let poll = Poll::build(&poll_row(None, 0, 0), &titles, &[], now()).with_emojis(
            &[emoji_row("blob"), emoji_row("wiredcat")],
            &Config::for_tests(),
        );
        let value = serde_json::to_value(&poll).unwrap();
        let codes: Vec<&str> = value["emojis"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|emoji| emoji["shortcode"].as_str())
            .collect();
        assert_eq!(codes, ["wiredcat"], "`blob` is only in the status body");
    }

    #[test]
    fn options_without_shortcodes_keep_the_emojis_empty() {
        let poll = Poll::build(&poll_row(None, 0, 0), &options(), &[], now())
            .with_emojis(&[emoji_row("wiredcat")], &Config::for_tests());
        assert!(poll.emojis.is_empty(), "no option names an emoji");
    }

    #[test]
    fn the_multiple_flag_passes_through() {
        let single =
            serde_json::to_value(Poll::build(&poll_row(None, 0, 0), &[], &[], now())).unwrap();
        assert_eq!(single["multiple"], false, "single choice by default");
        let mut many_row = poll_row(None, 0, 0);
        many_row.multiple = true;
        let many = serde_json::to_value(Poll::build(&many_row, &[], &[], now())).unwrap();
        assert_eq!(many["multiple"], true, "and many when stored so");
    }
}
