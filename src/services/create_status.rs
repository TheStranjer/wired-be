//! What `POST /api/v1/statuses` accepts: a request body as a client sends it,
//! checked and resolved into the values a status is built from.
//!
//! The reference is Pleroma's `StatusController.create`
//! (`lib/pleroma/web/mastodon_api/controllers/status_controller.ex`) and the
//! one limit check it reaches through `CommonAPI.post`,
//! `Utils.validate_character_limit` (`lib/pleroma/web/common_api/utils.ex`):
//! trimmed text that is empty is refused with "Cannot post an empty status
//! without attachments", text past the instance's limit with "The status is
//! over the character limit", and every parameter the body leaves out falls
//! back to a stored or configured default instead of being an error.
//!
//! Two differences from Pleroma, both deliberate. The measure is
//! [`status_length`], which charges each bare URL `URL_WEIGHT` characters the
//! way Mastodon's `charactersReservedPerUrl` does where Pleroma counts the raw
//! text; and the author's two defaults arrive as plain arguments to
//! [`check_body`] rather than being read off a row, so the whole ruleset is
//! testable without a database. [`create_status`] is the database-facing
//! half the endpoint calls: it supplies those two values from the caller's
//! row, renders the checked text, resolves its mentions, stores the row,
//! its mention and hashtag links, its claimed attachments and the body's
//! poll (when it carried one) in one transaction through
//! [`status::insert_conn`] and [`crate::models::poll::create`], and answers
//! the [`Status`] entity rendered for the author — Pleroma's `for: user`.
//! No endpoint parameter is left outstanding. `scheduled_at` has landed:
//! an RFC3339 instant that routes the request to
//! [`crate::services::schedule_status`] instead of a create, so the key
//! never reaches a stored status through this module's create arms — the
//! schedule parks the normalised params without it, and the publish
//! replays them through [`create_status`] at the parked moment.
//! `poll` has landed: the
//! `poll[options]`/`poll[expires_in]`/`poll[multiple]`/`poll[hide_totals]`
//! shape the [`ApiBody`] extractor already nests joined
//! [`CreateStatusParams`], [`check_body`] runs Pleroma's `CommonAPI` poll
//! rules over it, the checked poll travels on [`Checked::poll`], and both
//! create arms store it beside their status row through
//! [`crate::models::poll::create`], so an accepted poll answers its
//! [`Poll`] entity on the fresh post. `in_reply_to_id` has landed: a
//! body that names a parent posts into that parent's thread, and every
//! reply rule — the parent read through the same viewer-visibility rules
//! as [`status_for_viewer_or_404`] with a reblog row folded to its
//! original, the inherited thread id, author link, audience cap and
//! fallback language, and the auto-mentions of the parent's author and
//! the accounts the parent mentioned — is the `prepared_new_status` step
//! both create arms run before they write (Pleroma's `ActivityDraft`
//! reply arms and `get_to_and_cc`, which carry the parent's actor and
//! addressed users into the reply's addressing without ever touching the
//! text). `media_ids` has landed too: the post's attachments are the
//! poster's own unposted uploads, named as decision 10's id-array, read by
//! `checked_media` (which counts them against
//! [`Config::max_media_attachments`] and refuses id text that is no id)
//! and claimed by the status on the insert's own transaction over
//! [`crate::models::media_attachment::attach_to_status`] — an id that names
//! no row, somebody else's row or a row another status already holds is 422
//! `Media not found`, and a body carrying at least one attachment may
//! post with empty text (Pleroma's `attachments_from_ids` and
//! `validate_character_limit`, with this instance's outright refusal
//! where Pleroma silently drops). `in_reply_to_conversation_id` has landed
//! too (WBE-123's conversation arm): the poster's own
//! [`Participation`](crate::models::conversation::Participation) id, read
//! with the poster as owner through [`conversation::get_participation`]
//! (a malformed, unknown or foreign id is the shared 404), forces the post
//! to `direct`, merges the participation's recipients into its mentions
//! and names its `direct_conversation_id`, so the post answers into that
//! DM thread; the create's `after_status_created` hook then records every
//! local participant's side of the conversation through
//! [`conversation::participate_for_status`].
//!
//! [`ApiBody`]: crate::extract::ApiBody
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404
//!
//! The module is also the shared half of `PUT /api/v1/statuses/{id}`:
//! [`edit_status`] merges the edit body over the stored row, pushes the
//! merge through the very same [`check_body`], and renders the result
//! through [`render_status_fields`] — the function that turns checked text
//! into its stored HTML, its mention links and its hashtags, and the reason
//! an edit renders a post exactly the way a creation would. The edit's own
//! `media_ids` parameter resolves to the ids the author may attach,
//! Pleroma's `attachments_from_ids` dropping the ids that name nothing —
//! the edit wears an unusable id by leaving it out, where the create
//! refuses it outright (and where a poll sent beside non-empty ids is its
//! own refusal — see [`CreateStatusParams`]);
//! [`crate::models::status_edit::update_status`] writes the whole edit as
//! one transaction.
//!
//! Two more arms of `POST /api/v1/statuses` live here because they are
//! decisions about the same body. [`create_status_idempotent`] is the
//! `Idempotency-Key` arm: a claimed key answers the status it created
//! without writing anything, and a fresh one claims its row inside the
//! status's own transaction with `ON CONFLICT DO NOTHING`, so a race of two
//! creates under one key leaves exactly one status (the loser rolls its
//! write back and restates the winner — see
//! [`crate::models::idempotency_key`]). [`preview_status`] is the `preview`
//! arm: the whole `check_body`/`rendered_fields` pipeline runs, but
//! nothing is written and no hook fires — the [`Status`] entity a create
//! would have answered is built in memory from a minted id, `now()` and
//! zero counters, and its `poll` is the `null` of a question nothing was
//! stored for.
//! A body may also name a status to quote under `quote_id` (and under
//! Pleroma's older `quoted_status_id` spelling): the private `quote_target`
//! step resolves it to the live, viewable original before the write, the
//! row stores that original's id as `quote_of_id`, and the counter bump
//! comes from [`status::insert`] itself.
//!
//! [`Status`]: crate::entities::status::Status
//! [`Poll`]: crate::entities::poll::Poll
//! [`status::insert_conn`]: crate::models::status::insert_conn
//!
//! Every refusal of the body's own content is an [`AppError::Unprocessable`]
//! — a 422 whose body is `{"error":"…"}` (decision 8) — in Pleroma's or
//! Mastodon's own wording, pinned by the tests below: the two limit
//! messages, the poll refusals of `checked_poll` — `Poll must contain at
//! least 2 options`, `Poll can't contain more than <max> options`, `Poll
//! options cannot be longer than <chars> characters`, `Expiration date is
//! too soon`, `Expiration date is too far in the future` and `Polls cannot
//! have attachments`, `Invalid
//! visibility` for a submitted scope outside the five [`Visibility`] names,
//! `Invalid content_type` for a MIME type outside `CONTENT_TYPES`, Rails'
//! too-long sentence for a content warning or a language tag past its cap,
//! and the `expires_in` pair — `expires_in must be at least <floor> seconds`
//! naming this instance's floor for a shorter span, `expires_in is too
//! large` for a span no timestamp can hold.
//! An unusable quote target is decision 8's other half: a `quote_id` naming
//! a malformed, unknown, deleted or unseen status is [`AppError::NotFound`]
//! — the shared `{"error":"Record not found"}` — and only a target that is
//! visible but not quotable is the 422 above.

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    config::Config,
    entities::{
        account::JsonBool,
        status::{Status, StatusSource},
    },
    error::{AppError, AppResult},
    models::{
        conversation,
        hashtag::Hashtag,
        idempotency_key, mention,
        oauth_app::OauthApp,
        poll::{self, PollParams},
        status::{self, DEFAULT_CONTENT_TYPE, NewStatus, Visibility},
        status_edit, status_visibility,
        user::User,
    },
    services::{
        federation_out,
        mentions::resolve_mentions,
        notify,
        render_status::{ViewerMode, render_status},
        streaming,
    },
    state::AppState,
    util::{
        entities::{MentionRef, extract_hashtags, linkify_entities},
        format::{render_content, status_length},
        id::{new_id, parse_id},
    },
};

/// What one bare URL costs in a status: Mastodon's
/// `charactersReservedPerUrl`, passed to [`status_length`].
const URL_WEIGHT: usize = 23;

/// Longest content warning the endpoint accepts, in characters.
const MAX_SPOILER_CHARS: usize = 200;

/// Longest language tag the endpoint accepts, in characters.
const MAX_LANGUAGE_CHARS: usize = 10;

/// The MIME types a status may declare, matched verbatim and
/// case-sensitively — the list the instance and nodeinfo endpoints advertise
/// as this server's `post_formats`.
const CONTENT_TYPES: [&str; 3] = ["text/plain", "text/html", "text/markdown"];

/// 422 body for text that leaves nothing behind once trimmed with neither an
/// attachment nor a poll standing beside it (Pleroma's
/// `validate_character_limit` wording; a media-only post carries attachments
/// and a polled post carries its question, so both may send it).
const EMPTY_STATUS: &str = "Cannot post an empty status without attachments";

/// 422 body for more submitted `media_ids` than
/// [`Config::max_media_attachments`] (Pleroma's `validate_attachments_count`
/// wording).
const TOO_MANY_ATTACHMENTS: &str = "Too many attachments";

/// 422 body for a submitted `media_ids` entry that is no id at all: it names
/// no row the poster could claim, so it is [`MEDIA_NOT_FOUND`] with the rest
/// of that gate's refusals.
///
/// [`MEDIA_NOT_FOUND`]: crate::models::media_attachment::MEDIA_NOT_FOUND
const BAD_MEDIA_ID: &str = crate::models::media_attachment::MEDIA_NOT_FOUND;

/// 422 body for text over the instance's limit (Pleroma's wording again).
const OVER_LIMIT: &str = "The status is over the character limit";

/// 422 body for a submitted visibility outside the five names, Pleroma's
/// message for its `:visibility_error`.
const INVALID_VISIBILITY: &str = "Invalid visibility";

/// 422 body for a submitted MIME type outside [`CONTENT_TYPES`].
const INVALID_CONTENT_TYPE: &str = "Invalid content_type";

/// 422 body, in this instance's own wording, for an `expires_in` whose span
/// no calendar can land on: past the representable horizon the value is a
/// mistake, not a very distant expiry.
const EXPIRATION_TOO_LARGE: &str = "expires_in is too large";

/// Most accounts one reply may auto-mention off its parent: the parent's
/// author beside the accounts the parent itself @-mentioned, deduplicated
/// and without the replier (see `inherited_mentions`). The body's own
/// text mentions do not count against this cap — the author wrote them.
const MAX_INHERITED_MENTIONS: usize = 20;

/// 422 body for a poll with fewer than two options: Pleroma's
/// `validate_poll_options_amount` wording.
const TOO_FEW_POLL_OPTIONS: &str = "Poll must contain at least 2 options";

/// 422 body for a poll whose `poll[expires_in]` runs past
/// [`max_poll_expiration_secs`](crate::config::Config::max_poll_expiration_secs):
/// Pleroma's `validate_poll_expiration` wording for the too-far arm.
const POLL_EXPIRATION_TOO_FAR: &str = "Expiration date is too far in the future";

/// 422 body for a poll whose `poll[expires_in]` runs under
/// [`min_poll_expiration_secs`](crate::config::Config::min_poll_expiration_secs):
/// Pleroma's `validate_poll_expiration` wording for the too-soon arm.
const POLL_EXPIRATION_TOO_SOON: &str = "Expiration date is too soon";

/// 422 body for a poll submitted beside non-empty `media_ids`: a question
/// is not posted with pictures.
const POLL_WITH_MEDIA: &str = "Polls cannot have attachments";

/// 422 body for quoting a status the poster may see but may not quote:
/// somebody else's `private`, `direct` or `local` post, Pleroma's quote
/// rule restated in this ticket's wording.
const QUOTE_NOT_ALLOWED: &str = "You can only quote public or unlisted statuses";

/// The body of `POST /api/v1/statuses`, as the client wrote it.
///
/// Every field is an `Option` with a serde `default`, so a key the body omits
/// or sends as `null` stays this module's decision instead of becoming a
/// deserialization error. `poll` joined the struct with the polls ticket:
/// [`check_body`] runs Pleroma's poll rules over it and both create arms
/// store the accepted question beside the status row (`media_ids` beside a
/// poll is the 422 `Polls cannot have attachments`; the ids themselves mean
/// the post's attachments — see the struct's own `media_ids` field).
/// `scheduled_at` joined the struct with the scheduling ticket: a body
/// that sends it is not a create at all — see that field below. The
/// `sensitive` field is a [`flex`] boolean, so a form body's
/// `sensitive=on` spells what a JSON body's `true` spells.
///
/// The [`Serialize`] half is the schedule's: [`crate::services::schedule_status`]
/// stores the normalised params as the pending row's jsonb, and the
/// [`scheduled_statuses.publish`](crate::jobs::scheduled_statuses::KIND_PUBLISH)
/// job reads them back through this same shape.
///
/// [`flex`]: crate::extract::flex
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct CreateStatusParams {
    /// The post text; trimmed, and required to leave something behind.
    #[serde(default)]
    pub status: Option<String>,
    /// The content warning shown in place of the post; at most
    /// `MAX_SPOILER_CHARS` characters, and the empty string when absent.
    #[serde(default)]
    pub spoiler_text: Option<String>,
    /// Whether the post starts folded; the author's own default when the body
    /// does not say.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    pub sensitive: Option<bool>,
    /// The requested audience, one of the five [`Visibility`] names; the
    /// author's default scope when absent.
    #[serde(default)]
    pub visibility: Option<String>,
    /// The content language tag; at most `MAX_LANGUAGE_CHARS` characters,
    /// and no tag at all when absent or empty.
    #[serde(default)]
    pub language: Option<String>,
    /// The MIME type of the `status` text; [`DEFAULT_CONTENT_TYPE`] when
    /// absent.
    #[serde(default)]
    pub content_type: Option<String>,
    /// The status to quote, named by id; absent or `null` posts no quote.
    /// Pleroma's older `quoted_status_id` spelling is accepted as an alias,
    /// in JSON and form bodies alike through the [`ApiBody`] extractor
    /// (decision 10). The service's private `quote_target` step checks the
    /// target and hands the original's id to the stored row's `quote_of_id`.
    ///
    /// [`ApiBody`]: crate::extract::ApiBody
    #[serde(default, alias = "quoted_status_id")]
    pub quote_id: Option<String>,
    /// Whether this is a preview: validate and render, answer the entity a
    /// create would have, write nothing — see [`preview_status`]. Absent,
    /// null and every [`flex`] false spelling are a real post.
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    pub preview: Option<bool>,
    /// Pleroma's expiration extension: seconds from now after which the
    /// post deletes itself. A [`flex`] integer — a form body's
    /// `expires_in=3600` spells what JSON's `3600` spells, and anything
    /// that is no integer at all is the extractor's 422. Absent or null is
    /// a post that never expires; a value under
    /// [`min_expiration_secs`](crate::config::Config::min_expiration_secs)
    /// or too large to land on the calendar is the service's — see
    /// `checked_expiration`.
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_i64")]
    pub expires_in: Option<i64>,
    /// The status this one answers, as id text. A named parent is read
    /// with the same viewer-visibility rules as the show route's shared
    /// gate, so a malformed, unknown, deleted or poster-invisible parent
    /// is the shared 404, and a reblog row folds to the original it
    /// boosts; the reply then carries the parent's thread id, author
    /// link, audience cap and fallback language, and auto-mentions the
    /// parent's author and the accounts the parent mentioned — the post
    /// text itself is never modified. Absent, null or empty is a root
    /// post, Pleroma's own empty-string "no reply" arm.
    #[serde(default)]
    pub in_reply_to_id: Option<String>,
    /// Pleroma's conversation-reply arm: the id text of the poster's own
    /// [`conversation_participation`](crate::models::conversation::Participation)
    /// row. A named participation makes the post a `direct` reply into
    /// that DM thread: the participation's recipients join its mentions,
    /// the visibility is forced to `direct` however the body spelled it,
    /// and the new row lands in the participation's conversation, which
    /// is what `after_status_created`'s
    /// [`participate`](crate::models::conversation::participate) hook then
    /// reads back. A malformed, unknown, or someone-else's id is the
    /// shared 404. Absent, null or empty names no conversation, and the
    /// post stands on [`in_reply_to_id`](Self::in_reply_to_id) alone.
    #[serde(default)]
    pub in_reply_to_conversation_id: Option<String>,
    /// The ids of the poster's own unposted uploads this status carries,
    /// in the order they want them to render — decision 10's id-array:
    /// JSON's `["a","b"]` or the form's `media_ids[]=a&media_ids[]=b`,
    /// both read as id text the way [`EditStatusParams`] takes them.
    /// More than [`Config::max_media_attachments`] is 422 `Too many
    /// attachments`; an entry that is no id, or an id that names no row
    /// the poster owns and no status has claimed yet, is 422
    /// `Media not found` — the write's own gate, through
    /// [`crate::models::media_attachment::attach_to_status`]. With at
    /// least one id here, an empty [`status`](Self::status) is a
    /// media-only post rather than a refusal; a poll sent beside a
    /// non-empty list is 422 `Polls cannot have attachments`. Absent or
    /// null attaches nothing.
    ///
    /// [`EditStatusParams`]: self::EditStatusParams
    #[serde(default)]
    pub media_ids: Option<Vec<String>>,
    /// The poll the body asks for, in the shape the [`ApiBody`]
    /// extractor nests out of `poll[options][]`, `poll[expires_in]`,
    /// `poll[multiple]` and `poll[hide_totals]` form keys — or out of
    /// the equivalent JSON object. [`check_body`] runs Pleroma's
    /// `CommonAPI` rules over it and carries the result as
    /// [`Checked::poll`]; both create arms store it beside the status
    /// row through [`crate::models::poll::create`], and an accepted
    /// poll answers its [`Poll`] entity on the created post.
    ///
    /// [`ApiBody`]: crate::extract::ApiBody
    /// [`Poll`]: crate::entities::poll::Poll
    #[serde(default)]
    pub poll: Option<PollRequest>,
    /// Pleroma's scheduling arm: the instant the post should fire, as
    /// RFC3339 text. A body that sends it is not a create at all: the
    /// endpoint hands the whole request to
    /// [`crate::services::schedule_status`], which validates it exactly
    /// as a create is validated, parks the normalised params (without
    /// this key) until that moment, and answers a
    /// [`ScheduledStatus`](crate::entities::scheduled_status::ScheduledStatus)
    /// entity instead of a [`Status`]. Absent or null posts now; present
    /// text that is no RFC3339 instant is the schedule's 422. This key
    /// is handled by that arm and never by a create — the stored params
    /// the publish replays carry no `scheduled_at`.
    ///
    /// [`Status`]: crate::entities::status::Status
    #[serde(default)]
    pub scheduled_at: Option<String>,
}

/// The `poll` half of a [`CreateStatusParams`] body, as the client wrote
/// it.
///
/// This is the shape the [`ApiBody`] extractor nests out of
/// `poll[options][]`, `poll[expires_in]`, `poll[multiple]` and
/// `poll[hide_totals]` form keys (see [`crate::extract::multipart`]'s
/// bracket-key test) or out of the equivalent JSON object. `expires_in`,
/// `multiple` and `hide_totals` are [`flex`] fields, so a form body's
/// `poll[expires_in]=3600`, `poll[multiple]=true` and
/// `poll[hide_totals]=false` strings spell what JSON's number and booleans
/// spell. `checked_poll` applies Pleroma's `CommonAPI.Utils` poll rules
/// and answers the [`CheckedPoll`] both create arms store.
///
/// The [`Serialize`] half is the schedule's: a parked create keeps this
/// shape in its pending row's jsonb and the publish replays it.
///
/// [`ApiBody`]: crate::extract::ApiBody
/// [`flex`]: crate::extract::flex
#[derive(Debug, Deserialize, Serialize)]
pub struct PollRequest {
    /// The options as posted, in order. `#[serde(default)]` lets a poll
    /// that names no options at all reach `checked_poll`'s minimum of
    /// two rather than failing the extractor.
    #[serde(default)]
    pub options: Vec<String>,
    /// Seconds from now until the poll closes; a [`flex`] integer, with
    /// absent or null meaning a poll that never closes. Accepted values
    /// run from [`Config::min_poll_expiration_secs`] to
    /// [`Config::max_poll_expiration_secs`].
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_i64")]
    pub expires_in: Option<i64>,
    /// Whether one voter may mark several options; a [`flex`] boolean,
    /// `false` when absent or null.
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    pub multiple: Option<bool>,
    /// Whether the per-option counts stay hidden while the poll runs; a
    /// [`flex`] boolean, `false` when absent or null.
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    pub hide_totals: Option<bool>,
}

/// The poll [`check_body`] accepted.
///
/// What its `checked_poll` step measured and computed, handed by both
/// create arms to [`crate::models::poll::create`] beside the status row
/// they insert on the same transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedPoll {
    /// The options as posted, in order; duplicates are the poster's
    /// business (Pleroma's own rules allow them) and every count and
    /// length bound `checked_poll` vouches for.
    pub options: Vec<String>,
    /// The moment voting closes: `now` plus the posted `expires_in`, or
    /// `None` when the body named none and the poll never closes.
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether a voter may mark more than one option: the posted flag,
    /// `false` when the body left it out.
    pub multiple: bool,
    /// Whether the per-option counts stay hidden: the posted flag,
    /// `false` when the body left it out.
    pub hide_totals: bool,
}

impl CreateStatusParams {
    /// Whether this body asks for a preview ([`preview_status`]) rather
    /// than a post: the submitted flag, `false` when the body leaves it
    /// out, sends `null`, or spells any of the [`flex`] false forms. The
    /// endpoint asks this before it decides which service arm to run; the
    /// field itself plays no part in [`check_body`]'s validation.
    ///
    /// [`flex`]: crate::extract::flex
    #[must_use]
    pub const fn previews(&self) -> bool {
        matches!(self.preview, Some(true))
    }
}

/// A checked status body: what [`check_body`] resolved, borrowed from the
/// [`CreateStatusParams`] it read, ready for a caller to build a row from.
#[derive(Debug)]
pub struct Checked<'a> {
    /// The post text, trimmed and within the instance's limit as
    /// [`status_length`] measures it.
    pub text: &'a str,
    /// The content warning exactly as submitted, or the empty string when the
    /// body carried none; never trimmed, because a warning is stored verbatim.
    pub spoiler_text: &'a str,
    /// The submitted flag, or the author's default the caller passed in.
    pub sensitive: bool,
    /// The submitted audience, or the author's default scope the caller passed
    /// in, or [`Visibility::Public`] behind a stored scope that names nothing.
    pub visibility: Visibility,
    /// The submitted language tag; `None` when the body gave none or gave the
    /// empty string, which is how a client clears a tag it posted before.
    pub language: Option<&'a str>,
    /// The declared MIME type, always one of `CONTENT_TYPES` —
    /// [`DEFAULT_CONTENT_TYPE`] when the body did not say.
    pub content_type: &'static str,
    /// The moment the post deletes itself, `now` plus the submitted
    /// `expires_in`, or `None` when the body named no expiration — see
    /// `checked_expiration`.
    pub expires_at: Option<DateTime<Utc>>,
    /// The submitted `media_ids` read as ids, deduplicated to their first
    /// position and in the order the client sent them — empty when the body
    /// named none. Whether each one is a row the poster may actually claim
    /// is the write's decision, made atomically with the insert (see
    /// [`crate::models::status::insert`]).
    pub media_ids: Vec<Uuid>,
    /// The poll the body asked for, after [`check_body`]'s poll rules —
    /// see `checked_poll` — or `None` when the body carried none. The
    /// storage payload: both create arms hand it to
    /// [`crate::models::poll::create`] beside their status row, and the
    /// render batch reads it back into the entity's `poll` key.
    pub poll: Option<CheckedPoll>,
}

/// Checks a status body against the instance's limits and the author's two
/// defaults, or refuses it.
///
/// `config` supplies the length cap ([`Config::max_status_chars`],
/// `docs/ARCHITECTURE.md` decision 12). `default_scope` and
/// `default_sensitive` are the author's own preferences — Pleroma reads the
/// same two off the user as `default_scope` and `default_sensitive`, and here
/// they come in as plain values so that this module needs no database.
/// Whatever the body does not say, they answer; whatever it does say, they
/// lose to, except that a stored scope naming no [`Visibility`] falls back to
/// [`Visibility::Public`] rather than failing somebody's post.
///
/// The rules run in this order — the attachments (count, then id text), the
/// text (empty only with attachments or a poll standing behind it, then too
/// long), the poll (`checked_poll`, Pleroma's `CommonAPI` rules: options,
/// expiration, `media_ids`), the content warning, the language, the
/// visibility, the content type, then the expiration — so a body that
/// breaks several is refused for the first of them, and a media-only or
/// polled post is read before its empty text can be refused, the way
/// Pleroma's draft pipeline resolves attachments before it checks the
/// payload. The attachment refusal an id the poster may not claim earns is
/// not here — it is the write's, made on the database inside the status's
/// own transaction (see [`crate::models::status::insert`]).
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `EMPTY_STATUS` (skipped when the body
/// carries attachments or a poll, either of which stands in for the text),
/// `OVER_LIMIT`, `INVALID_VISIBILITY` or `INVALID_CONTENT_TYPE`, with
/// `checked_media`'s refusals for more submitted `media_ids` than the
/// instance allows or for an entry that is no id at all, with
/// `checked_poll`'s six refusals for a poll that breaks one of its rules,
/// with Rails' too-long wording for a content warning or a language tag
/// past its cap, and with `checked_expiration`'s refusal for an
/// `expires_in` under the instance's floor or past the calendar.
pub fn check_body<'a>(
    config: &Config,
    default_scope: &str,
    default_sensitive: bool,
    params: &'a CreateStatusParams,
) -> AppResult<Checked<'a>> {
    let media_ids = checked_media(config, params)?;
    let has_poll = params.poll.is_some();
    let text = check_text(
        config,
        params.status.as_deref().unwrap_or_default(),
        !media_ids.is_empty() || has_poll,
    )?;
    let poll = checked_poll(config, params)?;
    let spoiler_text = checked_spoiler(params.spoiler_text.as_deref())?;
    let language = checked_language(params.language.as_deref())?;
    let visibility = checked_visibility(params.visibility.as_deref(), default_scope)?;
    let content_type = checked_content_type(params.content_type.as_deref())?;
    let expires_at = checked_expiration(config, params.expires_in)?;
    Ok(Checked {
        text,
        spoiler_text,
        sensitive: params.sensitive.unwrap_or(default_sensitive),
        visibility,
        language,
        content_type,
        expires_at,
        media_ids,
        poll,
    })
}

/// The submitted `media_ids` read as ids: the whole body's refusals that
/// need no database — more entries than [`Config::max_media_attachments`]
/// is [`TOO_MANY_ATTACHMENTS`] (counted on the submission as sent, repeats
/// and all, Pleroma's `validate_attachments_count` cap), and an entry that
/// is no [`Uuid`] text names no row anybody could claim and is
/// [`BAD_MEDIA_ID`]. The ids then collapse to their first positions, so the
/// order the client asked for survives to the write unchanged.
///
/// Whether each surviving id names a row its poster may attach is the
/// write's, made on the database as the status's own insert commits (see
/// [`crate::models::status::insert`]).
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`TOO_MANY_ATTACHMENTS`] or
/// [`BAD_MEDIA_ID`].
fn checked_media(config: &Config, params: &CreateStatusParams) -> AppResult<Vec<Uuid>> {
    let Some(submitted) = params.media_ids.as_deref() else {
        return Ok(Vec::new());
    };
    if submitted.len() > config.max_media_attachments {
        return Err(AppError::Unprocessable(TOO_MANY_ATTACHMENTS.to_owned()));
    }
    let mut ids: Vec<Uuid> = Vec::with_capacity(submitted.len());
    for text in submitted {
        let id = parse_id(text).ok_or_else(|| AppError::Unprocessable(BAD_MEDIA_ID.to_owned()))?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// The post text: trimmed, no longer than [`Config::max_status_chars`]
/// measured with [`status_length`]'s weighting, and — with neither
/// attachments nor a poll standing beside it — not empty. A media-only
/// post trims to nothing and still stands, and so does a polled one:
/// Pleroma's `validate_character_limit` refuses `""` only over an empty
/// attachment list, and its `CommonAPI` posts a question with no text.
/// The cap itself survives either companion; a poll does not lift it.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`EMPTY_STATUS`] (empty text and
/// `can_stand_empty` false) or [`OVER_LIMIT`].
fn check_text<'a>(config: &Config, text: &'a str, can_stand_empty: bool) -> AppResult<&'a str> {
    let trimmed = text.trim();
    if trimmed.is_empty() && !can_stand_empty {
        return Err(AppError::Unprocessable(EMPTY_STATUS.to_owned()));
    }
    if status_length(trimmed, URL_WEIGHT) > config.max_status_chars {
        return Err(AppError::Unprocessable(OVER_LIMIT.to_owned()));
    }
    Ok(trimmed)
}

/// The poll of a create body, checked by Pleroma's `CommonAPI` rules
/// (`validate_poll_options_amount`, `validate_poll_options_length` and
/// `validate_poll_expiration` of `lib/pleroma/web/common_api/utils.ex`),
/// or `None` when the body carried none.
///
/// In order: at least two options and at most
/// [`Config::max_poll_options`] of them (duplicates are allowed — Pleroma
/// dedups at render, and this implementation stores every option as
/// posted); then every option non-empty and no longer than
/// [`Config::max_poll_option_chars`] characters, measured in scalar
/// values; then the posted `expires_in`, when present, inside
/// [`Config::min_poll_expiration_secs`] to
/// [`Config::max_poll_expiration_secs`] — the accepted span becomes
/// [`CheckedPoll::expires_at`], `now` plus the seconds (a span so far out
/// no calendar can land on it closes never, which no configuration that
/// keeps the shipped ceiling can reach); and last the one cross-parameter
/// rule, that a poll sent beside non-empty `media_ids` is refused:
/// questions are not asked over attachments.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`TOO_FEW_POLL_OPTIONS`],
/// `Poll can't contain more than {max_poll_options} options`,
/// `Poll options cannot be longer than {max_poll_option_chars}
/// characters`, [`POLL_EXPIRATION_TOO_FAR`], [`POLL_EXPIRATION_TOO_SOON`]
/// or [`POLL_WITH_MEDIA`].
fn checked_poll(config: &Config, params: &CreateStatusParams) -> AppResult<Option<CheckedPoll>> {
    let Some(request) = params.poll.as_ref() else {
        return Ok(None);
    };
    checked_poll_options(config, &request.options)?;
    checked_poll_option_chars(config, &request.options)?;
    let expires_at = checked_poll_expiration(config, request.expires_in)?;
    checked_poll_media(params.media_ids.as_deref())?;
    Ok(Some(CheckedPoll {
        options: request.options.clone(),
        expires_at,
        multiple: request.multiple.unwrap_or(false),
        hide_totals: request.hide_totals.unwrap_or(false),
    }))
}

/// The option-count arms of [`checked_poll`]: at least two, at most
/// [`Config::max_poll_options`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`TOO_FEW_POLL_OPTIONS`] or
/// `Poll can't contain more than {max_poll_options} options`.
fn checked_poll_options(config: &Config, options: &[String]) -> AppResult<()> {
    if options.len() < 2 {
        return Err(AppError::Unprocessable(TOO_FEW_POLL_OPTIONS.to_owned()));
    }
    if options.len() > config.max_poll_options {
        return Err(AppError::Unprocessable(format!(
            "Poll can't contain more than {} options",
            config.max_poll_options
        )));
    }
    Ok(())
}

/// The option-length arm of [`checked_poll`]: no option empty, none
/// past [`Config::max_poll_option_chars`] scalar values — Pleroma's one
/// message covers both spellings of a broken option.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `Poll options cannot be longer than
/// {max_poll_option_chars} characters`.
fn checked_poll_option_chars(config: &Config, options: &[String]) -> AppResult<()> {
    let broken = options.iter().any(|option| {
        let length = option.chars().count();
        length == 0 || length > config.max_poll_option_chars
    });
    if broken {
        return Err(AppError::Unprocessable(format!(
            "Poll options cannot be longer than {} characters",
            config.max_poll_option_chars
        )));
    }
    Ok(())
}

/// The expiration arm of [`checked_poll`]: the posted span, when present,
/// must land inside the instance's poll window, and the accepted instant
/// is computed once here — the same value [`create_status`] stores
/// as the poll's `expires_at` (see [`crate::models::poll::PollParams`]).
/// Bounds too wide for the `i64` the comparison runs in become the
/// nearest number it can hold: a ceiling that cannot be represented
/// refuses nothing, a floor that cannot be represented refuses every
/// submitted span, neither silently permissive.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`POLL_EXPIRATION_TOO_FAR`] above the
/// ceiling or [`POLL_EXPIRATION_TOO_SOON`] under the floor.
fn checked_poll_expiration(
    config: &Config,
    seconds: Option<i64>,
) -> AppResult<Option<DateTime<Utc>>> {
    let Some(secs) = seconds else {
        return Ok(None);
    };
    let max = i64::try_from(config.max_poll_expiration_secs).unwrap_or(i64::MAX);
    if secs > max {
        return Err(AppError::Unprocessable(POLL_EXPIRATION_TOO_FAR.to_owned()));
    }
    let min = i64::try_from(config.min_poll_expiration_secs).unwrap_or(i64::MAX);
    if secs < min {
        return Err(AppError::Unprocessable(POLL_EXPIRATION_TOO_SOON.to_owned()));
    }
    Ok(TimeDelta::try_seconds(secs).and_then(|span| Utc::now().checked_add_signed(span)))
}

/// The cross-parameter arm of [`checked_poll`]: a poll sent beside
/// non-empty `media_ids` is the attachment conflict; an empty or absent
/// list is no attachment at all.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`POLL_WITH_MEDIA`].
fn checked_poll_media(media_ids: Option<&[String]>) -> AppResult<()> {
    if media_ids.is_some_and(|ids| !ids.is_empty()) {
        return Err(AppError::Unprocessable(POLL_WITH_MEDIA.to_owned()));
    }
    Ok(())
}

/// The content warning: [`MAX_SPOILER_CHARS`] characters at most, and the
/// empty string when the body carried none — what the stored column holds for
/// a post without a warning.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with the too-long wording of [`checked_len`].
fn checked_spoiler(spoiler_text: Option<&str>) -> AppResult<&str> {
    checked_len(
        spoiler_text.unwrap_or_default(),
        MAX_SPOILER_CHARS,
        "Spoiler text",
    )
}

/// The language tag, or `None`: absent and empty both mean no tag, and a
/// present one must fit [`MAX_LANGUAGE_CHARS`] characters.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with the too-long wording of [`checked_len`].
fn checked_language(language: Option<&str>) -> AppResult<Option<&str>> {
    let Some(tag) = language.filter(|tag| !tag.is_empty()) else {
        return Ok(None);
    };
    checked_len(tag, MAX_LANGUAGE_CHARS, "Language").map(Some)
}

/// The audience: the submitted name, which must be one of the five
/// [`Visibility`] spellings; or, absent, the author's `default_scope` read the
/// same way, with [`Visibility::Public`] behind a stored value that names
/// nothing.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_VISIBILITY`] for a *submitted*
/// name outside the five; an unusable default is never the poster's fault.
fn checked_visibility(submitted: Option<&str>, default_scope: &str) -> AppResult<Visibility> {
    submitted.map_or_else(
        || Ok(default_scope.parse().unwrap_or(Visibility::Public)),
        |name| {
            name.parse()
                .ok()
                .ok_or_else(|| AppError::Unprocessable(INVALID_VISIBILITY.to_owned()))
        },
    )
}

/// The MIME type: the submitted one, which must equal an entry of
/// [`CONTENT_TYPES`] case-sensitively, or [`DEFAULT_CONTENT_TYPE`] when the
/// body did not name one.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_CONTENT_TYPE`] for anything
/// else, including Pleroma's own `text/x-markdown`.
fn checked_content_type(submitted: Option<&str>) -> AppResult<&'static str> {
    submitted.map_or(Ok(DEFAULT_CONTENT_TYPE), |name| {
        CONTENT_TYPES
            .into_iter()
            .find(|known| *known == name)
            .ok_or_else(|| AppError::Unprocessable(INVALID_CONTENT_TYPE.to_owned()))
    })
}

/// The text, when it fits `max_chars` scalar values (not bytes); otherwise
/// the 422 Rails puts in its changeset errors, naming `what` and the cap.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with
/// `Validation failed: {what} is too long (maximum is {max_chars} characters)`.
fn checked_len<'a>(text: &'a str, max_chars: usize, what: &str) -> AppResult<&'a str> {
    if text.chars().count() > max_chars {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {what} is too long (maximum is {max_chars} characters)"
        )));
    }
    Ok(text)
}

/// The instance's expiration floor as the `i64` the `expires_in` comparison
/// runs in: [`min_expiration_secs`](crate::config::Config::min_expiration_secs)
/// widened, with a floor too wide for `i64` becoming the largest number the
/// comparison can hold — a floor nobody can post past, not a silent zero.
fn expiration_floor(config: &Config) -> i64 {
    i64::try_from(config.min_expiration_secs).unwrap_or(i64::MAX)
}

/// The expiry an `expires_in` of `seconds` describes: `None` when the body
/// named none, and otherwise the instant `seconds` from now — Pleroma's
/// `:expiration, minimum_expiration_interval` rule, which here is
/// [`Config::min_expiration_secs`]: a shorter span (zero and negatives
/// included) is refused rather than silently lengthened, and so is one whose
/// span no calendar can land on. The accepted instant is computed once,
/// here, and travels to the stored row and to the scheduled delete's
/// `run_at` as the same value.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `expires_in must be at least {floor}
/// seconds` — naming this instance's floor — for a submitted value under
/// [`expiration_floor`], and with [`EXPIRATION_TOO_LARGE`] for a span that
/// does not fit a timestamp.
fn checked_expiration(config: &Config, seconds: Option<i64>) -> AppResult<Option<DateTime<Utc>>> {
    let Some(secs) = seconds else {
        return Ok(None);
    };
    let floor = expiration_floor(config);
    if secs < floor {
        return Err(AppError::Unprocessable(format!(
            "expires_in must be at least {floor} seconds"
        )));
    }
    TimeDelta::try_seconds(secs)
        .and_then(|span| Utc::now().checked_add_signed(span))
        .map(Some)
        .ok_or_else(|| AppError::Unprocessable(EXPIRATION_TOO_LARGE.to_owned()))
}

/// What reading a status text came to: the HTML to store, and the two
/// entity lists that go with it in the order the text carries them.
#[derive(Debug)]
pub struct RenderedFields {
    /// The rendered, linkified HTML for the `content` column.
    pub content: String,
    /// The accounts the text @-mentioned, in mention order.
    pub mentioned_user_ids: Vec<Uuid>,
    /// The hashtags the text carries, as [`extract_hashtags`] folded them.
    pub hashtags: Vec<String>,
}

/// Creates the status `params` describes as a local post by `user` and
/// answers its [`Status`] entity as the author sees it.
///
/// Nothing is written before [`check_body`] has accepted the body: it
/// supplies the author's own two defaults — the row's
/// [`default_scope`](User::default_scope) and
/// [`sensitive_default`](User::sensitive_default), Pleroma reads the same
/// pair off the user — and [`create_status`] does the rest: the quote
/// target (the private `quote_target` step in the shared
/// `prepared_new_status` below, resolved before anything is written), the
/// render and the entity lists (the [`render_status_fields`] step below),
/// the stored row with its mention and hashtag links, its claimed
/// attachments and the author's moved counters, the body's poll stored
/// beside it when it carried one (the `store_poll` step below) — and the
/// quoted original's `quotes_count`, stepped by [`status::insert`] along
/// with the row — and the render for the author. `app` — the client
/// that issued the presenting
/// token, `None` when the caller names none — is recorded as the post's
/// `application` pair, Pleroma's `put_application`.
///
/// The body's `media_ids` ride the stored row: [`status::insert`] claims
/// exactly the poster's own uploads no status has taken yet, on the insert's
/// own transaction, each at its position in the submitted order, and one id
/// the poster may not claim refuses the whole write (422 `Media not found`,
/// the media model's gate — see [`crate::models::media_attachment`]).
///
/// When the body names an `in_reply_to_id`, the shared `prepared_new_status`
/// step resolves the parent before anything renders: an unanswerable parent
/// is the shared 404, and an answerable one lends the row its thread, author
/// link, capped audience, fallback language and auto-mentions (Pleroma's
/// `ActivityDraft` reply arms), with the parent's own `replies_count`
/// stepped by [`status::insert`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] for the body's own refusals, through
/// [`check_body`] — attachments included, both the
/// [`Config::max_media_attachments`] cap and the write's `Media not found`
/// gate — and for a quote target that is visible but not quotable, through
/// the private `quote_target` step; [`AppError::NotFound`] for an
/// `in_reply_to_id` that names no status the poster may see and likewise for
/// a `quote_id` naming a malformed, unknown, deleted or unseen status,
/// through `quote_target`; [`AppError::Internal`] for the one row this
/// function wrote that the renderer refuses to answer (unreachable for a
/// fresh local post, which is neither a tombstone nor a reblog). Any
/// database failure arrives through `?` as [`AppError::Database`].
pub async fn create_status(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    params: CreateStatusParams,
) -> AppResult<Status> {
    let (new, poll) = prepared_new_status(state, user, app, &params).await?;
    let mut tx = state.db.begin().await?;
    let row = status::insert_conn(&mut tx, &state.config.base_url, &new).await?;
    store_poll(&mut tx, row.id, poll.as_ref()).await?;
    tx.commit().await?;
    created_entity(state, user, row).await
}

/// Stores the checked body's poll beside its freshly inserted status row,
/// on the create's open transaction: [`poll::create`] with the whole
/// [`CheckedPoll`] reshaped into [`PollParams`]. A body that carried no
/// poll writes nothing here, so a plain create is the same write it always
/// was; and a create that never commits — an idempotency-key race lost
/// after this point — leaves no stray poll behind, because the rollback
/// takes the poll and its options down with the status: a stored poll can
/// only ever belong to a status that was committed beside it.
///
/// # Errors
///
/// Any database failure of the poll or option insert arrives through `?`
/// as [`AppError::Database`].
async fn store_poll(
    conn: &mut PgConnection,
    status_id: Uuid,
    poll: Option<&CheckedPoll>,
) -> AppResult<()> {
    let Some(poll) = poll else {
        return Ok(());
    };
    poll::create(
        conn,
        status_id,
        &PollParams {
            options: poll.options.clone(),
            expires_at: poll.expires_at,
            multiple: poll.multiple,
            hide_totals: poll.hide_totals,
            ..PollParams::default()
        },
    )
    .await
    .map(drop)
}

/// The shared tail of both create arms: the fresh row's log line, the
/// [`statuses.expire`](crate::jobs::statuses::KIND_EXPIRE) job a row carrying
/// an expiry schedules for itself (see [`schedule_expiration`]), its
/// [`Status`] entity as `user` — the author — sees it, and the
/// [`after_status_created`] hook every consumer of a new post hangs off.
///
/// [`schedule_expiration`]: crate::jobs::statuses::schedule_expiration
///
/// # Errors
///
/// [`AppError::Internal`] for the one row this module wrote that the
/// renderer refuses to answer (unreachable for a fresh local post, which is
/// neither a tombstone nor a reblog); any database failure behind the render
/// arrives through `?` as [`AppError::Database`].
async fn created_entity(state: &AppState, user: &User, row: status::Status) -> AppResult<Status> {
    debug!(
        status = %row.id,
        user = %user.id,
        visibility = %row.visibility,
        "status created"
    );
    crate::jobs::statuses::schedule_expiration(state, &row).await;
    let entity = render_status(state, ViewerMode::user(user), &row)
        .await?
        // Unreachable for what was written a moment ago — live, local and no
        // reblog, so the renderer has nothing to drop it for — and a 500
        // rather than a panic if it ever happens.
        .ok_or_else(|| AppError::Internal(format!("the status {} did not render", row.id)))?;
    after_status_created(state, &row).await;
    Ok(entity)
}

/// The `quote_of_id` a body's `quote_id` names: `None` when the body named no
/// target (absent or `null` — posting without a quote is untouched), the id
/// of the quotable original once the target has been read and checked.
///
/// The checks run in the order the ticket pins them, and every miss on the
/// target is decision 8's shared 404 so a probe learns nothing:
/// [`parse_id`] refuses text that is not an id, [`status::get_by_id`] hides
/// unknown rows and tombstones alike, a reblog row is walked through to the
/// original it boosts — the quote is always of the post underneath, and that
/// is the id stored — and [`status_visibility::can_view`] hides what the
/// poster may not see. Past the gate, only a [`Visibility::Public`] or
/// [`Visibility::Unlisted`] original may be quoted, its own author excepted:
/// whoever may see their own `private`, `direct` or `local` post may quote
/// it, and anybody quoting somebody else's gets the 422 wording of
/// [`QUOTE_NOT_ALLOWED`].
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown, deleted or unseen
/// target, [`AppError::Unprocessable`] with [`QUOTE_NOT_ALLOWED`] for a
/// visible but unquotable one. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn quote_target(
    state: &AppState,
    user: &User,
    quote_id: Option<&str>,
) -> AppResult<Option<Uuid>> {
    let Some(text) = quote_id else {
        return Ok(None);
    };
    let id = parse_id(text).ok_or(AppError::NotFound)?;
    let row = status::get_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let target = quoted_original(state, row).await?;
    if !status_visibility::can_view(&state.db, &target, Some(user)).await? {
        return Err(AppError::NotFound);
    }
    if !matches!(target.visibility, Visibility::Public | Visibility::Unlisted)
        && target.user_id != user.id
    {
        return Err(AppError::Unprocessable(QUOTE_NOT_ALLOWED.to_owned()));
    }
    Ok(Some(target.id))
}

/// Follows `row`'s reblog links through to the original a quote of it names:
/// reblogs are rows too (decision 6), a client that names a boost means the
/// post underneath it, and a broken or tombstoned link is the same 404 any
/// other miss on the target is. Chains longer than one link do not occur in
/// stored data, but the loop walks one to its end all the same.
async fn quoted_original(state: &AppState, row: status::Status) -> AppResult<status::Status> {
    let mut current = row;
    while let Some(original_id) = current.reblog_of_id {
        current = status::get_by_id(&state.db, original_id)
            .await?
            .ok_or(AppError::NotFound)?;
    }
    Ok(current)
}

/// Creates the status `params` describes under the request's
/// `Idempotency-Key`, or restates the one that key already created.
///
/// When the pair `(user, key)` already names a live status, that status's
/// entity is the whole answer (200 for the endpoint as everywhere here):
/// nothing is checked beyond the key, nothing is written, and no hook fires
/// a second time. Otherwise the create runs exactly as [`create_status`]
/// does, with one more statement on its transaction: the key is claimed
/// around the freshly written status with `ON CONFLICT DO NOTHING` (see
/// [`idempotency_key::claim`]), so two requests racing under one key
/// serialize on that insert. The winner commits its own status — and its
/// poll, stored on the same transaction by `store_poll`, with it; the
/// loser finds the pair taken, rolls its write back without a trace (poll
/// and options included: a replayed or raced key leaves exactly one poll
/// row behind) and answers
/// the winner's entity. The third arm — the pair is taken but its status has
/// been deleted since — moves the key onto the fresh status
/// ([`idempotency_key::reassign`]), because a dead post is not a
/// replay-able answer. A body naming a `quote_id` is a create like any
/// other through this arm: the one committed write counts the original
/// exactly once, and a replay or a lost race never counts it again.
///
/// # Errors
///
/// Those of [`create_status`], plus [`AppError::NotFound`] for the one row
/// a replayed render cannot answer (a status deleted in the window between
/// the key read and the render).
pub async fn create_status_idempotent(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    params: CreateStatusParams,
    key: &str,
) -> AppResult<Status> {
    if let Some(row) = idempotency_key::live_status(&state.db, user.id, key).await? {
        debug!(
            status = %row.id,
            user = %user.id,
            "status reposted from its idempotency key"
        );
        return replayed_entity(state, user, &row).await;
    }
    let (new, poll) = prepared_new_status(state, user, app, &params).await?;
    let mut tx = state.db.begin().await?;
    let row = status::insert_conn(&mut tx, &state.config.base_url, &new).await?;
    store_poll(&mut tx, row.id, poll.as_ref()).await?;
    if idempotency_key::claim(&mut tx, user.id, key, row.id).await? {
        tx.commit().await?;
        return created_entity(state, user, row).await;
    }
    // The pair is taken — by a create that raced this one, or by a status
    // since deleted. Whichever, the re-read on this open transaction says
    // which, because a skipped `DO NOTHING` only ever names committed rows.
    if let Some(winner) = idempotency_key::live_status_conn(&mut tx, user.id, key).await? {
        // Ours never happened: the racing create's row is the key's answer,
        // and our uncommitted write goes back with this rollback.
        discard(tx).await;
        debug!(
            status = %winner.id,
            user = %user.id,
            "status reposted from a raced idempotency key"
        );
        return replayed_entity(state, user, &winner).await;
    }
    idempotency_key::reassign(&mut tx, user.id, key, row.id).await?;
    tx.commit().await?;
    created_entity(state, user, row).await
}

/// Rolls back the transaction of a create that lost its key race, logging a
/// rollback that failed: the answer the caller is about to return needs
/// nothing of ours, and a connection that dies before its `ROLLBACK` takes
/// the uncommitted write with it anyway.
async fn discard(tx: sqlx::Transaction<'_, sqlx::Postgres>) {
    if let Err(err) = tx.rollback().await {
        warn!(
            %err,
            "idempotent status create could not roll its losing write back"
        );
    }
}

/// Renders an already-stored status — the key's answer — as `user`, its
/// author, sees it: the create's render with no write and no hook behind it,
/// and a status that vanished between the key read and here is the 404.
///
/// # Errors
///
/// [`AppError::NotFound`] for a row the renderer cannot answer; any
/// database failure arrives through `?` as [`AppError::Database`].
async fn replayed_entity(state: &AppState, user: &User, row: &status::Status) -> AppResult<Status> {
    render_status(state, ViewerMode::user(user), row)
        .await?
        .ok_or(AppError::NotFound)
}

/// Builds the [`Status`] entity `params` describes **without writing
/// anything**.
///
/// Every check [`create_status`] runs, the same render with its mentions
/// resolved and its tags extracted, and an entity whose `id` is a freshly
/// minted `UUIDv7` (decision 4), whose `created_at` is now and whose
/// counters are zero — Pleroma's `preview` parameter, whose
/// `ActivityPub.create(changes, true)` answers the draft activity and
/// persists none of it.
///
/// Nothing touches `statuses`, `status_mentions`, the hashtag tables or
/// the poll tables, no counter moves, no key is claimed and
/// `after_status_created` never fires. A body carrying a poll is checked
/// the way a post's is and its entity answers `poll: null` — the question
/// is previewed, never stored. The one read the preview does perform is
/// the mention lookup — [`render_status_fields`]'s own, network fetch of
/// an unknown remote account included, since the rendered content is only
/// honest if the mention links are. A body naming a `quote_id` previews
/// the same way: the in-memory row quotes nothing, the original is not
/// gated, and no `quotes_count` moves.
///
/// # Errors
///
/// Those of [`create_status`]'s checks and render: every refusal
/// [`check_body`] owes the submitted body arrives unchanged, so a preview
/// that would not post is refused exactly as the post would be.
pub async fn preview_status(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    params: &CreateStatusParams,
) -> AppResult<Status> {
    let checked = check_body(
        &state.config,
        &user.default_scope,
        user.sensitive_default.get(),
        params,
    )?;
    let found = resolve_mentions(state, checked.text).await?;
    let fields = rendered_fields(state, checked.text, checked.content_type, &found);
    let mentioned: Vec<User> = found.into_iter().map(|(_, account)| account).collect();
    let tags = preview_tags(&fields.hashtags);
    let row = preview_row(state, user, app, &checked, &fields);
    debug!(
        status = %row.id,
        user = %user.id,
        "status previewed"
    );
    Ok(Status::from_source(
        StatusSource {
            row: &row,
            author: user,
            reply_author: None,
            media: &[],
            mentions: &mentioned,
            tags: &tags,
            reactions: &[],
            card: None,
            favourited: false,
            reblogged: JsonBool(false),
            bookmarked: false,
            bookmark_folder: None,
            pinned_at: None,
            muted: JsonBool(false),
            direct_participation: None,
            quote: None,
            reblog: None,
            poll: None,
        },
        Some(user),
        None,
        &state.config,
    ))
}

/// The `tags` entries a preview renders: one per usable name
/// [`extract_hashtags`] folded, deduplicated and in the name order
/// [`crate::models::hashtag::for_statuses`] reads stored tags back in. The
/// rows are fabricated — no `hashtags` row is written for a preview — and
/// only the name the entity renders off them is meaningful.
fn preview_tags(names: &[String]) -> Vec<Hashtag> {
    let created_at = Utc::now();
    let mut unique: Vec<String> = names.to_vec();
    unique.sort_unstable();
    unique.dedup();
    unique
        .into_iter()
        .map(|name| Hashtag {
            id: new_id(),
            name,
            inserted_at: created_at,
        })
        .collect()
}

/// The in-memory row a preview renders: a fresh id with the `uri`, `url` and
/// `conversation_id` [`status::insert`] would derive from it (decision 6),
/// every column the checked body decided, zero counters, no links to any
/// other status and `now()` for both timestamps. It is the row a create of
/// this body would have stored — except that it will never be stored.
fn preview_row(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    checked: &Checked<'_>,
    fields: &RenderedFields,
) -> status::Status {
    let id = new_id();
    let now = Utc::now();
    let base = state.config.base_url.as_str();
    status::Status {
        id,
        uri: format!("{base}/objects/{id}"),
        url: Some(format!("{base}/notice/{id}")),
        user_id: user.id,
        content: fields.content.clone(),
        raw_text: checked.text.to_owned(),
        content_type: checked.content_type.to_owned(),
        spoiler_text: checked.spoiler_text.to_owned(),
        sensitive: checked.sensitive,
        visibility: checked.visibility,
        language: checked.language.map(str::to_owned),
        in_reply_to_id: None,
        in_reply_to_account_id: None,
        in_reply_to_ap_id: None,
        conversation_id: id,
        ap_context: None,
        direct_conversation_id: None,
        reblog_of_id: None,
        quote_of_id: None,
        local: true,
        application_name: app.map(|found| found.client_name.clone()),
        application_website: app.and_then(|found| found.website.clone()),
        replies_count: 0,
        reblogs_count: 0,
        favourites_count: 0,
        quotes_count: 0,
        edited_at: None,
        deleted_at: None,
        expires_at: checked.expires_at,
        inserted_at: now,
        updated_at: now,
    }
}

/// Renders one status text the way this instance stores it.
///
/// The shared render half of both status writes — [`create_status`] runs it
/// on the checked body's text, [`edit_status`] on the merged one — so an
/// edit linkifies, mention-resolves and tags a post exactly the way posting
/// the same text would. The answer is the HTML with every known
/// `@-mention` and every hashtag linked, the mentioned accounts in mention
/// order, and the hashtags as [`extract_hashtags`] folded them.
///
/// The lookup is [`resolve_mentions`], so a bare `@nick`, a
/// `@nick@our-domain` and a mirrored `@nick@their.domain` are linked from
/// the stored rows, an unknown `@nick@their.domain` is fetched over the
/// network (under that function's shared budget and per-status cap, see
/// [`crate::services::mentions`]) and linked when it answers, while what
/// stays unknown remains the plain text it was written as. Both sides of
/// the map are keyed case-folded, because the text spells the account
/// however its author typed it and the link still carries that spelling.
///
/// # Errors
///
/// Any database failure behind the mention lookups arrives as
/// [`AppError::Database`]. A remote lookup's own failures are not errors:
/// those mentions just stay plain text.
pub async fn render_status_fields(
    state: &AppState,
    text: &str,
    content_type: &str,
) -> AppResult<RenderedFields> {
    let found = resolve_mentions(state, text).await?;
    Ok(rendered_fields(state, text, content_type, &found))
}

/// The pure half of [`render_status_fields`]: the same HTML, the same
/// mentioned ids and the same tags, for mentions a caller has already
/// resolved. [`preview_status`] keeps the resolved rows beside the fields
/// (its entity renders the mentioned accounts themselves), so the split is
/// the preview's, not the create's — for `create_status` and `edit_status`
/// this is the step [`render_status_fields`] runs after its lookup.
fn rendered_fields(
    state: &AppState,
    text: &str,
    content_type: &str,
    found: &[(MentionRef, User)],
) -> RenderedFields {
    let urls: HashMap<String, String> = found
        .iter()
        .map(|(mention, account)| (mention_key(mention), profile_url(account)))
        .collect();
    let link = |mention: &MentionRef| urls.get(&mention_key(mention)).cloned();
    let html = render_content(text, content_type);
    RenderedFields {
        content: linkify_entities(&html, &link, &state.config.base_url),
        mentioned_user_ids: found.iter().map(|(_, account)| account.id).collect(),
        hashtags: extract_hashtags(text),
    }
}

/// The URL a mention links to: the stored [`profile_url`](User::profile_url)
/// that Pleroma's `Formatter.mention_from_user` prefers as `user.uri`, or the
/// actor id every row is guaranteed to carry — the same fallback the
/// [`Mention`](crate::entities::mention::Mention) entity renders.
fn profile_url(user: &User) -> String {
    user.profile_url
        .clone()
        .unwrap_or_else(|| user.ap_id.clone())
}

/// The lookup key of one mention: the account it names, case-folded — `nick`
/// for a bare reference, `nick@domain` when the text spelled the domain out.
fn mention_key(mention: &MentionRef) -> String {
    mention.acct().to_lowercase()
}

/// [`NewStatus`] of one checked body: a `local` post by `user` with its
/// rendered content and source text, its warning and flags, the two entity
/// lists [`status::insert`] links in its transaction, the attachments
/// [`status::insert`] claims on the same transaction with, the client `app`
/// recorded as the post's application — Pleroma's `put_application`, which
/// is what makes the entity's `application` block come back — and the
/// original [`quote_target`] resolved the body's `quote_id` to, `None` for a
/// post quoting nothing.
fn new_status(
    user: &User,
    app: Option<&OauthApp>,
    checked: &Checked<'_>,
    entities: &RenderedFields,
    quote_of_id: Option<Uuid>,
) -> NewStatus {
    NewStatus {
        user_id: user.id,
        content: entities.content.clone(),
        raw_text: checked.text.to_owned(),
        content_type: checked.content_type.to_owned(),
        spoiler_text: checked.spoiler_text.to_owned(),
        sensitive: checked.sensitive,
        visibility: checked.visibility,
        language: checked.language.map(str::to_owned),
        expires_at: checked.expires_at,
        mentioned_user_ids: entities.mentioned_user_ids.clone(),
        hashtags: entities.hashtags.clone(),
        media_ids: checked.media_ids.clone(),
        local: true,
        quote_of_id,
        application_name: app.map(|found| found.client_name.clone()),
        application_website: app.and_then(|found| found.website.clone()),
        ..NewStatus::default()
    }
}

/// What [`prepared_new_status`] answers: the [`NewStatus`] both create
/// arms store and the checked body's poll beside it — the question names
/// no `NewStatus` column, so it rides out separately for `store_poll`.
/// Factored out because a mixed tuple of row and poll reads better by
/// name (and a bare `Result` of it trips `type_complexity`).
type Prepared = (NewStatus, Option<CheckedPoll>);

/// The shared pre-write half of both create arms and of the schedule
/// service: [`check_body`] over the body, the reply's parent when the body
/// names one, the
/// `in_reply_to_conversation_id` participation when the body names one,
/// the quote target when the body names one,
/// the render through [`render_status_fields`], and the [`NewStatus`] to
/// store with every reply decision already applied — the parent's id,
/// author and thread, the [`inherited_visibility`] audience, the parent's
/// language where the body named none, and the parent's author and
/// mentions auto-linked after the accounts the text itself mentioned
/// (`status_mentions` only; the text is not modified and no mention span
/// is inserted) — and, for a conversation reply, the `direct` visibility
/// forced, the participation's recipients merged into the mentions, and
/// its thread recorded for the insert ([`apply_conversation_reply`]).
/// The checked body's poll comes back beside the row: it belongs to no
/// `NewStatus` column, and both create arms hand it to `store_poll` on
/// the insert's own transaction.
///
/// The parent read happens before the render, like Pleroma's `ActivityDraft`
/// pipeline, where the `in_reply_to` arm runs before `content`: a body with
/// a bad parent and a long text is answered for its parent, but only after
/// [`check_body`] has owed the body its own 422s.
///
/// # Errors
///
/// Those of [`check_body`] and [`render_status_fields`], plus
/// [`AppError::NotFound`] through [`reply_parent`] for an `in_reply_to_id`
/// naming no status the poster may see, plus [`AppError::NotFound`]
/// through [`reply_participation`] for an `in_reply_to_conversation_id`
/// naming no participation the poster owns, plus those of
/// [`quote_target`] for the body's `quote_id`, plus any database failure
/// behind the parent's mention read as [`AppError::Database`].
pub(crate) async fn prepared_new_status(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    params: &CreateStatusParams,
) -> AppResult<Prepared> {
    let checked = check_body(
        &state.config,
        &user.default_scope,
        user.sensitive_default.get(),
        params,
    )?;
    let parent = reply_parent(state, user, params).await?;
    let participation = reply_participation(state, user, params).await?;
    let quote_of_id = quote_target(state, user, params.quote_id.as_deref()).await?;
    let entities = render_status_fields(state, checked.text, checked.content_type).await?;
    let mut new = new_status(user, app, &checked, &entities, quote_of_id);
    if let Some(parent) = parent {
        let inherited = inherited_mentions(
            parent.user_id,
            &parent_mentioned_ids(state, parent.id).await?,
            user.id,
        );
        new.visibility = inherited_visibility(
            parent.visibility,
            params.visibility.is_some(),
            new.visibility,
        );
        if new.language.is_none() {
            new.language = parent.language.clone();
        }
        new.in_reply_to_id = Some(parent.id);
        new.in_reply_to_account_id = Some(parent.user_id);
        new.conversation_id = Some(parent.conversation_id);
        new.mentioned_user_ids = merged_mentions(&new.mentioned_user_ids, &inherited);
    }
    if let Some(participation) = participation {
        apply_conversation_reply(state, &participation, &mut new).await?;
    }
    Ok((new, checked.poll))
}

/// The participation the body's `in_reply_to_conversation_id` names: read
/// with the poster as owner, so a malformed id, an id that names no
/// participation and a participation of somebody else's are all one
/// indistinguishable [`AppError::NotFound`] — the route never confirms
/// that a foreign conversation id exists. Absent, null or empty — the
/// same "nothing named" spelling as [`in_reply_to_id`] — is no
/// conversation at all.
///
/// [`in_reply_to_id`]: CreateStatusParams::in_reply_to_id
async fn reply_participation(
    state: &AppState,
    poster: &User,
    params: &CreateStatusParams,
) -> AppResult<Option<conversation::Participation>> {
    let Some(text) = params
        .in_reply_to_conversation_id
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    else {
        return Ok(None);
    };
    let Some(id) = parse_id(text) else {
        return Err(AppError::NotFound);
    };
    let found = conversation::get_participation(&state.db, poster.id, id).await?;
    found.map(Some).ok_or(AppError::NotFound)
}

/// The whole `in_reply_to_conversation_id` arm on a built [`NewStatus`]:
/// the post becomes a `direct` answer into `participation`'s thread —
/// visibility forced, the row linked to the conversation for the insert
/// (Pleroma's `make_context(in_reply_to, participation)`, which routes a
/// conversation reply's context through the participation), and the
/// participation's recipients merged into the mentions after whatever the
/// text and the parent arm already carried (Pleroma's addressed-users
/// union — each account linked once, in first-mention order).
async fn apply_conversation_reply(
    state: &AppState,
    participation: &conversation::Participation,
    new: &mut NewStatus,
) -> AppResult<()> {
    new.visibility = Visibility::Direct;
    new.direct_conversation_id = Some(participation.conversation_id);
    let addressed = conversation::recipients(&state.db, participation.id).await?;
    new.mentioned_user_ids = merged_mentions(&new.mentioned_user_ids, &addressed);
    Ok(())
}

/// The status the body's `in_reply_to_id` names, read with exactly the
/// [`crate::api::helpers::status_for_viewer_or_404`] rules run against the
/// poster: a malformed, unknown, deleted or poster-invisible parent is the
/// shared [`AppError::NotFound`], so a probe through a reply never learns
/// that somebody's private post exists. A reblog row folds to the original
/// it boosts — Pleroma's `in_reply_to` arm accepts an `Announce` id and the
/// reply lands on the object under it — and the fold needs no second
/// visibility pass because the walk behind [`status_visibility::can_view`]
/// admits a reblog only when its original is admitted too. `None` when the
/// body named no parent at all, or named the empty string: Pleroma's own
/// "not a reply" spelling.
async fn reply_parent(
    state: &AppState,
    poster: &User,
    params: &CreateStatusParams,
) -> AppResult<Option<status::Status>> {
    let Some(text) = params
        .in_reply_to_id
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    else {
        return Ok(None);
    };
    let Some(id) = parse_id(text) else {
        return Err(AppError::NotFound);
    };
    let Some(row) = status::get_by_id(&state.db, id).await? else {
        return Err(AppError::NotFound);
    };
    if !status_visibility::can_view(&state.db, &row, Some(poster)).await? {
        return Err(AppError::NotFound);
    }
    let Some(original_id) = row.reblog_of_id else {
        return Ok(Some(row));
    };
    let original = status::get_by_id(&state.db, original_id).await?;
    original.map(Some).ok_or(AppError::NotFound)
}

/// The accounts the parent itself @-mentions, in mention order: the set
/// Pleroma's `get_to_and_cc` carries into a reply's addressing, which here
/// becomes the extra `status_mentions` links the reply stores.
async fn parent_mentioned_ids(state: &AppState, parent_id: Uuid) -> AppResult<Vec<Uuid>> {
    let links = mention::for_statuses(&state.db, &[parent_id]).await?;
    Ok(links.get(&parent_id).map_or_else(Vec::new, |users| {
        users.iter().map(|account| account.id).collect()
    }))
}

/// The accounts a reply auto-mentions off its parent: its author first, then
/// the accounts the parent mentioned — the replier never (nobody mentions
/// themselves into their own reply), every account once, at most
/// [`MAX_INHERITED_MENTIONS`] in all. These are link rows only: the status
/// text is not modified and no mention span is inserted.
fn inherited_mentions(parent_author: Uuid, parent_mentions: &[Uuid], replier: Uuid) -> Vec<Uuid> {
    let mut inherited: Vec<Uuid> = Vec::new();
    for id in std::iter::once(parent_author).chain(parent_mentions.iter().copied()) {
        if id != replier && !inherited.contains(&id) {
            inherited.push(id);
        }
    }
    inherited.truncate(MAX_INHERITED_MENTIONS);
    inherited
}

/// The body's own mentions in text order with the auto-mentions appended:
/// an account the text already mentioned keeps its place and is not linked
/// a second time.
fn merged_mentions(own: &[Uuid], inherited: &[Uuid]) -> Vec<Uuid> {
    let mut merged: Vec<Uuid> = own.to_vec();
    for &id in inherited {
        if !merged.contains(&id) {
            merged.push(id);
        }
    }
    merged
}

/// The audience a reply lands in, from the parent's visibility, whether the
/// body *submitted* a `visibility` of its own, and what [`check_body`]
/// decided (the submitted name, or the author's default scope):
///
/// - a `direct` parent forces `direct`, whoever the body asked for;
/// - under a `private` parent, a submitted `public` or `unlisted` comes down
///   to `private`; every other submitted scope stands as asked — Pleroma's
///   own rules keep a submitted `public` under an `unlisted` parent, and
///   `check_body` has already refused any submitted name outside the five;
/// - a body that submitted nothing posts to its author's default scope, but
///   never wider than the parent it answers.
fn inherited_visibility(parent: Visibility, submitted: bool, chosen: Visibility) -> Visibility {
    if parent == Visibility::Direct {
        return Visibility::Direct;
    }
    if !submitted {
        return narrower_visibility(chosen, parent);
    }
    if parent == Visibility::Private && matches!(chosen, Visibility::Public | Visibility::Unlisted)
    {
        return Visibility::Private;
    }
    chosen
}

/// The narrower of two visibilities, by [`visibility_width`].
const fn narrower_visibility(chosen: Visibility, cap: Visibility) -> Visibility {
    if visibility_width(chosen) <= visibility_width(cap) {
        chosen
    } else {
        cap
    }
}

/// How wide a visibility's audience is, for the reply cap: `direct` reaches
/// the fewest eyes and `public` the most. `local` sits between the federated
/// scopes and `private` — everybody on this instance, nobody off it.
const fn visibility_width(visibility: Visibility) -> u8 {
    match visibility {
        Visibility::Direct => 0,
        Visibility::Private => 1,
        Visibility::Local => 2,
        Visibility::Unlisted => 3,
        Visibility::Public => 4,
    }
}

/// The post-write hook of [`create_status`]: the tickets that let a fresh
/// local status out of the database it was just written to — the mention
/// notifications for the mentioned accounts and the replied-to author
/// (landed from [`notify::on_mention`], Pleroma's `Create` notification
/// arm), the subscription `status` notifications for the local users who
/// subscribed to the author (landed from [`notify::on_subscribed_status`],
/// Pleroma's subscriber arm of the same create), the streaming `update` to
/// every timeline the new post belongs to (landed from
/// [`streaming::status_created`], Pleroma's `stream_out`), the
/// [`rich_media.fetch`](crate::jobs::rich_media) card job the post's first
/// link schedules for itself (see
/// [`schedule_fetch`](crate::jobs::rich_media::schedule_fetch)), and the
/// federation of the `Create` activity through the job queue (landed from
/// [`federation_out::after_status_created`], decision 11's queued fan-out)
/// — wire themselves in here, and only here.
///
/// `status` is the stored row: the id the tellings name, the author they
/// do not tell, the `visibility` that keeps a `direct` post out of the
/// subscriber arm, and the `reblog_of_id` that keeps a boost row out of
/// both. The stream push names the row by render hint rather than shipping
/// the rendered entity, so the streaming consumer needed no growth of this
/// one call's signature; the federation arm renders its own document from
/// the row, through [`crate::federation::note`].
///
/// [`notify::on_mention`]: crate::services::notify::on_mention
/// [`streaming::status_created`]: crate::services::streaming::status_created
/// [`federation_out::after_status_created`]: crate::services::federation_out::after_status_created
///
/// `pub(crate)`, not private: the federation ingest path
/// ([`crate::services::ingest`] step 9) runs a freshly mirrored remote
/// status through exactly this hook once its write commits — a new post is
/// a new post for the notifications, cards and conversation arms whoever
/// authored it, and every arm here already decides on the *recipient's*
/// locality, never the author's.
pub(crate) async fn after_status_created(state: &AppState, status: &status::Status) {
    notify::on_mention(state, status).await;
    notify::on_subscribed_status(state, status).await;
    streaming::status_created(state, status).await;
    crate::jobs::rich_media::schedule_fetch(state, status).await;
    participate_in_conversation(state, status).await;
    federation_out::after_status_created(state, status).await;
}

/// The conversation arm of [`after_status_created`]: a `direct` status is
/// recorded in its DM thread — the thread row found-or-created, the
/// status linked to it, every local participant's side bumped and every
/// addressee (remote included) stored on the recipient lists — through
/// [`conversation::participate_for_status`], which reads the roster off
/// the status's stored mention links. A status of any other visibility
/// does nothing.
///
/// Once that write commits, the thread's local participants are told: each
/// is pushed a [`streaming::conversation_updated`] on their own `direct`
/// stream, carrying their OWN participation id as the render hint — the
/// roster [`conversation::participations_for_status`] reads back. A status
/// of any other visibility threads nothing and so publishes nothing; a
/// remote participant owns no participation row and is simply absent.
///
/// Best-effort beside the rest of this hook: the post itself is already
/// committed, so a participation that fails to land, a roster that fails to
/// read, or a publish that has nothing to name are all just log lines — the
/// status stands, and its read view simply lacks the thread or the push.
async fn participate_in_conversation(state: &AppState, status: &status::Status) {
    if let Err(err) = conversation::participate_for_status(&state.db, status).await {
        warn!(
            status = %status.id,
            %err,
            "could not record the conversation participations of a status"
        );
        return;
    }
    let participants = match conversation::participations_for_status(&state.db, status.id).await {
        Ok(participants) => participants,
        Err(err) => {
            warn!(
                status = %status.id,
                %err,
                "could not read the conversation participants to publish"
            );
            return;
        }
    };
    for participation in participants {
        streaming::conversation_updated(state, participation.user_id, participation.id);
    }
}

/// The body of `PUT /api/v1/statuses/{id}`, as the client wrote it.
///
/// The five text parameters mirror [`CreateStatusParams`]'s without
/// `visibility` — an edit never moves a post to a different audience
/// (Pleroma's `make_update_data` carries the stored visibility forward) —
/// and every one is optional: only the parameters the body presents change
/// the post ([`edit_status`]). `media_ids` is decision 10's id-array —
/// JSON's `["a","b"]` or the form's `media_ids[]=a&media_ids[]=b`, both
/// read as a list of id text — and `sensitive` the same
/// [`flex`] boolean the create body accepts.
///
/// [`flex`]: crate::extract::flex
#[derive(Debug, Default, Deserialize)]
pub struct EditStatusParams {
    /// The new post text; trimmed and capped the way creation caps it.
    #[serde(default)]
    status: Option<String>,
    /// The new content warning, stored verbatim; at most
    /// `MAX_SPOILER_CHARS` characters.
    #[serde(default)]
    spoiler_text: Option<String>,
    /// Whether the post starts folded.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    sensitive: Option<bool>,
    /// The new content language tag; empty clears it.
    #[serde(default)]
    language: Option<String>,
    /// The `MIME` type [`status`](Self::status) was written in.
    #[serde(default)]
    content_type: Option<String>,
    /// The post's whole new attachment set, as id text; ids that name no
    /// attachment the author may attach drop out — see [`edit_status`].
    #[serde(default)]
    media_ids: Option<Vec<String>>,
}

/// The edit body merged over the stored row: every [`CreateStatusParams`]
/// field carrying the submitted value where the body presented one and the
/// row's own value where it did not. The merged body is what [`check_body`]
/// reads, so the create path's checks run on the post as it will stand —
/// and a field nobody edited can never fail a check it already passed.
fn merged_body(row: &status::Status, params: &EditStatusParams) -> CreateStatusParams {
    CreateStatusParams {
        status: kept(params.status.clone(), || row.raw_text.clone()),
        spoiler_text: kept(params.spoiler_text.clone(), || row.spoiler_text.clone()),
        sensitive: Some(params.sensitive.unwrap_or(row.sensitive)),
        visibility: Some(row.visibility.as_str().to_owned()),
        language: params.language.clone().or_else(|| row.language.clone()),
        content_type: kept(params.content_type.clone(), || row.content_type.clone()),
        // An edit is never a preview, never a schedule and never replays:
        // the create's decision parameters have no part in this merge. An
        // edit never moves the expiry either — `expires_in` is a creation
        // parameter, the stored `expires_at` rides along untouched
        // (Pleroma's `make_update_data` carries it forward the same way)
        // — and never its parent: an edit rewrites what was said in the
        // thread it stands in, and `in_reply_to_id` is not an edit
        // parameter. Neither is a poll: a post's question is fixed when it
        // is asked, and neither is a quote: `quote_id` is a creation
        // parameter, the stored `quote_of_id` rides along untouched. The
        // edit's own `media_ids` ride [`status_edit::EditParams`] through
        // [`crate::models::status_edit::update_status`]'s replacement arm,
        // which drops what the author may not attach instead of refusing
        // it, so the merge carries none and [`check_body`] never sees the
        // create's attachment rules.
        quote_id: None,
        preview: None,
        expires_in: None,
        in_reply_to_id: None,
        in_reply_to_conversation_id: None,
        poll: None,
        media_ids: None,
        scheduled_at: None,
    }
}

/// One optional edit parameter against the value the stored row keeps in
/// its place: the submitted one wins outright — even the empty spellings
/// that mean "clear this field" — and only its absence asks for `stored`.
fn kept<T, F: FnOnce() -> T>(submitted: Option<T>, stored: F) -> Option<T> {
    submitted.or_else(|| Some(stored()))
}

/// Edits the status `row` as `params` describes and answers the [`Status`]
/// entity as `user` — its author, which is the endpoint's precondition for
/// reaching this function at all — sees the post after the write.
///
/// `merged_body` resolves which parameters the body presented,
/// [`check_body`] refuses the merge exactly as it refuses a creation
/// (422 `Cannot post an empty status without attachments`,
/// `The status is over the character limit`, the spoiler and language
/// length wordings, `Invalid content_type`), and [`render_status_fields`]
/// renders the merged text into the content, mention list and tag list
/// creation would have written for it. The whole new state then travels to
/// [`status_edit::update_status`], whose one transaction records the
/// superseded version (and the `edited_at` stamp) only when content,
/// warning, sensitive flag or media actually changed, and replaces the
/// post's mentions, hashtags and media.
///
/// Of the edit's own `media_ids`, only the ids the author may attach to
/// this post survive to the write: rows nobody owns, rows another status
/// claims and text that is no id at all name nothing and drop out, as
/// Pleroma's `CommonAPI.Utils.attachments_from_ids` rejects its nils.
///
/// `None` is the row a delete took between the endpoint's gate and this
/// write; the endpoint answers that its shared 404.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for the merged body's refusals, through
/// [`check_body`]; the database failures of the write and of the render
/// arrive through `?` as [`AppError::Database`].
pub async fn edit_status(
    state: &AppState,
    user: &User,
    row: &status::Status,
    params: EditStatusParams,
) -> AppResult<Option<Status>> {
    let merged = merged_body(row, &params);
    let checked = check_body(
        &state.config,
        row.visibility.as_str(),
        row.sensitive,
        &merged,
    )?;
    let fields = render_status_fields(state, checked.text, checked.content_type).await?;
    let media_ids = params
        .media_ids
        .map(|ids| ids.iter().filter_map(|id| parse_id(id)).collect());
    let edit = status_edit::EditParams {
        content: fields.content,
        raw_text: checked.text.to_owned(),
        content_type: checked.content_type.to_owned(),
        spoiler_text: checked.spoiler_text.to_owned(),
        sensitive: checked.sensitive,
        language: checked.language.map(str::to_owned),
        mentioned_user_ids: fields.mentioned_user_ids,
        hashtags: fields.hashtags,
        media_ids,
    };
    let Some(updated) = status_edit::update_status(&state.db, row.id, edit).await? else {
        return Ok(None);
    };
    debug!(
        status = %updated.id,
        user = %user.id,
        edited = updated.edited_at.is_some(),
        "status edited"
    );
    render_status(state, ViewerMode::user(user), &updated).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Router,
        extract::{Path, Query, State},
        http::{StatusCode, header::CONTENT_TYPE},
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        http_client::HttpClient,
        mailer::MemoryTransport,
        models::{
            media_attachment::{self, NewMediaAttachment},
            user::{NewUser, insert as insert_user, local_ap_id},
        },
        uploads::UploaderBackend,
        util::entities::{hashtag_html, mention_html},
    };

    /// The instance origin the fixtures post under.
    const BASE: &str = TEST_BASE_URL;

    /// The state under test: `pool` and the test config, with the
    /// collaborators the service never touches.
    fn state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(MemoryTransport::new()),
        }
    }

    /// A local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the fixture user inserts")
    }

    /// A body carrying `text` as its only parameter.
    fn posted(text: &str) -> CreateStatusParams {
        CreateStatusParams {
            status: Some(text.to_owned()),
            ..CreateStatusParams::default()
        }
    }

    /// A body posting `text` beside the minimum two-option poll, for the
    /// persistence and key-race tests.
    fn polled(text: &str) -> CreateStatusParams {
        CreateStatusParams {
            poll: Some(PollRequest {
                options: vec!["a".to_owned(), "b".to_owned()],
                expires_in: None,
                multiple: None,
                hide_totals: None,
            }),
            ..posted(text)
        }
    }

    /// The status and the `{"error": …}` body one refusal renders: what the
    /// client sees, read off the response rather than the variant.
    async fn wire(err: AppError) -> (StatusCode, Value) {
        let response: Response = err.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1_024)
            .await
            .expect("an error body is collectable");
        (
            status,
            serde_json::from_slice(&bytes).expect("errors are JSON"),
        )
    }

    /// The one body every validation refusal owes: 422, `{"error": message}`.
    fn refused(answer: &(StatusCode, Value), message: &str) {
        assert_eq!(
            answer.0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "the status a refusal answers with"
        );
        assert_eq!(
            answer.1,
            json!({ "error": message }),
            "the body that refusal carries"
        );
    }

    /// A plain post through the service, as `author` with no application.
    async fn created(state: &AppState, author: &User, params: CreateStatusParams) -> Status {
        create_status(state, author, None, params)
            .await
            .expect("the post creates")
    }

    /// The stored row of a rendered entity, live and unabridged.
    async fn stored(pool: &PgPool, entity: &Status) -> status::Status {
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the created status is stored live")
    }

    /// The parameters a JSON body deserializes to: the same
    /// `into_params::<CreateStatusParams>` call an [`ApiBody`] makes, so these
    /// tests see a body exactly as the endpoint will.
    ///
    /// [`ApiBody`]: crate::extract::ApiBody
    fn params(value: Value) -> CreateStatusParams {
        crate::extract::into_params::<CreateStatusParams>(value)
            .expect("every body in these tests fits the parameter type")
    }

    /// The reading of `body` by [`check_body`] under the stock test
    /// configuration and an author who defaults to `public` and to not
    /// marking posts sensitive.
    fn accepted(body: &CreateStatusParams) -> Checked<'_> {
        check_body(&Config::for_tests(), "public", false, body)
            .unwrap_or_else(|err| panic!("this body should be accepted: {err}"))
    }

    /// The 422 message [`check_body`] answers for `body` under the same
    /// configuration and defaults.
    fn rejection(body: &CreateStatusParams) -> String {
        match check_body(&Config::for_tests(), "public", false, body) {
            Err(err) => err.to_string(),
            Ok(checked) => panic!("this body should be refused, got `{}`", checked.text),
        }
    }

    #[test]
    fn a_plain_post_is_read_with_the_author_defaults() {
        let body = params(json!({"status": "  hi  "}));
        let checked = accepted(&body);
        assert_eq!(checked.text, "hi", "the text is trimmed");
        assert_eq!(checked.spoiler_text, "", "no warning means no text");
        assert!(!checked.sensitive, "absent takes the author's `false`");
        assert_eq!(checked.visibility, Visibility::Public, "and their scope");
        assert_eq!(checked.language, None, "and no language at all");
        assert_eq!(
            checked.content_type, DEFAULT_CONTENT_TYPE,
            "and the default content type"
        );
        assert!(
            CONTENT_TYPES.contains(&DEFAULT_CONTENT_TYPE),
            "which is one of the advertised types, so the fallback can never \
             be a value `checked_content_type` would refuse"
        );
    }

    #[test]
    fn the_author_defaults_answer_what_the_body_leaves_out() {
        let body = params(json!({"status": "hi"}));
        let checked = check_body(&Config::for_tests(), "local", true, &body)
            .unwrap_or_else(|err| panic!("an author default is always usable: {err}"));
        assert_eq!(
            checked.visibility,
            Visibility::Local,
            "Pleroma's extra scope comes from the author's row too"
        );
        assert!(checked.sensitive, "and so does the folded flag");
    }

    #[test]
    fn what_the_body_sends_wins_over_the_author_defaults() {
        let body = params(json!({
            "status": "hi",
            "visibility": "direct",
            "sensitive": false,
            "language": "en",
            "spoiler_text": "spoilers",
            "content_type": "text/html",
        }));
        // The defaults of `accepted` are `public` / not-sensitive, so every
        // one of these has to have come from the body.
        let checked = accepted(&body);
        assert_eq!(checked.visibility, Visibility::Direct, "submitted scope");
        assert!(!checked.sensitive, "a submitted `false` is not absent");
        assert_eq!(checked.language, Some("en"), "submitted tag");
        assert_eq!(checked.spoiler_text, "spoilers", "submitted warning");
        assert_eq!(checked.content_type, "text/html", "submitted type");
    }

    #[test]
    fn a_sensitive_flag_accepts_the_form_spellings() {
        let on = params(json!({"status": "hi", "sensitive": "on"}));
        assert!(accepted(&on).sensitive, "`\"on\"` means true");
        let zero = params(json!({"status": "hi", "sensitive": "0"}));
        assert!(!accepted(&zero).sensitive, "`\"0\"` means false");
        let null = params(json!({"status": "hi", "sensitive": null}));
        assert!(!accepted(&null).sensitive, "a null is as absent as no key");
    }

    #[test]
    fn a_quote_id_accepts_both_spellings_and_its_absence() {
        let id = "01934567-89ab-7cde-f012-3456789abcde";
        let quoted = params(json!({"status": "hi", "quote_id": id}));
        assert_eq!(
            quoted.quote_id.as_deref(),
            Some(id),
            "the Mastodon spelling arrives"
        );
        let alias = params(json!({"status": "hi", "quoted_status_id": id}));
        assert_eq!(
            alias.quote_id.as_deref(),
            Some(id),
            "and so does Pleroma's older alias, into the same field"
        );
        for value in [
            json!({"status": "hi"}),
            json!({"status": "hi", "quote_id": null}),
        ] {
            let body = params(value);
            assert!(
                body.quote_id.is_none(),
                "an absent or null key is no quote at all"
            );
        }
    }

    #[test]
    fn a_status_with_nothing_to_say_is_refused() {
        for value in [
            json!({}),
            json!({"status": null}),
            json!({"status": ""}),
            json!({"status": "   \n\t "}),
        ] {
            let body = params(value);
            assert_eq!(
                rejection(&body),
                EMPTY_STATUS,
                "a status with no text and no attachments is Pleroma's message"
            );
        }
    }

    #[test]
    fn a_status_over_the_character_limit_is_refused() {
        let limit = Config::for_tests().max_status_chars;
        let exact = params(json!({"status": "x".repeat(limit)}));
        assert_eq!(
            accepted(&exact).text.chars().count(),
            limit,
            "exactly the limit is still within it"
        );
        let over = params(json!({"status": "x".repeat(limit + 1)}));
        assert_eq!(
            rejection(&over),
            OVER_LIMIT,
            "and one character more is the over-limit message"
        );
    }

    #[test]
    fn the_limit_counts_a_bare_url_as_twenty_three_characters() {
        // 49 characters as written, 23 as a client counts them: over a
        // 40-character limit by length, under it by weighting.
        let url = "https://example.com/a/very/long/path?query=string";
        assert_eq!(
            status_length(url, URL_WEIGHT),
            URL_WEIGHT,
            "one bare URL costs exactly `URL_WEIGHT`, which is what this \
             module pins as Mastodon's `charactersReservedPerUrl`"
        );
        let mut config = Config::for_tests();
        config.max_status_chars = 40;
        let body = params(json!({"status": url}));
        let checked = check_body(&config, "public", false, &body)
            .unwrap_or_else(|_| panic!("a 49-character URL weighs {URL_WEIGHT} here"));
        assert_eq!(checked.text, url, "and the URL is stored verbatim");

        let plain = params(json!({"status": "x".repeat(41)}));
        let refused = check_body(&config, "public", false, &plain)
            .err()
            .unwrap_or_else(|| panic!("41 plain characters do not fit a 40-character limit"));
        assert_eq!(
            refused.to_string(),
            OVER_LIMIT,
            "which is the same refusal the weighting saves the URL from"
        );
    }

    #[test]
    fn a_content_warning_is_capped_at_two_hundred_characters() {
        let edge = "x".repeat(MAX_SPOILER_CHARS);
        let body = params(json!({"status": "hi", "spoiler_text": edge}));
        assert_eq!(
            accepted(&body).spoiler_text,
            edge,
            "the cap itself fits, and is stored verbatim"
        );
        let padded = params(json!({"status": "hi", "spoiler_text": "  cats  "}));
        assert_eq!(
            accepted(&padded).spoiler_text,
            "  cats  ",
            "a warning is never trimmed, only the text is"
        );
        let over = params(json!({
            "status": "hi",
            "spoiler_text": "x".repeat(MAX_SPOILER_CHARS + 1),
        }));
        assert_eq!(
            rejection(&over),
            "Validation failed: Spoiler text is too long (maximum is 200 characters)",
            "one character past the cap is refused in Rails' own wording"
        );
    }

    #[test]
    fn a_language_is_capped_at_ten_characters_and_emptied_by_nothing() {
        let edge = params(json!({
            "status": "hi",
            "language": "a".repeat(MAX_LANGUAGE_CHARS),
        }));
        assert_eq!(
            accepted(&edge).language,
            Some("aaaaaaaaaa"),
            "the cap itself fits"
        );
        for value in [
            json!({"status": "hi", "language": ""}),
            json!({"status": "hi", "language": null}),
            json!({"status": "hi"}),
        ] {
            let body = params(value);
            assert_eq!(
                accepted(&body).language,
                None,
                "an empty, null or absent tag is no tag at all"
            );
        }
        let over = params(json!({
            "status": "hi",
            "language": "a".repeat(MAX_LANGUAGE_CHARS + 1),
        }));
        assert_eq!(
            rejection(&over),
            "Validation failed: Language is too long (maximum is 10 characters)",
            "eleven characters are one too many, in the same wording"
        );
    }

    #[test]
    fn the_two_caps_count_characters_not_bytes() {
        let warning = "…".repeat(MAX_SPOILER_CHARS);
        let body = params(json!({"status": "hi", "spoiler_text": warning}));
        assert_eq!(
            accepted(&body).spoiler_text,
            warning,
            "200 three-byte characters are 600 bytes and still fit"
        );
        let tag = "日本語言語タグ";
        let tagged = params(json!({"status": "hi", "language": tag}));
        assert_eq!(
            accepted(&tagged).language,
            Some(tag),
            "and so do 7 of them in a 10-character tag"
        );
    }

    #[test]
    fn every_known_visibility_name_is_accepted_as_itself() {
        for name in Visibility::ALL {
            let body = params(json!({
                "status": "hi",
                "visibility": name.as_str(),
            }));
            assert_eq!(
                accepted(&body).visibility,
                name,
                "`{name}` is its own audience, whatever the author defaults to"
            );
        }
    }

    #[test]
    fn an_unknown_submitted_visibility_is_refused() {
        for name in ["", "bogus", "Public", "published", " private"] {
            let body = params(json!({"status": "hi", "visibility": name}));
            assert_eq!(
                rejection(&body),
                INVALID_VISIBILITY,
                "`{name}` is not one of the five, and parsing them is \
                 case-sensitive"
            );
        }
    }

    #[test]
    fn an_unusable_stored_scope_falls_back_to_public() {
        for stored in ["", "bogus", "Public"] {
            let body = params(json!({"status": "hi"}));
            let checked = check_body(&Config::for_tests(), stored, false, &body)
                .unwrap_or_else(|err| panic!("a broken default is not the poster's fault: {err}"));
            assert_eq!(
                checked.visibility,
                Visibility::Public,
                "a row holding `{stored}` still posts publicly"
            );
        }
    }

    #[test]
    fn the_advertised_content_types_pass_and_every_other_is_refused() {
        for name in CONTENT_TYPES {
            let body = params(json!({"status": "hi", "content_type": name}));
            assert_eq!(
                accepted(&body).content_type,
                name,
                "`{name}` is one of the formats the instance advertises"
            );
        }
        let null = params(json!({"status": "hi", "content_type": null}));
        assert_eq!(
            accepted(&null).content_type,
            DEFAULT_CONTENT_TYPE,
            "a null type is as absent as no key"
        );
        for name in [
            "text/x-markdown",
            "TEXT/PLAIN",
            "text/plain ",
            "markdown",
            "",
        ] {
            let body = params(json!({"status": "hi", "content_type": name}));
            assert_eq!(
                rejection(&body),
                INVALID_CONTENT_TYPE,
                "`{name}` is not on the list: the match is exact and \
                 case-sensitive, so even Pleroma's own `text/x-markdown` \
                 alias is refused"
            );
        }
    }

    #[tokio::test]
    async fn a_plain_post_stores_every_column_the_body_decided() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "alice").await;

        let entity = created(
            &app_state,
            &alice,
            CreateStatusParams {
                spoiler_text: Some("cats".to_owned()),
                sensitive: Some(true),
                visibility: Some("local".to_owned()),
                language: Some("eo".to_owned()),
                content_type: Some("text/markdown".to_owned()),
                ..posted("  hello **world**  ")
            },
        )
        .await;
        let row = stored(pool, &entity).await;

        assert_eq!(row.user_id, alice.id, "the author is the row's");
        assert!(row.local, "a posted status is a local one");
        assert_eq!(row.uri, format!("{BASE}/objects/{}", row.id));
        assert_eq!(
            row.url,
            Some(format!("{BASE}/notice/{}", row.id)),
            "and the human URL derives from the same id"
        );
        assert_eq!(
            row.raw_text, "hello **world**",
            "the source text is the trimmed one"
        );
        assert_eq!(
            row.content, "<p>hello <strong>world</strong></p>",
            "the content is the format rendered"
        );
        assert_eq!(row.content_type, "text/markdown");
        assert_eq!(row.spoiler_text, "cats", "the warning is stored");
        assert!(row.sensitive, "so is the flag");
        assert_eq!(row.visibility, Visibility::Local, "and the scope");
        assert_eq!(row.language.as_deref(), Some("eo"), "and the tag");
        assert_eq!(row.conversation_id, row.id, "a root post is its own thread");
        assert!(
            row.application_name.is_none() && row.application_website.is_none(),
            "no application is named, so the block stays empty"
        );

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(value["id"], row.id.to_string(), "the entity is the row");
        assert_eq!(value["uri"], row.uri, "with its object id");
        assert_eq!(value["url"], json!(row.url), "and its human URL");
        assert_eq!(value["content"], row.content, "and the render");
        assert_eq!(value["visibility"], "local", "as stored, verbatim");
        assert_eq!(value["sensitive"], true);
        assert_eq!(value["spoiler_text"], "cats");
        assert_eq!(value["language"], "eo");
        assert_eq!(value["application"], Value::Null);
        assert_eq!(value["account"]["acct"], "alice", "rendered as the author");
        assert_eq!(
            value["pleroma"]["content"],
            json!({ "text/plain": "hello **world**" }),
            "the source text answers in the Pleroma block"
        );

        let counted = crate::models::user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(
            counted.statuses_count, 1,
            "the author counts the post they made"
        );
    }

    #[tokio::test]
    async fn mentions_and_hashtags_are_linked_stored_and_answered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "linker").await;
        let bob = user(pool, "bob").await;

        let entity = created(
            &app_state,
            &alice,
            posted("hi @BOB and @nobody, see #Rust #rust #123"),
        )
        .await;
        let row = stored(pool, &entity).await;

        assert_eq!(
            entity.content,
            format!(
                "<p>hi {} and @nobody, see {} {} #123</p>",
                mention_html(&bob.ap_id, "BOB"),
                hashtag_html(BASE, "rust"),
                hashtag_html(BASE, "rust"),
            ),
            "the known mention becomes its h-card in the written case, the \
             unknown one stays text, the tag links in both spellings and the \
             digits-only one is no tag"
        );
        assert_eq!(
            row.raw_text, "hi @BOB and @nobody, see #Rust #rust #123",
            "the source text keeps what was typed"
        );

        let mentioned: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
            "SELECT user_id FROM status_mentions WHERE status_id = $1 ORDER BY position",
        )
        .bind(row.id)
        .fetch_all(pool)
        .await
        .expect("the mention links read")
        .into_iter()
        .map(|link| link.0)
        .collect();
        assert_eq!(
            mentioned,
            vec![bob.id],
            "only the account this instance knows is linked"
        );

        let tagged: Vec<String> = sqlx::query_as::<_, (String,)>(
            "SELECT h.name FROM status_hashtags sh JOIN hashtags h ON h.id = sh.hashtag_id \
              WHERE sh.status_id = $1 ORDER BY h.name",
        )
        .bind(row.id)
        .fetch_all(pool)
        .await
        .expect("the tag links read")
        .into_iter()
        .map(|link| link.0)
        .collect();
        assert_eq!(tagged, vec!["rust"], "the tag is stored once, folded");

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "bob",
                     "url": bob.ap_id, "acct": "bob" }]),
            "the mentioned account answers in the entity"
        );
        assert_eq!(
            value["tags"],
            json!([{ "name": "rust", "url": format!("{BASE}/tags/rust") }]),
            "and so does the tag"
        );
    }

    /// The mock remote peer's `WebFinger` answer: any `acct:` handle gets a
    /// JRD whose self link points at this peer's own `/users/{nick}`.
    async fn mock_webfinger(
        State(base): State<String>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Response {
        let nick = params
            .get("resource")
            .and_then(|resource| resource.strip_prefix("acct:"))
            .and_then(|handle| handle.split_once('@').map(|(nick, _)| nick))
            .unwrap_or("alice");
        let jrd = json!({
            "subject": format!("acct:{nick}@mock.example"),
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": format!("{base}/users/{nick}"),
            }],
        });
        ([(CONTENT_TYPE, "application/jrd+json")], jrd.to_string()).into_response()
    }

    /// The mock remote peer's actor document: self-identifying, named for
    /// the path segment.
    async fn mock_actor(State(base): State<String>, Path(nick): Path<String>) -> Response {
        let id = format!("{base}/users/{nick}");
        let document = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Person",
            "id": id,
            "preferredUsername": nick,
            "name": "Mock",
            "inbox": format!("{id}/inbox"),
            "publicKey": {
                "id": format!("{id}#main-key"),
                "owner": id,
                "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
            },
        });
        (
            [(CONTENT_TYPE, "application/activity+json")],
            document.to_string(),
        )
            .into_response()
    }

    /// Serve the mock remote peer; answer with its origin and the task to
    /// abort when the test is done.
    async fn mock_peer() -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/.well-known/webfinger", get(mock_webfinger))
            .route("/users/{nick}", get(mock_actor))
            .with_state(base.clone());
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock peer stopped: {err}");
            }
        });
        (base, server)
    }

    /// A remote mention fetched from the mock peer is linked into
    /// `status_mentions` and linkified in the HTML with its remote profile
    /// URL. The fetch itself (`WebFinger` + actor, budget and cap) is
    /// [`resolve_mentions`]'s tested behaviour; the loopback mock answers
    /// on `127.0.0.1`, so the posted spelling is the mirror's own handle
    /// (decision 7) and the mention resolves from the row the fetch stored.
    #[tokio::test]
    async fn a_resolved_remote_mention_is_stored_and_rendered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "remote-author").await;
        let (base, server) = mock_peer().await;

        let fetched = crate::services::mentions::resolve_mentions_with_base(
            &app_state,
            "hi @alice@mock.example",
            Some(&base),
        )
        .await
        .expect("the mock peer resolves")
        .pop()
        .expect("the remote mention resolved");
        server.abort();
        let mirror_acct = format!("alice@{}", base.trim_start_matches("http://"));
        assert_eq!(fetched.1.nickname, mirror_acct, "mirror handle");

        let entity = created(
            &app_state,
            &alice,
            posted(&format!("hi @{}!", fetched.1.nickname)),
        )
        .await;
        let row = stored(pool, &entity).await;

        assert_eq!(
            entity.content,
            format!("<p>hi {}!</p>", mention_html(&fetched.1.ap_id, "alice")),
            "the remote mention is linkified to its actor URL"
        );
        let mentioned: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
            "SELECT user_id FROM status_mentions WHERE status_id = $1 ORDER BY position",
        )
        .bind(row.id)
        .fetch_all(pool)
        .await
        .expect("the mention links read")
        .into_iter()
        .map(|link| link.0)
        .collect();
        assert_eq!(
            mentioned,
            vec![fetched.1.id],
            "the remote mirror is linked in status_mentions"
        );
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["mentions"],
            json!([{ "id": fetched.1.id.to_string(), "username": "alice",
                     "url": fetched.1.ap_id, "acct": mirror_acct }]),
            "and answered in the entity"
        );
    }

    /// A mention of a domain that cannot be reached at all (`.invalid`
    /// never resolves) leaves the mention as plain text, resolves the
    /// known mentions beside it, and the status is created as always.
    #[tokio::test]
    async fn a_status_mentioning_an_unreachable_domain_still_creates() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "patient-author").await;
        let bob = user(pool, "known-bob").await;

        let entity = created(
            &app_state,
            &alice,
            posted("hi @ghost@nowhere.invalid and @known-bob"),
        )
        .await;

        assert_eq!(
            entity.content,
            format!(
                "<p>hi @ghost@nowhere.invalid and {}</p>",
                mention_html(&bob.ap_id, "known-bob")
            ),
            "the unreachable mention stays exactly as written"
        );
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "known-bob",
                     "url": bob.ap_id, "acct": "known-bob" }]),
            "only the reachable account is answered"
        );
    }

    #[tokio::test]
    async fn a_bare_post_links_nothing_and_stores_no_entities() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "plain").await;

        let entity = created(
            &app_state,
            &alice,
            posted("just text, with an email a@b.c and a #123"),
        )
        .await;
        let row = stored(pool, &entity).await;
        let value = serde_json::to_value(&entity).expect("the entity serializes");

        assert_eq!(
            value["content"], "<p>just text, with an email a@b.c and a #123</p>",
            "nothing in this text is an entity"
        );
        assert_eq!(value["mentions"], json!([]), "no mention, no entry");
        assert_eq!(value["tags"], json!([]), "no tag, no entry");
        let mentions: (i64,) =
            sqlx::query_as("SELECT count(*) FROM status_mentions WHERE status_id = $1")
                .bind(row.id)
                .fetch_one(pool)
                .await
                .expect("the mention links read");
        let tags: (i64,) =
            sqlx::query_as("SELECT count(*) FROM status_hashtags WHERE status_id = $1")
                .bind(row.id)
                .fetch_one(pool)
                .await
                .expect("the tag links read");
        assert_eq!(
            (mentions.0, tags.0),
            (0, 0),
            "and neither link table gained a row"
        );
    }

    #[tokio::test]
    async fn the_application_of_the_token_is_recorded_and_rendered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "client-author").await;
        let app = OauthApp {
            id: Uuid::now_v7(),
            client_name: "Wired Client".to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: "write:statuses".to_owned(),
            website: Some("https://client.example".to_owned()),
            client_id: "client-id".to_owned(),
            client_secret: "client-secret".to_owned(),
            trusted: false,
            user_id: Some(alice.id),
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let entity = create_status(&app_state, &alice, Some(&app), posted("from a client"))
            .await
            .expect("the post creates");
        let row = stored(pool, &entity).await;
        assert_eq!(
            row.application_name.as_deref(),
            Some("Wired Client"),
            "the client name is stored"
        );
        assert_eq!(
            row.application_website.as_deref(),
            Some("https://client.example"),
            "and its website"
        );

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["application"],
            json!({ "name": "Wired Client", "website": "https://client.example" }),
            "both answer as the entity's application block"
        );

        let nameless = created(&app_state, &alice, posted("no client at all")).await;
        let bare = serde_json::to_value(&nameless).expect("the entity serializes");
        assert_eq!(
            bare["application"],
            Value::Null,
            "a post with no named client renders no block"
        );
    }

    #[tokio::test]
    async fn a_refused_body_writes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "refused-author").await;

        let err = create_status(&app_state, &alice, None, CreateStatusParams::default())
            .await
            .expect_err("an absent text is refused");
        refused(&wire(err).await, EMPTY_STATUS);
        refused(
            &wire(
                create_status(
                    &app_state,
                    &alice,
                    None,
                    CreateStatusParams {
                        visibility: Some("sideways".to_owned()),
                        ..posted("hi")
                    },
                )
                .await
                .expect_err("an unknown scope is refused"),
            )
            .await,
            INVALID_VISIBILITY,
        );
        refused(
            &wire(
                create_status(
                    &app_state,
                    &alice,
                    None,
                    CreateStatusParams {
                        content_type: Some("text/rtf".to_owned()),
                        ..posted("hi")
                    },
                )
                .await
                .expect_err("an unknown format is refused"),
            )
            .await,
            INVALID_CONTENT_TYPE,
        );

        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM statuses")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 0, "not one request above wrote a row");
        let counted = crate::models::user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(counted.statuses_count, 0, "and no count moved");
    }

    #[tokio::test]
    async fn the_rich_formats_render_and_the_plain_one_escapes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "formatter").await;

        let markdown = created(
            &app_state,
            &alice,
            CreateStatusParams {
                content_type: Some("text/markdown".to_owned()),
                ..posted("a **bold** claim")
            },
        )
        .await;
        assert_eq!(
            markdown.content, "<p>a <strong>bold</strong> claim</p>",
            "markdown becomes the tag it means"
        );

        let html = created(
            &app_state,
            &alice,
            CreateStatusParams {
                content_type: Some("text/html".to_owned()),
                ..posted("<p>marked up</p><script>steal()</script>")
            },
        )
        .await;
        assert_eq!(
            html.content, "<p>marked up</p>",
            "submitted HTML keeps the allowed tags and loses the script \
             element with its text, as this instance's scrubber does"
        );

        let plain = created(
            &app_state,
            &alice,
            posted("<script>steal()</script> & friends"),
        )
        .await;
        assert_eq!(
            plain.content, "<p>&lt;script&gt;steal()&lt;/script&gt; &amp; friends</p>",
            "and plain text is escaped, so it can say those characters"
        );
    }

    /// An edit body as `ApiBody` hands it over, from the JSON shape the
    /// extractor builds for either accepted content type.
    fn body(value: Value) -> EditStatusParams {
        crate::extract::into_params::<EditStatusParams>(value)
            .expect("every body in these tests fits the edit parameter type")
    }

    #[test]
    fn every_edit_parameter_is_optional() {
        let empty = body(json!({}));
        assert_eq!(empty.status, None, "no text means the text stays");
        assert_eq!(empty.spoiler_text, None, "as does the warning");
        assert_eq!(empty.sensitive, None, "the flag");
        assert_eq!(empty.language, None, "the tag");
        assert_eq!(empty.content_type, None, "the type");
        assert_eq!(empty.media_ids, None, "and nothing is said about media");

        let full = body(json!({
            "status": "new words",
            "spoiler_text": "careful",
            "sensitive": "on",
            "language": "eo",
            "content_type": "text/markdown",
            "media_ids": ["a", "b"],
        }));
        assert_eq!(full.status.as_deref(), Some("new words"));
        assert_eq!(full.spoiler_text.as_deref(), Some("careful"));
        assert_eq!(full.sensitive, Some(true), "the form spellings ride along");
        assert_eq!(full.language.as_deref(), Some("eo"));
        assert_eq!(full.content_type.as_deref(), Some("text/markdown"));
        assert_eq!(
            full.media_ids,
            Some(vec!["a".to_owned(), "b".to_owned()]),
            "decision 10's array arrives as id text either way"
        );
    }

    #[tokio::test]
    async fn the_merge_carries_the_submitted_field_and_the_stored_rest() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "merger").await;
        let entity = created(
            &app_state,
            &alice,
            CreateStatusParams {
                spoiler_text: Some("kept warning".to_owned()),
                sensitive: Some(true),
                visibility: Some("private".to_owned()),
                language: Some("eo".to_owned()),
                content_type: Some("text/markdown".to_owned()),
                ..posted("the stored text")
            },
        )
        .await;
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        let row = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");

        let merged = merged_body(&row, &body(json!({"status": "new words"})));
        assert_eq!(
            merged.status.as_deref(),
            Some("new words"),
            "the submitted field wins"
        );
        assert_eq!(
            merged.spoiler_text.as_deref(),
            Some("kept warning"),
            "and every field the body left out arrives as the row has it"
        );
        assert_eq!(merged.sensitive, Some(true));
        assert_eq!(merged.visibility.as_deref(), Some("private"));
        assert_eq!(merged.language.as_deref(), Some("eo"));
        assert_eq!(merged.content_type.as_deref(), Some("text/markdown"));

        let checked = check_body(&Config::for_tests(), "public", false, &merged)
            .expect("a merge of a stored post is always checkable");
        assert_eq!(checked.text, "new words", "the new text goes through");
        assert_eq!(checked.visibility, Visibility::Private, "the scope stays");
        assert!(checked.sensitive, "so does the flag");
    }

    #[tokio::test]
    async fn an_empty_language_submitted_clears_where_absence_keeps() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "untagger").await;
        let entity = created(
            &app_state,
            &alice,
            CreateStatusParams {
                language: Some("de".to_owned()),
                ..posted("hallo")
            },
        )
        .await;
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        let row = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");

        let kept = merged_body(&row, &body(json!({"status": "hi again"})));
        assert_eq!(kept.language.as_deref(), Some("de"), "absent keeps the tag");
        let cleared = merged_body(&row, &body(json!({"language": ""})));
        assert_eq!(
            cleared.language.as_deref(),
            Some(""),
            "a submitted empty tag merges as the empty string, which \
             `check_body` reads as no tag at all"
        );
        assert_eq!(
            check_body(&Config::for_tests(), "public", false, &cleared)
                .expect("clearing is always checkable")
                .language,
            None
        );
    }

    #[tokio::test]
    async fn editing_through_the_service_supersedes_a_version_and_rewrites_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "editor-service").await;
        let entity = created(&app_state, &alice, posted("first version #keep")).await;
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        let row = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");

        let edited = edit_status(
            &app_state,
            &alice,
            &row,
            body(json!({"status": "second version"})),
        )
        .await
        .expect("the edit writes")
        .expect("the status is live");
        assert_eq!(edited.content, "<p>second version</p>", "the new render");
        assert!(
            edited.edited_at.is_some(),
            "a superseded version stamps the edit"
        );

        let stored = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the edited status is stored");
        assert_eq!(stored.raw_text, "second version");
        assert_eq!(
            stored.spoiler_text, "",
            "the fields nobody sent are the fields nobody moved"
        );
        let history = status_edit::list_for_status(pool, id)
            .await
            .expect("the revisions read");
        assert_eq!(history.len(), 1, "the first version is on record");
        assert_eq!(history[0].raw_text, "first version #keep");
        assert_eq!(
            history[0].content, entity.content,
            "the snapshot carries the old render, linkified and all — not \
             just the old source"
        );
    }

    #[tokio::test]
    async fn a_rejected_edit_writes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "refused-editor").await;
        let entity = created(&app_state, &alice, posted("untouched")).await;
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        let row = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");

        let long = format!(
            "Validation failed: Spoiler text is too long (maximum is {MAX_SPOILER_CHARS} characters)"
        );
        for (value, message) in [
            (json!({"status": "   "}), EMPTY_STATUS),
            (json!({"content_type": "text/rtf"}), INVALID_CONTENT_TYPE),
            (
                json!({"spoiler_text": "x".repeat(MAX_SPOILER_CHARS + 1)}),
                long.as_str(),
            ),
        ] {
            let err = edit_status(&app_state, &alice, &row, body(value))
                .await
                .expect_err("the body above is refused before anything is written");
            assert_eq!(err.to_string(), message, "the refusal wording");
        }

        let history = status_edit::list_for_status(pool, id)
            .await
            .expect("the revisions read");
        assert!(history.is_empty(), "no refused edit recorded a revision");
        let stored = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");
        assert_eq!(stored.content, "<p>untouched</p>", "or moved the row");
        assert!(stored.edited_at.is_none(), "or stamped it either");
    }

    // ────────────  the Idempotency-Key and preview arms  ────────────

    /// How many `polls` rows the database holds: one per stored question,
    /// none for a plain create, a refused body or a preview.
    async fn poll_rows(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM polls")
            .fetch_one(pool)
            .await
            .expect("the poll count reads")
    }

    /// How many `idempotency_keys` rows the database holds: one per create
    /// that claimed a key, none for a replay or a preview.
    async fn key_rows(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM idempotency_keys")
            .fetch_one(pool)
            .await
            .expect("the count reads")
    }

    /// The ids of every live status in the database, sorted: what a replay
    /// leaves untouched and a fresh create grows by exactly one.
    async fn live_ids(pool: &PgPool) -> Vec<Uuid> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM statuses WHERE deleted_at IS NULL ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .expect("the ids read")
    }

    /// A post through the idempotency arm, as `author` under `key`.
    async fn keyed(state: &AppState, author: &User, text: &str, key: &str) -> Status {
        create_status_idempotent(state, author, None, posted(text), key)
            .await
            .expect("the keyed post creates")
    }

    /// Claim `key` for `status_id` outside any create — the row a racing
    /// request would have committed before this test's create looks.
    async fn claimed_for(pool: &PgPool, author: &User, key: &str, status_id: Uuid) {
        let mut tx = pool.begin().await.expect("a transaction opens");
        let claimed = idempotency_key::claim(&mut tx, author.id, key, status_id)
            .await
            .expect("the claim runs");
        assert!(claimed, "the fixture pair was free");
        tx.commit().await.expect("the claim commits");
    }

    /// A plain stored post, written straight through the model: the test's
    /// stand-in for a create whose key was claimed in an earlier life.
    async fn stored_post(pool: &PgPool, author: &User, text: &str) -> status::Status {
        status::insert(pool, BASE, &NewStatus::local(author, text))
            .await
            .expect("the fixture post inserts")
    }

    #[tokio::test]
    async fn the_same_key_replays_the_status_it_created() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "keyed-once").await;

        let first = keyed(&app_state, &alice, "keyed once", "k1").await;
        let second = keyed(&app_state, &alice, "a different second body", "k1").await;

        assert_eq!(
            first.id, second.id,
            "one key, one status, the same id answered twice"
        );
        assert_eq!(
            second.content, "<p>keyed once</p>",
            "the replay restates the status the key created — the second \
             body is not even looked at"
        );
        assert_eq!(
            live_ids(pool).await,
            vec![Uuid::parse_str(&first.id).expect("the entity carries its id")],
            "and exactly one row was ever written"
        );
        assert_eq!(key_rows(pool).await, 1, "one claim stands");
        let counted = crate::models::user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(counted.statuses_count, 1, "counted once");
    }

    #[tokio::test]
    async fn different_keys_create_their_own_statuses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "keyed-twice").await;

        let first = keyed(&app_state, &alice, "one", "k1").await;
        let second = keyed(&app_state, &alice, "two", "k2").await;

        assert_ne!(first.id, second.id, "two keys are two posts");
        assert_eq!(live_ids(pool).await.len(), 2, "and two rows stand");
        assert_eq!(key_rows(pool).await, 2, "each with its own claim");
    }

    #[tokio::test]
    async fn one_key_belongs_to_its_author_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "sharp").await;
        let bob = user(pool, "echo").await;

        let mine = keyed(&app_state, &alice, "alice's", "shared").await;
        let theirs = keyed(&app_state, &bob, "bob's", "shared").await;

        assert_ne!(
            mine.id, theirs.id,
            "the same key text is one key per account"
        );
        let again = keyed(&app_state, &alice, "ignored", "shared").await;
        assert_eq!(again.id, mine.id, "and each account replays its own status");
        assert_eq!(key_rows(pool).await, 2, "two claims, one per pair");
    }

    #[tokio::test]
    async fn a_key_claimed_by_another_writer_answers_its_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "late").await;
        let winner = stored_post(pool, &alice, "the winner").await;
        claimed_for(pool, &alice, "k1", winner.id).await;

        let replay = keyed(&app_state, &alice, "the loser's body", "k1").await;

        assert_eq!(
            replay.id,
            winner.id.to_string(),
            "the committed claim is the key's answer"
        );
        assert_eq!(
            live_ids(pool).await,
            vec![winner.id],
            "and the losing body created nothing of its own"
        );
        assert_eq!(key_rows(pool).await, 1, "the claim stands alone");
    }

    /// Two creates under one fresh key, driven concurrently from one task:
    /// whichever arm the second request takes — pre-read replay or the
    /// mid-transaction `ON CONFLICT` loss — exactly one status stands and
    /// both callers answer it with the same id. Both bodies carry a poll,
    /// so the single surviving `polls` row proves the loser's poll rolled
    /// back with its status.
    #[tokio::test]
    async fn two_creates_racing_under_one_key_leave_one_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "racer").await;
        let fixture = Arc::new((state(pool), alice));

        let (first, second) = tokio::join!(
            create_status_idempotent(&fixture.0, &fixture.1, None, polled("race one"), "k1"),
            create_status_idempotent(&fixture.0, &fixture.1, None, polled("race two"), "k1"),
        );
        let one = first.expect("the first racer creates or replays");
        let two = second.expect("the second racer creates or replays");

        assert_eq!(
            one.id, two.id,
            "both callers are answered by the one status the key made"
        );
        assert_eq!(
            live_ids(pool).await,
            vec![Uuid::parse_str(&one.id).expect("an id")]
        );
        assert_eq!(key_rows(pool).await, 1, "one claim");
        assert_eq!(
            poll_rows(pool).await,
            1,
            "one poll — the loser's rolled back with its status"
        );
        let winner_id = Uuid::parse_str(&one.id).expect("an id");
        let stand = poll::get_for_status(pool, winner_id)
            .await
            .expect("the poll reads")
            .expect("the winner's poll stands");
        assert_eq!(
            poll::options(pool, stand.id)
                .await
                .expect("the options read")
                .len(),
            2,
            "and its options stand with it"
        );
        for entity in [&one, &two] {
            let value = serde_json::to_value(entity).expect("the entity serializes");
            assert_eq!(
                value["poll"]["id"],
                stand.id.to_string(),
                "both answers carry the one surviving poll"
            );
        }
        let counted = crate::models::user::get_by_id(pool, fixture.1.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(
            counted.statuses_count, 1,
            "the loser's rolled-back write counted nothing"
        );
    }

    #[tokio::test]
    async fn a_key_whose_status_was_deleted_creates_afresh() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "repeater").await;
        let doomed = keyed(&app_state, &alice, "post and delete me", "k1").await;
        let doomed_id = Uuid::parse_str(&doomed.id).expect("the entity carries its id");
        let tombstone = status::soft_delete(pool, doomed_id)
            .await
            .expect("the delete runs")
            .expect("the row was live");
        assert!(
            tombstone.deleted_at.is_some(),
            "the mapped row is gone from the API"
        );

        let fresh = keyed(&app_state, &alice, "the next one", "k1").await;
        let fresh_id = Uuid::parse_str(&fresh.id).expect("the entity carries its id");

        assert_ne!(
            fresh.id, doomed.id,
            "a deleted status is no replay-able answer"
        );
        assert_eq!(
            live_ids(pool).await,
            vec![fresh_id],
            "the fresh post is the only live row — and it moved the stale \
             claim instead of colliding with it"
        );
        assert_eq!(key_rows(pool).await, 1, "still one mapping for the pair");
        let restated = keyed(&app_state, &alice, "ignored", "k1").await;
        assert_eq!(
            restated.id, fresh.id,
            "and the moved key now replays afresh"
        );
    }

    #[tokio::test]
    async fn a_preview_builds_the_whole_entity() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "looker").await;
        let bob = user(pool, "bob").await;

        let entity = preview_status(
            &app_state,
            &alice,
            None,
            &params(json!({
                "status": "hi @bob and @nobody, see #Rust #rust",
                "preview": true,
            })),
        )
        .await
        .expect("the preview builds");
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        let id = Uuid::parse_str(&entity.id).expect("the preview carries its id");

        assert!(!id.is_nil(), "a freshly minted id");
        assert_eq!(value["id"], id.to_string());
        assert_eq!(
            value["uri"],
            format!("{BASE}/objects/{id}"),
            "the urls a create of this body would derive"
        );
        assert_eq!(value["url"], json!(format!("{BASE}/notice/{id}")));
        assert_eq!(
            value["content"],
            format!(
                "<p>hi {} and @nobody, see {} {}</p>",
                mention_html(&bob.ap_id, "bob"),
                hashtag_html(BASE, "rust"),
                hashtag_html(BASE, "rust"),
            ),
            "the same render the create stores, mentions and tags linkified"
        );
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "bob",
                     "url": bob.ap_id, "acct": "bob" }]),
            "the mentioned account is resolved into the entity"
        );
        assert_eq!(
            value["tags"],
            json!([{ "name": "rust", "url": format!("{BASE}/tags/rust") }]),
            "and the tag once, folded, as the stored render answers it"
        );
        let age = (Utc::now() - entity.created_at).num_seconds();
        assert!((0..120).contains(&age), "created now: {age}s ago");
        assert_eq!(value["reblogs_count"], 0);
        assert_eq!(value["favourites_count"], 0);
        assert_eq!(value["replies_count"], 0);
        assert_eq!(value["pleroma"]["quotes_count"], 0);
        assert_eq!(value["account"]["acct"], "looker", "rendered as its author");
        assert_eq!(
            value["pleroma"]["content"],
            json!({ "text/plain": "hi @bob and @nobody, see #Rust #rust" }),
            "the source text beside the render"
        );
    }

    #[tokio::test]
    async fn a_preview_writes_no_row_anywhere() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "look-but-dont").await;
        let bob = user(pool, "bob").await;

        let entity = preview_status(
            &app_state,
            &alice,
            None,
            &params(json!({
                "status": "hi @bob, see #Rust",
                "preview": true,
            })),
        )
        .await
        .expect("the preview builds");
        let id = Uuid::parse_str(&entity.id).expect("the preview carries its id");

        assert!(live_ids(pool).await.is_empty(), "no status was written");
        assert_eq!(key_rows(pool).await, 0, "no key was claimed");
        let mentions: (i64,) = sqlx::query_as("SELECT count(*) FROM status_mentions")
            .fetch_one(pool)
            .await
            .expect("the mention count reads");
        let tags: (i64,) = sqlx::query_as("SELECT count(*) FROM hashtags")
            .fetch_one(pool)
            .await
            .expect("the tag count reads");
        let tag_links: (i64,) = sqlx::query_as("SELECT count(*) FROM status_hashtags")
            .fetch_one(pool)
            .await
            .expect("the tag link count reads");
        assert_eq!(
            (mentions.0, tags.0, tag_links.0),
            (0, 0, 0),
            "no mention link, no tag row and no tag link were written — \
             the mentions and tags the entity answers are resolved, not stored"
        );
        let counted = crate::models::user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(counted.statuses_count, 0, "no count moved");
        assert!(
            status::get_by_id(pool, id).await.unwrap().is_none(),
            "and the previewed id names no row"
        );
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "bob",
                     "url": bob.ap_id, "acct": "bob" }]),
            "the entity resolves the mention although nothing links it"
        );
    }

    #[tokio::test]
    async fn two_previews_of_one_body_are_two_fresh_ids() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "twice-seen").await;
        let asked = params(json!({"status": "same words", "preview": "true"}));

        let first = preview_status(&app_state, &alice, None, &asked)
            .await
            .expect("the first preview builds");
        let second = preview_status(&app_state, &alice, None, &asked)
            .await
            .expect("the second preview builds");

        assert_ne!(first.id, second.id, "every preview mints its own id");
        assert_eq!(first.content, second.content, "and the same render");
        assert!(live_ids(pool).await.is_empty(), "and neither wrote");
    }

    #[tokio::test]
    async fn a_preview_refuses_what_a_creation_refuses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "wishful").await;

        for (value, message) in [
            (json!({"status": "  ", "preview": true}), EMPTY_STATUS),
            (
                json!({"status": "hi", "visibility": "sideways", "preview": true}),
                INVALID_VISIBILITY,
            ),
            (
                json!({"status": "hi", "content_type": "text/rtf", "preview": true}),
                INVALID_CONTENT_TYPE,
            ),
            (
                json!({"status": "hi", "media_ids": ["not-an-id"], "preview": true}),
                BAD_MEDIA_ID,
            ),
            (
                json!({ "status": "hi",
                        "media_ids": ["", "", "", "", ""],
                        "preview": true, }),
                TOO_MANY_ATTACHMENTS,
            ),
            (
                json!({
                    "status": "hi",
                    "spoiler_text": "x".repeat(MAX_SPOILER_CHARS + 1),
                    "preview": true,
                }),
                "Validation failed: Spoiler text is too long (maximum is 200 characters)",
            ),
        ] {
            let err = preview_status(&app_state, &alice, None, &params(value))
                .await
                .expect_err("the body above is refused like any post");
            refused(&wire(err).await, message);
        }
        assert!(
            live_ids(pool).await.is_empty(),
            "not one refused preview wrote a row"
        );
        assert_eq!(key_rows(pool).await, 0, "or claimed a key");
    }

    /// Pleroma's `expires_in`: the accepted span becomes the exact instant
    /// it counts out from now, the floor itself passes, and no expiry at
    /// all — absent or null — is no `expires_at`.
    #[test]
    fn an_expires_in_becomes_the_instant_it_counts_out() {
        let absent = params(json!({"status": "hi"}));
        assert_eq!(accepted(&absent).expires_at, None, "no key, no expiry");
        let null = params(json!({"status": "hi", "expires_in": null}));
        assert_eq!(accepted(&null).expires_at, None, "and null is as absent");

        for secs in [600_i64, 3_600, 86_400] {
            let before = Utc::now();
            let body = params(json!({"status": "hi", "expires_in": secs}));
            let due = accepted(&body)
                .expires_at
                .unwrap_or_else(|| panic!("{secs} seconds is an expiry"));
            let span = due - before;
            assert!(
                (TimeDelta::seconds(secs) - span).num_seconds().abs() <= 1,
                "{secs} seconds lands within a second of the span it names"
            );
        }
    }

    /// Under the floor, over the calendar: the two 422s `expires_in` owes,
    /// in the wording the ticket pins for the first and naming the value
    /// from the configuration.
    #[test]
    fn the_floor_and_the_calendar_refuse_the_expirations_they_cannot_take() {
        for secs in [-1_i64, 0, 1, 599] {
            let body = params(json!({"status": "hi", "expires_in": secs}));
            assert_eq!(
                rejection(&body),
                "expires_in must be at least 600 seconds",
                "{secs} is under the stock 600-second floor"
            );
        }
        let body = params(json!({"status": "hi", "expires_in": i64::MAX}));
        assert_eq!(
            rejection(&body),
            EXPIRATION_TOO_LARGE,
            "a span no timestamp can hold is a mistake, not a distant expiry"
        );
    }

    /// The refusal is generated from `MIN_EXPIRATION_SECS`: an instance
    /// that lowered the floor names its own number and takes its own
    /// minimum.
    #[test]
    fn the_message_names_this_instances_floor() {
        let config = Config {
            min_expiration_secs: 120,
            ..Config::for_tests()
        };
        let checked = |secs: i64| {
            let body = params(json!({"status": "hi", "expires_in": secs}));
            check_body(&config, "public", false, &body)
                .map(|_| ())
                .map_err(|err| err.to_string())
        };
        assert!(checked(120).is_ok(), "the configured floor itself passes");
        assert_eq!(
            checked(119).expect_err("one under it does not"),
            "expires_in must be at least 120 seconds",
            "and the message says the configured number, not the shipped one"
        );
        let stock = params(json!({"status": "hi", "expires_in": 120}));
        assert_eq!(
            check_body(&Config::for_tests(), "public", false, &stock)
                .expect_err("the stock floor still refuses it")
                .to_string(),
            "expires_in must be at least 600 seconds",
            "the default config keeps the default message"
        );
    }

    /// The `flex` reading of the parameter: a form body's numeric string is
    /// the same expiry as JSON's number, and everything that is no integer
    /// at all — text, a boolean, a fraction, a container — is the
    /// extractor's 422 before any service runs.
    #[test]
    fn only_integers_deserialize_and_a_numeric_string_counts_as_one() {
        let body = params(json!({"status": "hi", "expires_in": "3600"}));
        assert!(
            accepted(&body).expires_at.is_some(),
            "`expires_in=3600` from a form body is an expiry"
        );
        for value in [
            json!("soon"),
            json!(""),
            json!("60.5"),
            json!(60.5),
            json!(true),
            json!([3600]),
            json!({"secs": 3600}),
        ] {
            let parsed = crate::extract::into_params::<CreateStatusParams>(
                json!({"status": "hi", "expires_in": value}),
            );
            assert!(
                parsed.is_err(),
                "`{value}` is no integer and must not deserialize"
            );
        }
    }

    /// The `status_mentions` links of one stored status, in position order.
    async fn mention_ids(pool: &PgPool, status_id: Uuid) -> Vec<Uuid> {
        sqlx::query_as::<_, (Uuid,)>(
            "SELECT user_id FROM status_mentions WHERE status_id = $1 ORDER BY position",
        )
        .bind(status_id)
        .fetch_all(pool)
        .await
        .expect("the mention links read")
        .into_iter()
        .map(|link| link.0)
        .collect()
    }

    /// The live row behind a status id, for the counters and links a reply
    /// or a quote moved on the row it names.
    async fn parent_row(pool: &PgPool, id: Uuid) -> status::Status {
        status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the parent is stored live")
    }

    /// A body answering the status `parent_id`, with `text` as its post.
    fn replying(text: &str, parent_id: Uuid) -> CreateStatusParams {
        CreateStatusParams {
            in_reply_to_id: Some(parent_id.to_string()),
            ..posted(text)
        }
    }

    /// The `{"error": …}` body of the shared 404, plus its status code:
    /// what an unanswerable reply parent or quote original owes the client.
    async fn missed(err: AppError) {
        let (code, body) = wire(err).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "the parent miss is a 404");
        assert_eq!(
            body,
            json!({ "error": "Record not found" }),
            "the shared body"
        );
    }

    /// A plain stored post written straight through the model — the fixture
    /// parent a service test replies to.
    async fn plain_post(pool: &PgPool, author: &User, text: &str) -> status::Status {
        status::insert(pool, BASE, &NewStatus::local(author, text))
            .await
            .expect("the fixture post inserts")
    }

    // ─────────────────────────  replies (in_reply_to_id)  ─────────────────────────

    /// The parameter arrives as id text or not at all: absent, null and the
    /// form body's plain string all deserialize into the optional field.
    #[test]
    fn the_reply_parameter_deserializes_as_id_text() {
        let absent = params(json!({"status": "hi"}));
        assert_eq!(
            absent.in_reply_to_id, None,
            "no key is a root post's parameter"
        );
        let null = params(json!({"status": "hi", "in_reply_to_id": null}));
        assert_eq!(null.in_reply_to_id, None, "a null is as absent as no key");
        let one = params(json!({
            "status": "hi",
            "in_reply_to_id": "01934567-89ab-7cde-f012-3456789abcde",
        }));
        assert_eq!(
            one.in_reply_to_id.as_deref(),
            Some("01934567-89ab-7cde-f012-3456789abcde"),
            "id text is stored as written"
        );
    }

    /// The whole visibility matrix of the reply inheritance rule: a
    /// `direct` parent forces `direct`; under a `private` parent a
    /// submitted `public` or `unlisted` comes down while every other
    /// submitted scope stands; a submitted scope under a wider parent
    /// stands untouched; and an unsubmitted one is the author's default
    /// capped at the parent's width, never wider.
    #[test]
    fn the_inherited_visibility_matrix_is_exact() {
        for parent in Visibility::ALL {
            assert_eq!(
                inherited_visibility(Visibility::Direct, true, parent),
                Visibility::Direct,
                "a direct parent forces direct whatever was asked for"
            );
            assert_eq!(
                inherited_visibility(Visibility::Direct, false, parent),
                Visibility::Direct,
                "and whatever the author defaults to"
            );
        }
        for chosen in [Visibility::Public, Visibility::Unlisted] {
            assert_eq!(
                inherited_visibility(Visibility::Private, true, chosen),
                Visibility::Private,
                "a submitted `{chosen}` under a private parent comes down"
            );
        }
        for (chosen, kept) in [
            (Visibility::Private, Visibility::Private),
            (Visibility::Direct, Visibility::Direct),
            (Visibility::Local, Visibility::Local),
        ] {
            assert_eq!(
                inherited_visibility(Visibility::Private, true, chosen),
                kept,
                "every other submitted scope under a private parent stands"
            );
        }
        assert_eq!(
            inherited_visibility(Visibility::Unlisted, true, Visibility::Public),
            Visibility::Public,
            "Pleroma keeps a submitted public under an unlisted parent"
        );
        assert_eq!(
            inherited_visibility(Visibility::Public, true, Visibility::Private),
            Visibility::Private,
            "a narrower submitted scope is never widened by a public parent"
        );
        assert_eq!(
            inherited_visibility(Visibility::Local, true, Visibility::Unlisted),
            Visibility::Unlisted,
            "a submitted scope under a local parent stands as asked"
        );
        // Unsubmitted: the default `chosen` capped at the parent's width.
        for (parent, chosen, expected) in [
            (Visibility::Public, Visibility::Local, Visibility::Local),
            (Visibility::Public, Visibility::Public, Visibility::Public),
            (Visibility::Public, Visibility::Private, Visibility::Private),
            (
                Visibility::Unlisted,
                Visibility::Public,
                Visibility::Unlisted,
            ),
            (Visibility::Private, Visibility::Public, Visibility::Private),
            (Visibility::Private, Visibility::Local, Visibility::Private),
            (Visibility::Local, Visibility::Public, Visibility::Local),
            (Visibility::Local, Visibility::Private, Visibility::Private),
            (
                Visibility::Unlisted,
                Visibility::Private,
                Visibility::Private,
            ),
        ] {
            assert_eq!(
                inherited_visibility(parent, false, chosen),
                expected,
                "a default `{chosen}` replying to `{parent}` is capped to \
                 `{expected}`, never wider than the parent"
            );
        }
    }

    /// The pure half of the auto-mention rule: the parent's author leads,
    /// duplicates fold to their first place, the replier never appears, and
    /// the inherited set is capped.
    #[test]
    fn inherited_mentions_lead_with_the_author_and_drop_the_rest() {
        let author = Uuid::now_v7();
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let replier = Uuid::now_v7();
        assert_eq!(
            inherited_mentions(author, &[first, second], replier),
            vec![author, first, second],
            "the parent's author leads, then its mentions in order"
        );
        assert_eq!(
            inherited_mentions(author, &[author, first, first, second], second),
            vec![author, first],
            "duplicates fold to the first place and the replier drops out"
        );
        assert_eq!(
            inherited_mentions(author, &[first, author], author),
            vec![first],
            "an author replying to their own post is never auto-mentioned, \
             even where the parent mentioned them"
        );
        let crowd: Vec<Uuid> = (0..25).map(|_| Uuid::now_v7()).collect();
        let inherited = inherited_mentions(author, &crowd, replier);
        assert_eq!(
            inherited.len(),
            MAX_INHERITED_MENTIONS,
            "author plus 25 mentions is capped at {MAX_INHERITED_MENTIONS}"
        );
        assert_eq!(inherited[0], author, "and the author leads the cap");
        assert_eq!(
            &inherited[1..],
            &crowd[..MAX_INHERITED_MENTIONS - 1],
            "the first accounts in order win the rest"
        );
    }

    /// The body's own mentions lead and keep their places; the inherited
    /// ones join behind them, each account linked once however both sides
    /// name it.
    #[test]
    fn the_bodies_own_mentions_lead_the_merge() {
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(merged_mentions(&[b], &[a, b, c]), vec![b, a, c]);
        assert_eq!(merged_mentions(&[], &[a]), vec![a]);
        assert_eq!(
            merged_mentions(&[a, b], &[]),
            vec![a, b],
            "no parent add nothing"
        );
    }

    #[tokio::test]
    async fn a_reply_stores_its_parents_fields_counts_and_language() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "replier").await;
        let bob = user(pool, "bob").await;
        let parent = created(
            &app_state,
            &bob,
            CreateStatusParams {
                language: Some("eo".to_owned()),
                ..posted("the parent")
            },
        )
        .await;
        let parent_id = Uuid::parse_str(&parent.id).expect("the entity carries its id");

        let answer = created(&app_state, &alice, replying("right back", parent_id)).await;
        let row = stored(pool, &answer).await;

        assert_eq!(
            row.in_reply_to_id,
            Some(parent_id),
            "the reply names its parent"
        );
        assert_eq!(
            row.in_reply_to_account_id,
            Some(bob.id),
            "and the parent's author"
        );
        assert_eq!(
            row.conversation_id, parent_id,
            "a reply joins the root's own thread"
        );
        assert_eq!(
            row.language.as_deref(),
            Some("eo"),
            "an absent language falls back to the parent's tag"
        );
        assert_eq!(row.raw_text, "right back", "the text is not modified");
        assert_eq!(
            row.content, "<p>right back</p>",
            "no mention span is inserted into the render"
        );
        assert_eq!(
            mention_ids(pool, row.id).await,
            vec![bob.id],
            "the parent's author joins status_mentions alone"
        );

        let counted = parent_row(pool, parent_id).await;
        assert_eq!(counted.replies_count, 1, "the parent counts the reply");

        let value = serde_json::to_value(&answer).expect("the entity serializes");
        assert_eq!(value["in_reply_to_id"], parent_id.to_string());
        assert_eq!(value["in_reply_to_account_id"], bob.id.to_string());
        assert_eq!(value["pleroma"]["in_reply_to_account_acct"], "bob");
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "bob",
                     "url": bob.ap_id, "acct": "bob" }]),
            "the auto-mention answers in the entity"
        );
    }

    /// A submitted language beats the parent's; a parent with no tag
    /// inherits nothing.
    #[tokio::test]
    async fn a_submitted_language_beats_the_parent_s() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "tagged").await;
        let bob = user(pool, "bob").await;
        let tagged = created(
            &app_state,
            &bob,
            CreateStatusParams {
                language: Some("eo".to_owned()),
                ..posted("saluton")
            },
        )
        .await;
        let tagged_id = Uuid::parse_str(&tagged.id).expect("an id");
        let mut body = replying("danke", tagged_id);
        body.language = Some("de".to_owned());
        let overridden = created(&app_state, &alice, body).await;
        assert_eq!(
            overridden.language.as_deref(),
            Some("de"),
            "the submitted tag wins over the parent's"
        );

        let plain = plain_post(pool, &bob, "no tag here").await;
        let rootless = created(&app_state, &alice, replying("nor this", plain.id)).await;
        assert_eq!(
            rootless.language, None,
            "a parent with no tag leaves the reply tagless"
        );
    }

    /// The parent's mentions ride along after its author, deduplicated
    /// against the reply's own text mentions — and a self-reply mentions
    /// nobody.
    #[tokio::test]
    async fn a_reply_inherits_the_parent_s_mentions_deduplicated() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "replier").await;
        let bob = user(pool, "bob").await;
        let carol = user(pool, "carol").await;
        let dave = user(pool, "dave").await;

        let parent = created(&app_state, &bob, posted("@carol and @dave, hi")).await;
        let parent_id = Uuid::parse_str(&parent.id).expect("an id");
        let answer = created(&app_state, &alice, replying("sure @CAROL", parent_id)).await;
        let row = stored(pool, &answer).await;

        assert_eq!(
            mention_ids(pool, row.id).await,
            vec![carol.id, bob.id, dave.id],
            "the text's own mention leads, then the parent's author, then \
             the parent's mentions — with the account both sides name \
             linked once"
        );

        let mine = created(&app_state, &alice, posted("my own @dave")).await;
        let mine_id = Uuid::parse_str(&mine.id).expect("an id");
        let self_reply = created(&app_state, &alice, replying("to myself", mine_id)).await;
        let self_row = stored(pool, &self_reply).await;
        assert_eq!(
            mention_ids(pool, self_row.id).await,
            vec![dave.id],
            "the replier never joins their own auto-mentions, only the \
             accounts the parent mentioned that are not them"
        );
        assert_eq!(
            self_row.in_reply_to_account_id,
            Some(alice.id),
            "while the reply link itself still names the author"
        );
    }

    /// Twenty-five accounts on the parent's side is more than the cap: the
    /// author leads, the first nineteen follow in order, and the tail is
    /// simply not linked. A crowd member the reply text mentions itself
    /// stays — the body's own mentions do not count against the cap.
    #[tokio::test]
    async fn the_inherited_mentions_are_capped_at_twenty() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "replier").await;
        let bob = user(pool, "bob").await;
        let mut crowd: Vec<User> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for n in 1..=25_u32 {
            let nick = format!("member{n:02}");
            names.push(format!("@{nick}"));
            crowd.push(user(pool, &nick).await);
        }
        let crowd_ids: Vec<Uuid> = crowd.iter().map(|found| found.id).collect();
        let parent = created(&app_state, &bob, posted(&names.join(" "))).await;
        let parent_id = Uuid::parse_str(&parent.id).expect("an id");
        assert_eq!(
            mention_ids(pool, parent_id).await,
            crowd_ids,
            "the fixture parent really mentions all twenty-five"
        );

        let last = crowd_ids[24];
        let answer = created(
            &app_state,
            &alice,
            replying(&format!("noted @{}", crowd[24].nickname), parent_id),
        )
        .await;
        let row = stored(pool, &answer).await;

        let links = mention_ids(pool, row.id).await;
        let mut expected: Vec<Uuid> = vec![last, bob.id];
        expected.extend(crowd_ids[..MAX_INHERITED_MENTIONS - 1].iter().copied());
        assert_eq!(
            links, expected,
            "the body's own mention leads, then the capped inheritage — \
             author first, its nineteen first mentions in order"
        );
        for dropped in &crowd_ids[MAX_INHERITED_MENTIONS - 1..24] {
            assert!(
                !links.contains(dropped),
                "the parent's tail past the cap is not linked"
            );
        }
    }

    /// The service-level legs of the visibility inheritance rules the pure
    /// matrix pins: a submitted `public` coming down under a `private`
    /// parent, an unsubmitted default capped the same way, a `direct`
    /// parent forcing `direct`, and a requested `public` standing
    /// untouched under an `unlisted` parent.
    #[tokio::test]
    async fn a_reply_s_audience_follows_its_parent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "wide").await;
        let bob = user(pool, "bob").await;

        let private_parent = created(
            &app_state,
            &bob,
            CreateStatusParams {
                visibility: Some("private".to_owned()),
                ..posted("hush")
            },
        )
        .await;
        let private_id = Uuid::parse_str(&private_parent.id).expect("an id");
        let mut requested = replying("reply", private_id);
        requested.visibility = Some("public".to_owned());
        let downgraded = created(&app_state, &bob, requested).await;
        assert_eq!(
            downgraded.visibility, "private",
            "a submitted public under a private parent comes down"
        );

        let unsubmitted = created(&app_state, &bob, replying("reply", private_id)).await;
        assert_eq!(
            unsubmitted.visibility, "private",
            "a public default under a private parent is capped to private"
        );

        let direct_parent = created(
            &app_state,
            &bob,
            CreateStatusParams {
                visibility: Some("direct".to_owned()),
                ..posted("secret")
            },
        )
        .await;
        let direct_id = Uuid::parse_str(&direct_parent.id).expect("an id");
        let mut asked_wide = replying("reply", direct_id);
        asked_wide.visibility = Some("public".to_owned());
        let forced = created(&app_state, &bob, asked_wide).await;
        assert_eq!(
            forced.visibility, "direct",
            "a direct parent forces direct whatever was asked for"
        );

        let unlisted_parent = created(
            &app_state,
            &bob,
            CreateStatusParams {
                visibility: Some("unlisted".to_owned()),
                ..posted("quiet")
            },
        )
        .await;
        let unlisted_id = Uuid::parse_str(&unlisted_parent.id).expect("an id");
        let mut kept_wide = replying("reply", unlisted_id);
        kept_wide.visibility = Some("public".to_owned());
        let standing = created(&app_state, &alice, kept_wide).await;
        assert_eq!(
            standing.visibility, "public",
            "an unlisted parent keeps a requested public as requested"
        );
    }

    /// A reply to a reblog row answers the boosted original: the ids, the
    /// thread and the counted reply all land on the original, not on the
    /// boost row the id named.
    #[tokio::test]
    async fn a_reply_to_a_reblog_answers_the_original() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "replier").await;
        let bob = user(pool, "original-author").await;
        let carol = user(pool, "booster").await;
        let original = created(&app_state, &bob, posted("the original")).await;
        let original_id = Uuid::parse_str(&original.id).expect("an id");
        let reblog = status::insert(
            pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(original_id),
                ..NewStatus::local(&carol, "")
            },
        )
        .await
        .expect("a reblog row inserts");

        let answer = created(&app_state, &alice, replying("reacting", reblog.id)).await;
        let row = stored(pool, &answer).await;

        assert_eq!(
            row.in_reply_to_id,
            Some(original_id),
            "the reply targets the reblogged original"
        );
        assert_eq!(
            row.in_reply_to_account_id,
            Some(bob.id),
            "and the original's author, not the booster"
        );
        assert_eq!(row.conversation_id, original_id, "the original's thread");
        assert_eq!(
            mention_ids(pool, row.id).await,
            vec![bob.id],
            "the auto-mention names the original's author"
        );

        let reblog_after = parent_row(pool, reblog.id).await;
        assert_eq!(
            reblog_after.replies_count, 0,
            "the boost row does not count the reply"
        );
        let original_after = parent_row(pool, original_id).await;
        assert_eq!(
            original_after.replies_count, 1,
            "the original gains the reply"
        );
    }

    /// Everything that cannot answer — malformed text, an id that names no
    /// row, a tombstone, a post the poster may not see, and a reblog whose
    /// original went away — is the one shared 404, and none of it writes.
    /// The empty string, Pleroma's own no-reply spelling, is not among
    /// them: it posts a root status.
    #[tokio::test]
    async fn an_unanswerable_parent_is_the_shared_not_found() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "stranger").await;
        let bob = user(pool, "bob").await;

        let gone = plain_post(pool, &bob, "doomed").await;
        let tombstone = status::soft_delete(pool, gone.id)
            .await
            .expect("the delete runs")
            .expect("the row was live");
        assert!(tombstone.deleted_at.is_some(), "the parent is a tombstone");
        let hidden = created(
            &app_state,
            &bob,
            CreateStatusParams {
                visibility: Some("private".to_owned()),
                ..posted("not for you")
            },
        )
        .await;
        let hidden_id = Uuid::parse_str(&hidden.id).expect("an id");
        let stranded_reblog = status::insert(
            pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(gone.id),
                ..NewStatus::local(&bob, "")
            },
        )
        .await
        .expect("a reblog of a tombstone's id inserts");

        let misses: Vec<CreateStatusParams> = vec![
            {
                let mut body = posted("hi");
                body.in_reply_to_id = Some("not-an-id".to_owned());
                body
            },
            replying("hi", Uuid::now_v7()),
            replying("hi", gone.id),
            replying("hi", hidden_id),
            replying("hi", stranded_reblog.id),
        ];
        let before = live_ids(pool).await;
        for params in misses {
            let err = create_status(&app_state, &alice, None, params)
                .await
                .expect_err("the parent above cannot be answered");
            missed(err).await;
        }
        assert_eq!(
            live_ids(pool).await,
            before,
            "not one refused reply wrote a row"
        );

        let root = created(
            &app_state,
            &alice,
            CreateStatusParams {
                in_reply_to_id: Some("  ".to_owned()),
                ..posted("a root after all")
            },
        )
        .await;
        let row = stored(pool, &root).await;
        assert!(
            row.in_reply_to_id.is_none() && row.in_reply_to_account_id.is_none(),
            "a blank parent parameter is no reply at all"
        );
        assert_eq!(
            row.conversation_id, row.id,
            "and the post is its own thread"
        );
        assert_eq!(
            live_ids(pool).await.len(),
            before.len() + 1,
            "and the blank is the only row the whole test wrote"
        );
    }

    /// Three levels deep: every answer joins the root's conversation and
    /// steps the parent it answers, whatever else the thread holds.
    #[tokio::test]
    async fn a_thread_shares_one_conversation_across_three_levels() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "first").await;
        let bob = user(pool, "bob").await;
        let carol = user(pool, "carol").await;

        let root = created(&app_state, &alice, posted("root")).await;
        let root_id = Uuid::parse_str(&root.id).expect("an id");
        let second = created(&app_state, &bob, replying("second", root_id)).await;
        let second_id = Uuid::parse_str(&second.id).expect("an id");
        let third = created(&app_state, &carol, replying("third", second_id)).await;

        let second_row = stored(pool, &second).await;
        let third_row = stored(pool, &third).await;
        assert_eq!(second_row.conversation_id, root_id, "the first reply");
        assert_eq!(third_row.conversation_id, root_id, "the second reply");

        let root_row = parent_row(pool, root_id).await;
        assert_eq!(
            root_row.replies_count, 1,
            "the root counts its direct reply"
        );
        let second_after = parent_row(pool, second_id).await;
        assert_eq!(
            second_after.replies_count, 1,
            "the middle row counts the one under it"
        );
        assert_eq!(
            third_row.in_reply_to_id,
            Some(second_id),
            "and each level answers the row directly above it"
        );
    }

    /// The keyed arm shares the create's whole pre-write half, so a reply
    /// posts under an `Idempotency-Key` exactly as it would without one —
    /// and a replay restates it without counting the parent twice.
    #[tokio::test]
    async fn a_keyed_reply_replies_and_replays() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "keyer").await;
        let bob = user(pool, "bob").await;
        let parent = plain_post(pool, &bob, "keyable").await;

        let first =
            create_status_idempotent(&app_state, &alice, None, replying("keyed", parent.id), "k1")
                .await
                .expect("the keyed reply creates");
        assert_eq!(
            first.in_reply_to_id,
            Some(parent.id.to_string()),
            "the entity answers its parent"
        );
        let row = status::get_by_id(pool, Uuid::parse_str(&first.id).expect("an id"))
            .await
            .unwrap()
            .expect("the reply is stored");
        assert_eq!(row.in_reply_to_account_id, Some(bob.id));
        assert_eq!(row.conversation_id, parent.conversation_id);
        assert_eq!(parent_row(pool, parent.id).await.replies_count, 1);

        let again = create_status_idempotent(
            &app_state,
            &alice,
            None,
            replying("ignored", parent.id),
            "k1",
        )
        .await
        .expect("the replay answers");
        assert_eq!(again.id, first.id, "one key, one reply");
        assert_eq!(
            parent_row(pool, parent.id).await.replies_count,
            1,
            "and the parent counted it once"
        );
    }

    /// The media body: one unattached `image/png` upload by `owner`, as the
    /// row a `media_ids` entry names.
    async fn upload(pool: &PgPool, owner: &User) -> media_attachment::MediaAttachment {
        media_attachment::insert(
            pool,
            &NewMediaAttachment {
                user_id: Some(owner.id),
                url: format!("{BASE}/media/fixture.png"),
                content_type: "image/png".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect("a fixture upload inserts")
    }

    /// A post carrying `text` and `ids` as its `media_ids`, the ids as the
    /// client spells them.
    fn with_media(text: &str, ids: &[&str]) -> CreateStatusParams {
        CreateStatusParams {
            status: Some(text.to_owned()),
            media_ids: Some(ids.iter().map(|id| (*id).to_owned()).collect()),
            ..CreateStatusParams::default()
        }
    }

    /// The stored media row, for the claims and rollbacks a test inspects.
    async fn media_row(pool: &PgPool, id: Uuid) -> media_attachment::MediaAttachment {
        media_attachment::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture upload is stored")
    }

    /// The ids an entity's `media_attachments` answers, in answer order.
    fn attachment_ids(entity: &Status) -> Vec<String> {
        entity
            .media_attachments
            .iter()
            .map(|attachment| attachment.id.clone())
            .collect()
    }

    #[test]
    fn the_media_ids_parameter_deserializes_as_id_text() {
        let absent = params(json!({"status": "hi"}));
        assert_eq!(absent.media_ids, None, "no key says nothing about media");
        let null = params(json!({"status": "hi", "media_ids": null}));
        assert_eq!(null.media_ids, None, "a null is as absent as no key");
        let list = params(json!({
            "status": "hi",
            "media_ids": ["01934567-89ab-7cde-f012-3456789abcde", "junk"],
        }));
        assert_eq!(
            list.media_ids,
            Some(vec![
                "01934567-89ab-7cde-f012-3456789abcde".to_owned(),
                "junk".to_owned(),
            ]),
            "decision 10's array arrives as id text, JSON or form alike —
             the extractor folds `media_ids[]=a&media_ids[]=b` to the same list"
        );
    }

    #[test]
    fn checked_media_caps_the_submitted_ids_and_keeps_their_order() {
        let a = Uuid::now_v7().to_string();
        let b = Uuid::now_v7().to_string();

        // The cap counts the submission as sent: five asked-for attachments
        // are over the default limit of four, distinct ids or no.
        let many: Vec<String> = (0..5).map(|_| Uuid::now_v7().to_string()).collect();
        let strs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(rejection(&with_media("hi", &strs)), TOO_MANY_ATTACHMENTS);
        let exact = with_media("hi", &strs[..4]);
        assert_eq!(
            accepted(&exact).media_ids.len(),
            4,
            "the limit itself is exactly attachable"
        );

        // An id submitted twice claims its row once, at its first position.
        let twice = with_media("hi", &[a.as_str(), b.as_str(), a.as_str()]);
        assert_eq!(
            accepted(&twice).media_ids,
            vec![
                Uuid::parse_str(&a).expect("an id"),
                Uuid::parse_str(&b).expect("an id"),
            ],
            "the order the client asked for survives, duplicates folded out"
        );

        // The cap reads the submission before the dedup, and off the
        // instance's own number: five spellings of one id are still five
        // asked-for attachments against a limit of two.
        let mut tight = Config::for_tests();
        tight.max_media_attachments = 2;
        let five_same = with_media("hi", &[a.as_str(); 5]);
        let err = check_body(&tight, "public", false, &five_same)
            .expect_err("five entries under a limit of two is the refusal");
        assert_eq!(err.to_string(), TOO_MANY_ATTACHMENTS);
    }

    #[test]
    fn media_id_text_that_is_no_id_is_media_not_found() {
        for spelling in ["not-an-id", "", "  "] {
            let body = with_media("hi", &[spelling]);
            assert_eq!(
                rejection(&body),
                BAD_MEDIA_ID,
                "`{spelling}` names no row anybody could claim"
            );
        }
        let mixed = with_media("hi", &[&Uuid::now_v7().to_string(), "nope"]);
        assert_eq!(
            rejection(&mixed),
            BAD_MEDIA_ID,
            "and one bad entry refuses the whole body"
        );
    }

    #[test]
    fn attachments_stand_in_for_a_missing_text() {
        let one = Uuid::now_v7().to_string();

        let media_only = with_media("   ", &[one.as_str()]);
        let checked = accepted(&media_only);
        assert_eq!(checked.text, "", "the trimmed nothing stands as the text");
        assert_eq!(checked.media_ids.len(), 1, "beside its one attachment");

        let nothing = with_media("   ", &[]);
        assert_eq!(
            rejection(&nothing),
            EMPTY_STATUS,
            "an empty attachment list is no attachment: the text still owes the post"
        );

        let bad = with_media("", &["not-an-id"]);
        assert_eq!(
            rejection(&bad),
            BAD_MEDIA_ID,
            "the media gate is read before the text it relaxes, Pleroma's pipeline order"
        );
    }

    #[tokio::test]
    async fn a_media_only_post_stores_the_row_and_claims_the_uploads_in_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "alice").await;
        let first_upload = upload(pool, &alice).await;
        let second_upload = upload(pool, &alice).await;

        // Posted newest first: the order the client asks for, not the order
        // the rows were minted in.
        let entity = created(
            &app_state,
            &alice,
            with_media(
                "",
                &[&second_upload.id.to_string(), &first_upload.id.to_string()],
            ),
        )
        .await;
        let row = stored(pool, &entity).await;
        assert_eq!(row.raw_text, "", "the post says nothing");
        assert_eq!(
            attachment_ids(&entity),
            vec![second_upload.id.to_string(), first_upload.id.to_string()],
            "the entity answers the attachments in the requested order"
        );

        let second = media_row(pool, second_upload.id).await;
        assert_eq!(
            second.status_id,
            Some(row.id),
            "the newest upload went first"
        );
        assert_eq!(second.position, 0, "at position zero");
        let first = media_row(pool, first_upload.id).await;
        assert_eq!(first.status_id, Some(row.id), "and the oldest follows it");
        assert_eq!(first.position, 1, "at the position the list gave it");
    }

    #[tokio::test]
    async fn media_the_poster_cannot_claim_refuses_the_whole_post() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "alice").await;
        let bob = user(pool, "bob").await;
        let foreign = upload(pool, &bob).await;
        let first_post = created(&app_state, &alice, posted("mine already stands")).await;
        let first_id = Uuid::parse_str(&first_post.id).expect("an id");
        let claimed = media_attachment::insert(
            pool,
            &NewMediaAttachment {
                user_id: Some(alice.id),
                status_id: Some(first_id),
                url: format!("{BASE}/media/fixture.png"),
                content_type: "image/png".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect("a fixture attachment inserts")
        .id;

        for (what, id) in [
            ("somebody else's upload", foreign.id),
            ("media a status already claims", claimed),
            ("an id no row holds", Uuid::now_v7()),
        ] {
            let mine = upload(pool, &alice).await;
            let body = with_media("hi", &[&mine.id.to_string(), &id.to_string()]);
            let err = create_status(&app_state, &alice, None, body)
                .await
                .expect_err("the unclaimable id above refuses the post");
            refused(&wire(err).await, BAD_MEDIA_ID);
            let mut live = live_ids(pool).await;
            assert_eq!(live.len(), 1, "{what}: the refused post wrote no row");
            assert_eq!(live.pop().expect("the one live id"), first_id);
            assert_eq!(
                media_row(pool, mine.id).await.status_id,
                None,
                "{what}: and the claimable upload rode the rollback out with it"
            );
        }
    }

    #[tokio::test]
    async fn every_visibility_can_carry_attachments() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "alice").await;

        for name in ["public", "unlisted", "private", "direct", "local"] {
            let mine = upload(pool, &alice).await;
            let entity = create_status(
                &app_state,
                &alice,
                None,
                CreateStatusParams {
                    visibility: Some(name.to_owned()),
                    ..with_media("a picture with words", &[&mine.id.to_string()])
                },
            )
            .await
            .unwrap_or_else(|err| panic!("`{name}` posts with media: {err}"));
            assert_eq!(
                entity.visibility, name,
                "`{name}` stands as asked, attachments and all"
            );
            assert_eq!(
                attachment_ids(&entity),
                vec![mine.id.to_string()],
                "`{name}` answers its attachment"
            );
            assert_eq!(
                media_row(pool, mine.id).await.status_id,
                Uuid::parse_str(&entity.id).ok(),
                "`{name}` claimed the row"
            );
        }
    }

    #[tokio::test]
    async fn a_replayed_create_restates_the_status_and_its_media() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "alice").await;
        let mine = upload(pool, &alice).await;

        let first = create_status_idempotent(
            &app_state,
            &alice,
            None,
            with_media("once", &[&mine.id.to_string()]),
            "media-key",
        )
        .await
        .expect("the keyed create posts");
        let again = create_status_idempotent(
            &app_state,
            &alice,
            None,
            with_media("ignored", &[]),
            "media-key",
        )
        .await
        .expect("the replay answers");

        assert_eq!(again.id, first.id, "one key, one status");
        assert_eq!(
            attachment_ids(&again),
            attachment_ids(&first),
            "and the replay restates its attachments"
        );
        assert_eq!(live_ids(pool).await.len(), 1, "no second row");
        assert_eq!(media_row(pool, mine.id).await.position, 0, "claimed once");
    }

    /// `n` distinct option texts, numbered from zero.
    fn option_texts(n: usize) -> Vec<String> {
        (0..n).map(|index| format!("option {index}")).collect()
    }

    /// The reading of [`check_body`] under the stock test config with one
    /// field changed by `update`: the error's text, or `None` when the
    /// body was accepted.
    fn rejection_under<F>(config: &Config, body: &CreateStatusParams, update: F) -> Option<String>
    where
        F: FnOnce(&mut Config),
    {
        let mut owned = config.clone();
        update(&mut owned);
        check_body(&owned, "public", false, body)
            .map(|_| ())
            .map_err(|err| err.to_string())
            .err()
    }

    /// An accepted poll rides out of [`check_body`] as the [`CheckedPoll`]
    /// the create will store: options in order, the submitted
    /// span computed into a closing instant, and both flags as posted. A
    /// body without a poll leaves the field `None`; a `false` spelling is
    /// a decision, not an absence.
    #[test]
    fn an_accepted_poll_is_carried_in_full() {
        let absent = params(json!({"status": "hi"}));
        assert!(accepted(&absent).poll.is_none(), "no poll key, no poll");

        let body = params(json!({
            "status": "vote please",
            "poll": {
                "options": ["yes", "no"],
                "expires_in": 3600,
                "multiple": true,
                "hide_totals": true,
            },
        }));
        let poll = accepted(&body)
            .poll
            .expect("the checked body carries its poll");
        assert_eq!(poll.options, vec!["yes", "no"], "options keep their order");
        let due = poll
            .expires_at
            .unwrap_or_else(|| panic!("3600 seconds is a closing instant"));
        let span = due - Utc::now();
        assert!(
            (TimeDelta::seconds(3600) - span).num_seconds().abs() <= 1,
            "the poll closes about an hour out, not {span}"
        );
        assert!(poll.multiple, "the submitted any-choice flag");
        assert!(poll.hide_totals, "the submitted hidden-totals flag");

        let plain = params(json!({"status": "hi", "poll": {"options": ["a", "b"]}}));
        let plain = accepted(&plain).poll.expect("the bare poll is carried too");
        assert_eq!(
            plain.expires_at, None,
            "no expires_in is a poll that never closes"
        );
        assert!(!plain.multiple, "an absent flag is `false`, not undecided");
        assert!(!plain.hide_totals, "and so is the other one");

        let off = params(json!({
            "status": "hi",
            "poll": {"options": ["a", "b"], "multiple": false, "hide_totals": "off"},
        }));
        let off = accepted(&off)
            .poll
            .expect("a submitted `false` is as valid as a `true`");
        assert!(!off.multiple, "`false` stays false");
        assert!(!off.hide_totals, "and the form spelling `off` does too");
    }

    /// Fewer than two options — including a poll that named none at all,
    /// which the `serde(default)` keeps a service decision rather than an
    /// extractor error — is Pleroma's minimum refusal; two is the minimum
    /// met.
    #[test]
    fn a_poll_needs_at_least_two_options() {
        for poll in [
            json!({}),
            json!({"options": []}),
            json!({"options": ["only"]}),
        ] {
            let body = params(json!({"status": "hi", "poll": poll}));
            assert_eq!(
                rejection(&body),
                TOO_FEW_POLL_OPTIONS,
                "`{poll}` is less than a question"
            );
        }
        let body = params(json!({"status": "hi", "poll": {"options": ["a", "b"]}}));
        assert!(
            accepted(&body).poll.is_some(),
            "two options is the minimum met"
        );
    }

    /// The option ceiling at its exact boundary: twenty pass, twenty-one
    /// are refused, and the message names the configured number.
    #[test]
    fn a_poll_cannot_carry_more_options_than_the_ceiling() {
        let config = Config::for_tests();
        assert_eq!(
            config.max_poll_options, 20,
            "the stock ceiling is the twenty this test counts options against"
        );
        let exact = params(json!({"status": "hi", "poll": {"options": option_texts(20)}}));
        let checked = accepted(&exact)
            .poll
            .expect("twenty options are within the ceiling");
        assert_eq!(checked.options.len(), 20, "and all of them ride along");
        let over = params(json!({"status": "hi", "poll": {"options": option_texts(21)}}));
        assert_eq!(
            rejection(&over),
            "Poll can't contain more than 20 options",
            "one past the ceiling is refused, naming it"
        );
    }

    /// The ceiling is the configuration's, not a welded 20: an instance
    /// that raised it takes twenty-one, and one that lowered it names its
    /// own number in its own message.
    #[test]
    fn the_poll_option_ceiling_is_configurable() {
        let two = params(json!({"status": "hi", "poll": {"options": ["a", "b"]}}));
        let tight = rejection_under(&Config::for_tests(), &two, |config| {
            config.max_poll_options = 1;
        });
        assert_eq!(
            tight.expect("a ceiling of one refuses the two-option minimum"),
            "Poll can't contain more than 1 options",
            "the message names the configured number, not the shipped one"
        );
        let roomy = params(json!({"status": "hi", "poll": {"options": option_texts(21)}}));
        let opened = rejection_under(&Config::for_tests(), &roomy, |config| {
            config.max_poll_options = 21;
        });
        assert!(
            opened.is_none(),
            "the 21 the stock config refuses pass one that raised the ceiling to 21"
        );
    }

    /// The option-length rule at its exact boundaries: 200 characters pass,
    /// 201 are refused, and an empty option is refused with the same
    /// message — the one wording Pleroma's rule covers broken options
    /// with.
    #[test]
    fn a_poll_option_must_not_be_empty_or_over_long() {
        let exact = params(json!({
            "status": "hi",
            "poll": {"options": ["x".repeat(200), "b"]},
        }));
        assert!(
            accepted(&exact).poll.is_some(),
            "exactly 200 characters is still within the cap"
        );
        let over = params(json!({
            "status": "hi",
            "poll": {"options": ["a", "x".repeat(201)]},
        }));
        assert_eq!(
            rejection(&over),
            "Poll options cannot be longer than 200 characters",
            "one character past the cap is the refusal, naming it"
        );
        let empty = params(json!({"status": "hi", "poll": {"options": ["", "b"]}}));
        assert_eq!(
            rejection(&empty),
            "Poll options cannot be longer than 200 characters",
            "an empty option is refused with the same sentence"
        );
    }

    /// The length cap is the configuration's too: a short-capped instance
    /// measures and names its own number.
    #[test]
    fn the_poll_option_cap_is_configurable() {
        let body = params(json!({
            "status": "hi",
            "poll": {"options": ["0123456789", "eleven chars"]},
        }));
        let refused = rejection_under(&Config::for_tests(), &body, |config| {
            config.max_poll_option_chars = 10;
        });
        assert_eq!(
            refused.expect("the 11-character option breaks the configured cap"),
            "Poll options cannot be longer than 10 characters",
            "the message names the configured cap"
        );
    }

    /// The expiration window: 299 (and every shorter span, negatives
    /// included) are too soon, the 300 floor and the `31_536_000` ceiling
    /// pass, and `31_536_001` is too far — each with Pleroma's own words.
    #[test]
    fn the_poll_expiration_window_refuses_the_spans_it_cannot_take() {
        let posted = |secs: i64| {
            params(json!({
                "status": "hi",
                "poll": {"options": ["a", "b"], "expires_in": secs},
            }))
        };
        for short in [-1_i64, 0, 1, 299] {
            assert_eq!(
                rejection(&posted(short)),
                POLL_EXPIRATION_TOO_SOON,
                "{short} seconds is under the 300-second floor"
            );
        }
        assert!(
            accepted(&posted(300)).poll.is_some(),
            "the floor itself passes"
        );
        assert!(
            accepted(&posted(31_536_000)).poll.is_some(),
            "and the one-year ceiling itself passes"
        );
        assert_eq!(
            rejection(&posted(31_536_001)),
            POLL_EXPIRATION_TOO_FAR,
            "one second past the ceiling is too far"
        );
    }

    /// Both window bounds come from the configuration, and the stock
    /// window still refuses what an opened one takes.
    #[test]
    fn the_poll_expiration_window_is_configurable() {
        let short = params(json!({
            "status": "hi",
            "poll": {"options": ["a", "b"], "expires_in": 60},
        }));
        let lowered = rejection_under(&Config::for_tests(), &short, |config| {
            config.min_poll_expiration_secs = 30;
        });
        assert!(lowered.is_none(), "60 seconds pass a 30-second floor");
        assert_eq!(
            rejection(&short),
            POLL_EXPIRATION_TOO_SOON,
            "the stock 300-second floor still refuses it"
        );
        let far = params(json!({
            "status": "hi",
            "poll": {"options": ["a", "b"], "expires_in": 172_800},
        }));
        let shortened = rejection_under(&Config::for_tests(), &far, |config| {
            config.max_poll_expiration_secs = 86_400;
        });
        assert_eq!(
            shortened.expect("two days are past a one-day ceiling"),
            POLL_EXPIRATION_TOO_FAR,
            "the message is the same whatever the number was"
        );
    }

    /// A poll beside attachments is the conflict refusal; an empty
    /// `media_ids` list is no attachment at all, and `media_ids` without
    /// a poll is the attachments field — accepted here, claimed or
    /// refused by the write.
    #[test]
    fn a_poll_cannot_have_attachments() {
        let with_media = params(json!({
            "status": "hi",
            "poll": {"options": ["a", "b"]},
            "media_ids": ["01934567-89ab-7cde-f012-3456789abcde"],
        }));
        assert_eq!(
            rejection(&with_media),
            POLL_WITH_MEDIA,
            "a question over a picture is refused"
        );
        let empty_list = params(json!({
            "status": "hi",
            "poll": {"options": ["a", "b"]},
            "media_ids": [],
        }));
        assert!(
            accepted(&empty_list).poll.is_some(),
            "an empty id list names no attachment, so there is no conflict"
        );
        let media_only = params(json!({
            "status": "hi",
            "media_ids": ["01934567-89ab-7cde-f012-3456789abcde"],
        }));
        let checked = accepted(&media_only);
        assert!(
            checked.poll.is_none(),
            "media_ids without a poll is no conflict — just the attachment ids"
        );
    }

    /// A poll carries an otherwise empty status: the empty refusal is
    /// skipped, the length cap is not lifted, and the poll rules run
    /// between the text and the visibility — a body breaking a poll rule
    /// and a scope rule is refused for the poll, and one breaking the
    /// poll's window and the top-level `expires_in` for the poll.
    #[test]
    fn a_poll_stands_in_for_the_text_and_runs_before_the_visibility() {
        let blank = params(json!({"status": "   ", "poll": {"options": ["a", "b"]}}));
        let checked = accepted(&blank);
        assert_eq!(checked.text, "", "the trimmed nothing is stored as nothing");
        assert!(checked.poll.is_some(), "and the question carries the post");

        let limit = Config::for_tests().max_status_chars;
        let over = params(json!({
            "status": "x".repeat(limit + 1),
            "poll": {"options": ["a", "b"]},
        }));
        assert_eq!(
            rejection(&over),
            OVER_LIMIT,
            "the poll does not lift the status length cap"
        );

        let bad_poll_and_bad_scope = params(json!({
            "status": "hi",
            "visibility": "sideways",
            "poll": {"options": ["only"]},
        }));
        assert_eq!(
            rejection(&bad_poll_and_bad_scope),
            TOO_FEW_POLL_OPTIONS,
            "the poll is refused before the visibility is looked at"
        );

        let bad_poll_and_bad_expiry = params(json!({
            "status": "",
            "poll": {"options": ["a", "b"], "expires_in": 1},
            "expires_in": -5,
        }));
        assert_eq!(
            rejection(&bad_poll_and_bad_expiry),
            POLL_EXPIRATION_TOO_SOON,
            "the poll's window answers before the top-level `checked_expiration` gets its turn"
        );
    }

    /// A `direct` poll is a private question, which Pleroma posts: the
    /// audience check neither sees nor refuses the poll.
    #[test]
    fn a_direct_poll_is_allowed() {
        let body = params(json!({
            "status": "vote quietly",
            "visibility": "direct",
            "poll": {"options": ["a", "b"]},
        }));
        let checked = accepted(&body);
        assert_eq!(
            checked.visibility,
            Visibility::Direct,
            "the audience posted"
        );
        assert!(checked.poll.is_some(), "and the poll carried");
    }

    /// Repeated options are the poster's business: the count bounds are
    /// kept, the texts are not deduplicated.
    #[test]
    fn duplicate_poll_options_are_allowed() {
        let body = params(json!({"status": "hi", "poll": {"options": ["same", "same"]}}));
        let poll = accepted(&body).poll.expect("duplicates are no rule broken");
        assert_eq!(
            poll.options,
            vec!["same", "same"],
            "both copies ride along as posted"
        );
    }

    /// The poll arrives the same from JSON and from form keys: the
    /// extractor nests `poll[options][]` into the option list and every
    /// other `poll[...]` value is a [`flex`] string the deserializers
    /// read; anything that is no poll shape at all stays the extractor's
    /// 422.
    ///
    /// [`flex`]: crate::extract::flex
    #[test]
    fn the_poll_deserializes_from_the_json_and_form_spellings() {
        let from_json = params(json!({
            "status": "pick",
            "poll": {
                "options": ["a", "b"],
                "expires_in": 3600,
                "multiple": true,
                "hide_totals": false,
            },
        }));
        assert!(from_json.poll.is_some(), "the json shape is a poll request");

        let form = crate::extract::parse_pairs([
            ("status", "pick"),
            ("poll[options][]", "a"),
            ("poll[options][]", "b"),
            ("poll[expires_in]", "3600"),
            ("poll[multiple]", "true"),
            ("poll[hide_totals]", "on"),
            ("media_ids[]", "id-1"),
        ]);
        let from_form: CreateStatusParams =
            crate::extract::into_params(form).expect("the form shape fits the same type");
        let poll = from_form.poll.expect("the nested keys became a poll");
        assert_eq!(
            poll.options,
            vec!["a", "b"],
            "the append keys are the option list"
        );
        assert_eq!(
            poll.expires_in,
            Some(3_600),
            "the string number is the flex integer"
        );
        assert_eq!(
            poll.multiple,
            Some(true),
            "the string boolean is the flex boolean"
        );
        assert_eq!(poll.hide_totals, Some(true), "`on` means true");
        assert_eq!(
            from_form.media_ids,
            Some(vec!["id-1".to_owned()]),
            "and `media_ids[]` nests beside it"
        );

        for broken in [
            json!("yes"),
            json!({"options": "ab"}),
            json!({"options": ["a", "b"], "expires_in": "soon"}),
            json!({"options": ["a", "b"], "multiple": "maybe"}),
        ] {
            let parsed = crate::extract::into_params::<CreateStatusParams>(
                json!({"status": "hi", "poll": broken}),
            );
            assert!(
                parsed.is_err(),
                "`{broken}` is no poll and must not deserialize"
            );
        }
    }

    // ────────────  poll persistence through the service  ────────────

    /// A create whose body carries a poll stores the `polls` row and its
    /// options beside the status — and the entity answers the whole
    /// [`Poll`]: the stored row's id, options in submission order at zero
    /// votes, the deadline the posted span counts out, and the posted
    /// flags. The status's own expiry stays what the body's top-level
    /// `expires_in` said, which here is nothing.
    ///
    /// [`Poll`]: crate::entities::poll::Poll
    #[tokio::test]
    async fn a_polled_create_stores_the_poll_and_renders_its_entity() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "balloter").await;

        let before = Utc::now();
        let entity = created(
            &app_state,
            &alice,
            params(json!({
                "status": "ballot",
                "poll": {
                    "options": ["a", "b"],
                    "expires_in": 3600,
                    "multiple": true,
                    "hide_totals": false,
                },
            })),
        )
        .await;
        let row = stored(pool, &entity).await;
        let stand = poll::get_for_status(pool, row.id)
            .await
            .expect("the poll reads")
            .expect("the create stored the poll beside its status");

        assert_eq!(stand.status_id, row.id, "the poll names its status");
        assert!(stand.multiple, "the posted any-choice flag is stored");
        assert!(
            !stand.hide_totals,
            "and so is the posted show-totals decision"
        );
        let due = stand.expires_at.expect("an hour-out deadline");
        let span = due - before;
        assert!(
            (TimeDelta::seconds(3600) - span).num_seconds().abs() <= 5,
            "the stored instant is now plus the posted span, not {span}"
        );
        assert_eq!(stand.voters_count, 0, "a fresh poll counts nothing");
        assert_eq!(stand.votes_count, 0, "in either column");

        let options = poll::options(pool, stand.id)
            .await
            .expect("the options read");
        let titles: Vec<&str> = options.iter().map(|found| found.title.as_str()).collect();
        assert_eq!(titles, vec!["a", "b"], "options store in submission order");
        assert!(
            options.iter().all(|found| found.votes_count == 0),
            "a fresh option has no votes"
        );

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["poll"]["id"],
            stand.id.to_string(),
            "the entity answers the stored poll's id"
        );
        assert_eq!(
            value["poll"]["options"],
            json!([
                {"title": "a", "votes_count": 0},
                {"title": "b", "votes_count": 0},
            ]),
            "options render in order with their fresh counts"
        );
        assert_eq!(value["poll"]["multiple"], true, "the posted flags render");
        assert_eq!(value["poll"]["expired"], false, "an hour out is wide open");
        assert!(
            value["poll"]["expires_at"].is_string(),
            "the deadline renders as a timestamp"
        );
        assert_eq!(
            value["pleroma"]["expires_at"],
            Value::Null,
            "the poll's closing time is not the status's deletion time"
        );
        assert_eq!(
            poll_rows(pool).await,
            1,
            "exactly one polls row was written"
        );
    }

    /// A body whose poll carries only the options — no span, no flags —
    /// stores the never-closing single-choice question the checked body
    /// decided, and the entity agrees: `expires_at` null, `expired` false,
    /// `multiple` false.
    #[tokio::test]
    async fn a_bare_poll_stores_the_defaults_the_check_computed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "plain-balloter").await;

        let entity = created(&app_state, &alice, polled("which one?")).await;
        let row = stored(pool, &entity).await;
        let stand = poll::get_for_status(pool, row.id)
            .await
            .expect("the poll reads")
            .expect("the create stored the poll");

        assert!(
            stand.expires_at.is_none(),
            "no expires_in is a poll that never closes"
        );
        assert!(!stand.multiple, "an absent flag stores `false`");
        assert!(!stand.hide_totals, "and so does the other one");

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(value["poll"]["expires_at"], Value::Null);
        assert_eq!(
            value["poll"]["expired"], false,
            "never closes, never closed"
        );
        assert_eq!(value["poll"]["multiple"], false);
        assert_eq!(
            value["poll"]["votes_count"], 0,
            "the roll-ups start at zero"
        );
        assert_eq!(value["poll"]["voters_count"], 0);
        assert_eq!(value["poll"]["voted"], false, "the author has not voted");
        assert_eq!(value["poll"]["own_votes"], json!([]));
        assert_eq!(
            value["poll"]["options"],
            json!([
                {"title": "a", "votes_count": 0},
                {"title": "b", "votes_count": 0},
            ]),
            "the checked options reach the rows and the render in order"
        );
    }

    /// A preview whose body carries a poll runs the whole check and render
    /// and stores nothing: the entity answers `poll: null`, the `polls`
    /// and `poll_options` tables stay as empty as the `statuses` one, and
    /// the poll still stood in for the empty text the way a post's would.
    #[tokio::test]
    async fn a_previewed_poll_answers_null_and_is_stored_nowhere() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "poll-looker").await;

        let entity = preview_status(
            &app_state,
            &alice,
            None,
            &params(json!({
                "status": "   ",
                "preview": true,
                "poll": {
                    "options": ["a", "b"],
                    "expires_in": 3600,
                    "multiple": true,
                },
            })),
        )
        .await
        .expect("the poll stands in for the empty text, previewed or not");

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["poll"],
            Value::Null,
            "a preview has stored nothing to render"
        );
        assert!(live_ids(pool).await.is_empty(), "no status row");
        assert_eq!(poll_rows(pool).await, 0, "no polls row");
        let options: (i64,) = sqlx::query_as("SELECT count(*) FROM poll_options")
            .fetch_one(pool)
            .await
            .expect("the option count reads");
        assert_eq!(options.0, 0, "and no option rows either");
        assert_eq!(key_rows(pool).await, 0, "and no key was claimed");
        let counted = crate::models::user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(counted.statuses_count, 0, "and no count moved");
    }

    /// The `after_status_created` hook pushes Mastodon's `update` with the
    /// render hint onto the hub the moment the row is written — the create
    /// path a remote ingestion later shares — and the preview arm, which
    /// writes nothing, fires nothing.
    #[tokio::test]
    async fn the_create_hook_pushes_the_update_event() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app_state = state(&pool);
        let alice = user(&pool, "hooking-author").await;
        let mut rx = app_state.hub.subscribe();

        let entity = created(&app_state, &alice, posted("streamed from the hook")).await;

        let event = rx.recv().await.expect("the create hook published an event");
        assert_eq!(event.event, "update", "the fresh post arrives");
        let hinted = Uuid::parse_str(&entity.id).expect("the entity names its status");
        assert_eq!(
            event.render,
            Some(crate::streaming::hub::RenderSpec::Status(hinted)),
            "the push carries the created status's render hint"
        );
        assert!(
            event.streams.contains(&"public".to_owned()),
            "a public post rides the public timeline: {:?}",
            event.streams
        );
        assert!(event.payload.is_none(), "the entity is rendered per socket");
    }

    /// Drains every event already buffered on `rx`: the hub hands a publish to
    /// its local subscribers synchronously, so an awaited emitter call is all
    /// here by the time this reads.
    fn drained(
        rx: &mut tokio::sync::broadcast::Receiver<Arc<crate::streaming::hub::StreamEvent>>,
    ) -> Vec<crate::streaming::hub::StreamEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(Arc::unwrap_or_clone(event));
        }
        events
    }

    /// `author`'s `direct` status addressing everyone in `mentioned`, written
    /// through the model — the unthreaded row the create hook's conversation
    /// arm then records and announces.
    async fn direct_status(
        pool: &PgPool,
        author: &User,
        mentioned: &[&User],
        text: &str,
    ) -> status::Status {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                visibility: Visibility::Direct,
                mentioned_user_ids: mentioned.iter().map(|who| who.id).collect(),
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("the direct fixture inserts")
    }

    /// A mirror of the remote actor `nickname@remote.example`.
    async fn remote_user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id: format!("https://remote.example/users/{nickname}"),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// `reader`'s OWN participation id in the thread `status_id` was recorded
    /// into — the id a conversation hint must name for that reader.
    async fn participation_id(pool: &PgPool, status_id: Uuid, reader: Uuid) -> Uuid {
        let thread = status::get_by_id(pool, status_id)
            .await
            .expect("the row reads")
            .expect("the fixture is live")
            .direct_conversation_id
            .expect("the conversation arm threaded the fixture");
        conversation::participation_ids(pool, reader, &[thread])
            .await
            .expect("the participation reads")
            .get(&thread)
            .copied()
            .expect("the reader is a local participant of the fixture thread")
    }

    /// A `direct` status by a local author to a local addressee tells both
    /// local sides on their own `direct` stream, each with their OWN
    /// participation id as the render hint — the shape `conversation` clients
    /// render their conversation list from.
    #[tokio::test]
    async fn a_direct_status_publishes_a_conversation_event_per_local_participant() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app_state = state(&pool);
        let alice = user(&pool, "dm-author").await;
        let bob = user(&pool, "dm-recipient").await;
        let dm = direct_status(&pool, &alice, &[&bob], "@dm-recipient psst").await;
        let mut rx = app_state.hub.subscribe();

        participate_in_conversation(&app_state, &dm).await;

        let events = drained(&mut rx);
        assert_eq!(events.len(), 2, "one event per local side of the thread");
        for event in &events {
            assert_eq!(event.event, "conversation", "Mastodon's conversation event");
            assert!(event.payload.is_none(), "no body: the socket renders it");
            assert!(
                !event.local_only,
                "a conversation update is not flagged local-only"
            );
        }
        let alice_participation = participation_id(&pool, dm.id, alice.id).await;
        let bob_participation = participation_id(&pool, dm.id, bob.id).await;
        assert_ne!(
            alice_participation, bob_participation,
            "each side owns its own participation row"
        );
        let alice_event = events
            .iter()
            .find(|event| event.streams == vec![format!("direct:{}", alice.id)])
            .expect("the author is told on her own direct stream");
        assert_eq!(
            alice_event.audience,
            crate::streaming::hub::Audience::Users(vec![alice.id]),
            "the reader is the whole audience"
        );
        assert_eq!(
            alice_event.render,
            Some(crate::streaming::hub::RenderSpec::Conversation(
                alice_participation
            )),
            "alice's hint names her OWN participation"
        );
        let bob_event = events
            .iter()
            .find(|event| event.streams == vec![format!("direct:{}", bob.id)])
            .expect("the local addressee is told on his own direct stream");
        assert_eq!(
            bob_event.audience,
            crate::streaming::hub::Audience::Users(vec![bob.id]),
            "the reader is the whole audience"
        );
        assert_eq!(
            bob_event.render,
            Some(crate::streaming::hub::RenderSpec::Conversation(
                bob_participation
            )),
            "bob's hint names his OWN participation"
        );
    }

    /// A status of any visibility but `direct` records no thread and so
    /// announces no conversation: the arm is a no-op beside the write.
    #[tokio::test]
    async fn a_non_direct_status_publishes_no_conversation_event() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app_state = state(&pool);
        let alice = user(&pool, "private-author").await;
        let bob = user(&pool, "private-tagged").await;
        let post = status::insert(
            &pool,
            BASE,
            &NewStatus {
                visibility: Visibility::Private,
                mentioned_user_ids: vec![bob.id],
                ..NewStatus::local(&alice, "@private-tagged hello")
            },
        )
        .await
        .expect("the private fixture inserts");
        let mut rx = app_state.hub.subscribe();

        participate_in_conversation(&app_state, &post).await;

        assert!(
            drained(&mut rx).is_empty(),
            "a followers-only post threads nothing and tells no conversation"
        );
    }

    /// A `direct` status addressed to a remote account as well as a local one
    /// tells every LOCAL participant and nothing for the remote: a remote
    /// account owns no participation row, so it is absent from the roster the
    /// arm publishes to.
    #[tokio::test]
    async fn a_direct_status_publishes_nothing_for_a_remote_participant() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app_state = state(&pool);
        let alice = user(&pool, "federation-author").await;
        let bob = user(&pool, "federation-recipient").await;
        let carol = remote_user(&pool, "federation-carol").await;
        let dm = direct_status(&pool, &alice, &[&bob, &carol], "a mixed DM").await;
        let mut rx = app_state.hub.subscribe();

        participate_in_conversation(&app_state, &dm).await;

        let events = drained(&mut rx);
        assert_eq!(events.len(), 2, "only the two local sides are told");
        assert!(
            events.iter().all(|event| event
                .streams
                .iter()
                .all(|stream| *stream != format!("direct:{}", carol.id))),
            "the remote addressee owns no participation and is published to nothing"
        );
    }
}
