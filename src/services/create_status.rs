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
//! row, renders the checked text, resolves its mentions, stores the row and
//! its mention and hashtag links through [`status::insert`], and answers the
//! [`Status`] entity rendered for the author — Pleroma's `for: user`. The
//! endpoint's `poll` parameter joined [`CreateStatusParams`] with this
//! ticket as the `poll[options]`/`poll[expires_in]`/`poll[multiple]`/
//! `poll[hide_totals]` shape the [`ApiBody`] extractor already nests, and
//! [`check_body`] runs Pleroma's `CommonAPI` poll rules over it; the checked
//! poll travels on [`Checked::poll`] for the persistence ticket, which is
//! the one that stores it — until then a body whose poll is accepted still
//! creates a plain status whose entity answers `poll: null`. The two
//! remaining parameters — `in_reply_to_id` and `scheduled_at` — join
//! [`CreateStatusParams`] as further `Option`s with their own tickets
//! (decision 15), and until then a body that sends them is only a body
//! whose known fields are checked here.
//!
//! [`ApiBody`]: crate::extract::ApiBody
//!
//! The module is also the shared half of `PUT /api/v1/statuses/{id}`:
//! [`edit_status`] merges the edit body over the stored row, pushes the
//! merge through the very same [`check_body`], and renders the result
//! through [`render_status_fields`] — the function that turns checked text
//! into its stored HTML, its mention links and its hashtags, and the reason
//! an edit renders a post exactly the way a creation would. The edit's own
//! `media_ids` parameter resolves to the ids the author may attach,
//! Pleroma's `attachments_from_ids` dropping the ids that name nothing
//! (creation carries the parameter too, but only its poll check reads it —
//! see [`CreateStatusParams`]);
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
//! zero counters.
//!
//! [`Status`]: crate::entities::status::Status
//! [`status::insert`]: crate::models::status::insert
//!
//! Every refusal is an [`AppError::Unprocessable`] — a 422 whose body is
//! `{"error":"…"}` (decision 8) — in Pleroma's or Mastodon's own wording,
//! pinned by the tests below: the two limit messages, the poll refusals of
//! `checked_poll` — `Poll must contain at least 2 options`, `Poll can't
//! contain more than <max> options`, `Poll options cannot be longer than
//! <chars> characters`, `Expiration date is too soon`, `Expiration date is
//! too far in the future` and `Polls cannot have attachments`, `Invalid
//! visibility` for a submitted scope outside the five [`Visibility`] names,
//! `Invalid content_type` for a MIME type outside `CONTENT_TYPES`, Rails'
//! too-long sentence for a content warning or a language tag past its cap,
//! and the `expires_in` pair — `expires_in must be at least <floor> seconds`
//! naming this instance's floor for a shorter span, `expires_in is too
//! large` for a span no timestamp can hold.

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;
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
        hashtag::Hashtag,
        idempotency_key,
        oauth_app::OauthApp,
        status::{self, DEFAULT_CONTENT_TYPE, NewStatus, Visibility},
        status_edit,
        user::User,
    },
    services::{mentions::resolve_mentions, render_status::render_status},
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

/// 422 body for text that leaves nothing behind once trimmed: Pleroma's
/// `validate_character_limit` wording. A body carrying a poll skips this
/// refusal (Pleroma posts the question with no text), and an
/// attachments-only post stays a refusal until the persistence ticket gives
/// `media_ids` a meaning beyond the poll check.
const EMPTY_STATUS: &str = "Cannot post an empty status without attachments";

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

/// The body of `POST /api/v1/statuses`, as the client wrote it.
///
/// Every field is an `Option` with a serde `default`, so a key the body omits
/// or sends as `null` stays this module's decision instead of becoming a
/// deserialization error. `poll` and `media_ids` joined with this ticket —
/// the first checked by [`check_body`] and carried on [`Checked::poll`] for
/// the persistence ticket, the second read *only* by this ticket's poll
/// check (`media_ids` beside a poll is the 422 `Polls cannot have
/// attachments`; the create itself ignores it — attachments reach a status
/// no other way until a later ticket wires them). `in_reply_to_id` and
/// `scheduled_at` join the struct with their own tickets (decision 15),
/// the same way. The `sensitive` field is a [`flex`] boolean, so a form
/// body's `sensitive=on` spells what a JSON body's `true` spells.
///
/// [`flex`]: crate::extract::flex
#[derive(Debug, Default, Deserialize)]
pub struct CreateStatusParams {
    /// The post text; trimmed, and required to leave something behind.
    #[serde(default)]
    status: Option<String>,
    /// The content warning shown in place of the post; at most
    /// `MAX_SPOILER_CHARS` characters, and the empty string when absent.
    #[serde(default)]
    spoiler_text: Option<String>,
    /// Whether the post starts folded; the author's own default when the body
    /// does not say.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    sensitive: Option<bool>,
    /// The requested audience, one of the five [`Visibility`] names; the
    /// author's default scope when absent.
    #[serde(default)]
    visibility: Option<String>,
    /// The content language tag; at most `MAX_LANGUAGE_CHARS` characters,
    /// and no tag at all when absent or empty.
    #[serde(default)]
    language: Option<String>,
    /// The MIME type of the `status` text; [`DEFAULT_CONTENT_TYPE`] when
    /// absent.
    #[serde(default)]
    content_type: Option<String>,
    /// Whether this is a preview: validate and render, answer the entity a
    /// create would have, write nothing — see [`preview_status`]. Absent,
    /// null and every [`flex`] false spelling are a real post.
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    preview: Option<bool>,
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
    expires_in: Option<i64>,
    /// The attachment ids the body names, as id text: JSON's
    /// `["a","b"]` or the form's `media_ids[]=a&media_ids[]=b`, both
    /// nested into a list by the [`ApiBody`] extractor's key rules
    /// (decision 10). Creation itself ignores this field; the one check
    /// it takes part in is the poll rule that refuses a poll sent
    /// beside non-empty ids (`Polls cannot have attachments`). Storing
    /// attachments for a fresh post is a later ticket's work, like the
    /// poll's storage.
    ///
    /// [`ApiBody`]: crate::extract::ApiBody
    #[serde(default)]
    media_ids: Option<Vec<String>>,
    /// The poll the body asks for, in the shape the [`ApiBody`]
    /// extractor nests out of `poll[options][]`, `poll[expires_in]`,
    /// `poll[multiple]` and `poll[hide_totals]` form keys — or out of
    /// the equivalent JSON object. [`check_body`] runs Pleroma's
    /// `CommonAPI` rules over it and carries the result as
    /// [`Checked::poll`] for the persistence ticket; until that ticket
    /// wires the storage, an accepted poll still creates a plain
    /// status whose entity answers `poll: null`.
    ///
    /// [`ApiBody`]: crate::extract::ApiBody
    #[serde(default)]
    poll: Option<PollRequest>,
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
/// and answers the [`CheckedPoll`] the persistence ticket stores.
///
/// [`ApiBody`]: crate::extract::ApiBody
/// [`flex`]: crate::extract::flex
#[derive(Debug, Deserialize)]
pub struct PollRequest {
    /// The options as posted, in order. `#[serde(default)]` lets a poll
    /// that names no options at all reach `checked_poll`'s minimum of
    /// two rather than failing the extractor.
    #[serde(default)]
    options: Vec<String>,
    /// Seconds from now until the poll closes; a [`flex`] integer, with
    /// absent or null meaning a poll that never closes. Accepted values
    /// run from [`Config::min_poll_expiration_secs`] to
    /// [`Config::max_poll_expiration_secs`].
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_i64")]
    expires_in: Option<i64>,
    /// Whether one voter may mark several options; a [`flex`] boolean,
    /// `false` when absent or null.
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    multiple: Option<bool>,
    /// Whether the per-option counts stay hidden while the poll runs; a
    /// [`flex`] boolean, `false` when absent or null.
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    hide_totals: Option<bool>,
}

/// The poll [`check_body`] accepted.
///
/// What its `checked_poll` step measured and computed, ready for the
/// persistence ticket to hand to [`crate::models::poll::create`] beside
/// its status row. Nothing stores it until that ticket lands.
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
    /// The poll the body asked for, after [`check_body`]'s poll rules —
    /// see `checked_poll` — or `None` when the body carried none. The
    /// persistence ticket's payload: nothing of it is stored (and the
    /// entity answers `poll: null`) until that ticket wires the write.
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
/// The rules run in this order — the text (empty unless a poll rides
/// along, then too long), the poll (`checked_poll`, Pleroma's
/// `CommonAPI` rules: options, expiration, `media_ids`), the content
/// warning, the language, the visibility, the content type, then the
/// expiration — so a body that breaks several is refused for the first of
/// them.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `EMPTY_STATUS` (skipped when the body
/// carries a poll, whose question stands in for the text), `OVER_LIMIT`,
/// `INVALID_VISIBILITY` or `INVALID_CONTENT_TYPE`, with `checked_poll`'s
/// six refusals for a poll that breaks one of its rules, with Rails'
/// too-long wording for a content warning or a language tag past its cap,
/// and with `checked_expiration`'s refusal for an `expires_in` under the
/// instance's floor or past the calendar.
pub fn check_body<'a>(
    config: &Config,
    default_scope: &str,
    default_sensitive: bool,
    params: &'a CreateStatusParams,
) -> AppResult<Checked<'a>> {
    let has_poll = params.poll.is_some();
    let text = check_text(
        config,
        params.status.as_deref().unwrap_or_default(),
        has_poll,
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
        poll,
    })
}

/// The post text: trimmed, non-empty unless the body carries a poll —
/// Pleroma lets a question be posted with no text — and no longer than
/// [`Config::max_status_chars`] measured with [`status_length`]'s
/// weighting, which a poll does not lift.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`EMPTY_STATUS`] or [`OVER_LIMIT`].
fn check_text<'a>(config: &Config, text: &'a str, has_poll: bool) -> AppResult<&'a str> {
    let trimmed = text.trim();
    if trimmed.is_empty() && !has_poll {
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
/// dedups at render, which is the persistence ticket's side of this
/// implementation); then every option non-empty and no longer than
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
/// is computed once here — the same value the persistence ticket stores
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
/// pair off the user — and [`create_status`] does the rest: the render and
/// the entity lists (the [`render_status_fields`] step below), the stored row
/// with its mention and hashtag links and the author's moved counters
/// ([`status::insert`]), and the render for the author. `app` — the client
/// that issued the presenting
/// token, `None` when the caller names none — is recorded as the post's
/// `application` pair, Pleroma's `put_application`.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for the body's own refusals, through
/// [`check_body`]; [`AppError::Internal`] for the one row this function
/// wrote that the renderer refuses to answer (unreachable for a fresh local
/// post, which is neither a tombstone nor a reblog). Any database failure
/// arrives through `?` as [`AppError::Database`].
pub async fn create_status(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    params: CreateStatusParams,
) -> AppResult<Status> {
    let checked = check_body(
        &state.config,
        &user.default_scope,
        user.sensitive_default.get(),
        &params,
    )?;
    let entities = render_status_fields(state, checked.text, checked.content_type).await?;
    let row = status::insert(
        &state.db,
        &state.config.base_url,
        &new_status(user, app, &checked, &entities),
    )
    .await?;
    created_entity(state, user, row).await
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
    let entity = render_status(state, Some(user), &row)
        .await?
        // Unreachable for what was written a moment ago — live, local and no
        // reblog, so the renderer has nothing to drop it for — and a 500
        // rather than a panic if it ever happens.
        .ok_or_else(|| AppError::Internal(format!("the status {} did not render", row.id)))?;
    after_status_created(state, &entity);
    Ok(entity)
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
/// serialize on that insert. The winner commits its own status; the loser
/// finds the pair taken, rolls its write back without a trace and answers
/// the winner's entity. The third arm — the pair is taken but its status has
/// been deleted since — moves the key onto the fresh status
/// ([`idempotency_key::reassign`]), because a dead post is not a
/// replay-able answer.
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
    let checked = check_body(
        &state.config,
        &user.default_scope,
        user.sensitive_default.get(),
        &params,
    )?;
    let entities = render_status_fields(state, checked.text, checked.content_type).await?;
    let mut tx = state.db.begin().await?;
    let row = status::insert_conn(
        &mut tx,
        &state.config.base_url,
        &new_status(user, app, &checked, &entities),
    )
    .await?;
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
    render_status(state, Some(user), row)
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
/// Nothing touches `statuses`, `status_mentions` or the hashtag tables, no
/// counter moves, no key is claimed and `after_status_created` never
/// fires. The one read the preview does perform is the mention lookup —
/// [`render_status_fields`]'s own, network fetch of an unknown remote
/// account included, since the rendered content is only honest if the
/// mention links are.
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
            favourited: false,
            reblogged: JsonBool(false),
            bookmarked: false,
            bookmark_folder: None,
            pinned_at: None,
            muted: JsonBool(false),
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

/// The [`NewStatus`] of one checked body: a `local` post by `user` with its
/// rendered content and source text, its warning and flags, the two entity
/// lists [`status::insert`] links in its transaction, and the client `app`
/// recorded as the post's application — Pleroma's `put_application`, which
/// is what makes the entity's `application` block come back.
fn new_status(
    user: &User,
    app: Option<&OauthApp>,
    checked: &Checked<'_>,
    entities: &RenderedFields,
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
        local: true,
        application_name: app.map(|found| found.client_name.clone()),
        application_website: app.and_then(|found| found.website.clone()),
        ..NewStatus::default()
    }
}

/// The post-write hook of [`create_status`]: the tickets that let a fresh
/// local status out of the database it was just written to — the streaming
/// `update` broadcast to the author's and the mentioned users' sockets and
/// to the public and hashtag timelines, the notifications for the mentions
/// and the author's followers, and the federation of the `Create` activity
/// through the job queue (see [`crate::models::job`], decision 11) — wire
/// themselves in here, and only here.
///
/// Intentionally empty until then, like the reports'
/// [`after_report_created`](crate::api::reports) hook: the write path owns
/// the row and the render, and the entity it hands over is the one all three
/// of those consumers are built from.
const fn after_status_created(_state: &AppState, _status: &Status) {}

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
        // An edit is never a preview and never replays: the create's two
        // decision parameters have no part in this merge. An edit never
        // moves the expiry either — `expires_in` is a creation parameter,
        // the stored `expires_at` rides along untouched (Pleroma's
        // `make_update_data` carries it forward the same way). Neither is
        // a poll: a post's question is fixed when it is asked — and the
        // edit's own `media_ids` are resolved by [`edit_status`] against
        // the stored attachments, never through this create-shaped body.
        preview: None,
        expires_in: None,
        poll: None,
        media_ids: None,
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
    render_status(state, Some(user), &updated).await
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
        models::user::{NewUser, insert as insert_user, local_ap_id},
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
        assert_eq!(fetched.1.nickname, "alice@127.0.0.1", "mirror handle");

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
                     "url": fetched.1.ap_id, "acct": "alice@127.0.0.1" }]),
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
    /// both callers answer it with the same id.
    #[tokio::test]
    async fn two_creates_racing_under_one_key_leave_one_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "racer").await;
        let fixture = Arc::new((state(pool), alice));

        let (first, second) = tokio::join!(
            create_status_idempotent(&fixture.0, &fixture.1, None, posted("race one"), "k1"),
            create_status_idempotent(&fixture.0, &fixture.1, None, posted("race two"), "k1"),
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
    /// the persistence ticket will store: options in order, the submitted
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
    /// a poll is the field creation ignores.
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
        let media_only = params(json!({"status": "hi", "media_ids": ["abc"]}));
        let checked = accepted(&media_only);
        assert!(
            checked.poll.is_none(),
            "media_ids without a poll is the ignored field"
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
}
