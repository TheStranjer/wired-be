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
//! [`status_length`](crate::util::format::status_length), which charges each bare URL `URL_WEIGHT` characters the
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
//! with the poster as owner through
//! [`conversation::get_participation`](crate::models::conversation::get_participation)
//! (a malformed, unknown or foreign id is the shared 404), forces the post
//! to `direct`, merges the participation's recipients into its mentions
//! and names its `direct_conversation_id`, so the post answers into that
//! DM thread; the create's `after_status_created` hook then records every
//! local participant's side of the conversation through
//! [`conversation::participate_for_status`](crate::models::conversation::participate_for_status).
//!
//! [`ApiBody`]: crate::extract::ApiBody
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404
//! [`Config::max_media_attachments`]: crate::config::Config::max_media_attachments
//! [`AppError::Unprocessable`]: crate::error::AppError::Unprocessable
//! [`AppError::NotFound`]: crate::error::AppError::NotFound
//! [`Visibility`]: crate::models::status::Visibility
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
//! comes from [`status::insert`](crate::models::status::insert) itself.
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

pub mod after_create;
pub mod build;
pub mod create;
pub mod create_media;
pub mod create_poll;
pub mod create_render;
pub mod edit;
pub mod idempotency;
pub mod params;
pub mod preview;
pub mod reply;
pub mod reply_thread;
#[cfg(test)]
mod test_support;
pub mod validate;
pub mod validate_media;
pub mod validate_poll;
pub mod validate_text;

pub(crate) use after_create::after_status_created;
pub(crate) use build::prepared_new_status;
pub use build::render_status_fields;
pub use create::{RenderedFields, create_status};
pub use edit::{EditStatusParams, edit_status};
pub use idempotency::create_status_idempotent;
pub use params::{CheckedPoll, CreateStatusParams, PollRequest};
pub use preview::preview_status;
pub use validate::{Checked, check_body};

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
/// [`Config::max_media_attachments`](crate::config::Config::max_media_attachments)
/// (Pleroma's `validate_attachments_count`
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
